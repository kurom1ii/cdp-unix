// =============================================================================
// daemon.rs — Unix socket server + CDP message router (actor pattern)
// =============================================================================
//
//   Client₁ ──┐                         ┌── Chrome pipe reader thread
//   Client₂ ──┼── Abstract Socket ──▶ ROUTER ◀── (msg_rx)
//   Client₃ ──┘  @cdp-unix (kernel)  │    │
//                                     │    └── Chrome pipe writer thread
//                                     │        (cmd_tx)
//                                     ├── Page Pool
//                                     └── Pending map (chrome_id → who asked)
//
// Abstract socket = kernel namespace, không tạo file, tự cleanup.
// Router = single tokio::select! loop. Không lock. Không race.
// =============================================================================

use crate::browser::{BrowserProcess, ChromeMsg};
use crate::pool::{
    attach_target, close_page, create_warm_page, PagePool, PendingInternal, WarmPage,
};
use crate::protocol::{self, ClientMsg, DaemonMsg};
use crate::socket;
use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ── Config ───────────────────────────────────────────────────────────────────

pub struct DaemonConfig {
    /// Tên abstract socket (vd: "cdp-unix" → @cdp-unix trong kernel)
    pub socket_name: String,
    pub chrome_path: Option<String>,
    pub chrome_args: Vec<String>,
    pub warmup_count: usize,
    pub warmup_url: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_name: socket::DEFAULT_SOCKET_NAME.to_string(),
            chrome_path: None,
            chrome_args: vec![],
            warmup_count: 4,
            warmup_url: "about:blank".into(),
        }
    }
}

// ── Internal messages to the router ──────────────────────────────────────────

enum RouterEvent {
    FromClient { client_id: u64, msg: ClientMsg },
    ClientConnected {
        client_id: u64,
        tx: mpsc::UnboundedSender<DaemonMsg>,
    },
    ClientDisconnected { client_id: u64 },
    WarmPageReady(WarmPage),
}

enum PendingOwner {
    Client { client_id: u64, original_id: u64 },
    Internal {
        resp_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
    },
}

struct ClientState {
    tx: mpsc::UnboundedSender<DaemonMsg>,
    sessions: HashMap<String, (String, String)>,
}

// ── Main daemon loop ─────────────────────────────────────────────────────────

pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    // ── Spawn Chrome ────────────────────────────────────────────────────
    let (_browser, mut io) =
        BrowserProcess::spawn(config.chrome_path.as_deref(), &config.chrome_args)?;
    let chrome_pid = _browser.pid;
    let chrome_tx = io.cmd_tx.clone();

    let id_counter = Arc::new(AtomicU64::new(1));
    let (pending_int_tx, mut pending_int_rx) = mpsc::unbounded_channel::<PendingInternal>();
    let (router_tx, mut router_rx) = mpsc::unbounded_channel::<RouterEvent>();

    // ── Abstract Unix socket listener ───────────────────────────────────
    // Không tạo file — nằm trong kernel namespace
    let listener = socket::listen(&config.socket_name)?;
    info!(name = %config.socket_name, "daemon listening on abstract socket @{}", config.socket_name);

    // Accept task
    let accept_router_tx = router_tx.clone();
    tokio::spawn(async move {
        let mut next_id = 1u64;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let cid = next_id;
                    next_id += 1;
                    info!(client_id = cid, "client connected");
                    spawn_client_handler(cid, stream, accept_router_tx.clone());
                }
                Err(e) => error!("accept error: {e}"),
            }
        }
    });

    // ── Initial warmup ──────────────────────────────────────────────────
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    {
        let chrome_tx = chrome_tx.clone();
        let pending_tx = pending_int_tx.clone();
        let id_ctr = id_counter.clone();
        let router_tx = router_tx.clone();
        let count = config.warmup_count;
        let url = config.warmup_url.clone();

        tokio::spawn(async move {
            info!(count, url = %url, "warmup starting");
            for i in 0..count {
                match create_warm_page(&chrome_tx, &pending_tx, &id_ctr, &url).await {
                    Ok(page) => {
                        let _ = router_tx.send(RouterEvent::WarmPageReady(page));
                    }
                    Err(e) => warn!(i, "warmup page failed: {e}"),
                }
            }
            info!("warmup done");
        });
    }

    // ── Router loop ─────────────────────────────────────────────────────
    let start = Instant::now();
    let mut clients: HashMap<u64, ClientState> = HashMap::new();
    let mut pending: HashMap<u64, PendingOwner> = HashMap::new();
    let mut session_owner: HashMap<String, u64> = HashMap::new();
    let mut pool = PagePool::new();

    loop {
        tokio::select! {
            Some(chrome_msg) = io.msg_rx.recv() => {
                match chrome_msg {
                    ChromeMsg::Response { id, payload } => {
                        if let Some(owner) = pending.remove(&id) {
                            match owner {
                                PendingOwner::Internal { resp_tx } => {
                                    let _ = resp_tx.send(payload);
                                }
                                PendingOwner::Client { client_id, original_id } => {
                                    if let Some(cs) = clients.get(&client_id) {
                                        let _ = cs.tx.send(DaemonMsg::CdpResponse {
                                            id: original_id,
                                            payload,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    ChromeMsg::Event { method, params, session_id } => {
                        if let Some(sid) = &session_id {
                            if let Some(&cid) = session_owner.get(sid) {
                                if let Some(cs) = clients.get(&cid) {
                                    let _ = cs.tx.send(DaemonMsg::CdpEvent {
                                        method,
                                        params,
                                        session_id,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            Some(pi) = pending_int_rx.recv() => {
                pending.insert(pi.chrome_id, PendingOwner::Internal { resp_tx: pi.resp_tx });
            }

            Some(evt) = router_rx.recv() => {
                match evt {
                    RouterEvent::WarmPageReady(page) => {
                        debug!(target_id = %page.target_id, "warm page → pool");
                        pool.push(page);
                    }

                    RouterEvent::ClientConnected { client_id, tx } => {
                        clients.insert(client_id, ClientState {
                            tx,
                            sessions: HashMap::new(),
                        });
                    }

                    RouterEvent::ClientDisconnected { client_id } => {
                        if let Some(cs) = clients.remove(&client_id) {
                            for (sid, (tid, cid)) in cs.sessions {
                                session_owner.remove(&sid);
                                let tx = chrome_tx.clone();
                                let ptx = pending_int_tx.clone();
                                let idc = id_counter.clone();
                                tokio::spawn(async move {
                                    close_page(&tx, &ptx, &idc, &tid, &cid).await;
                                });
                                pool.released();
                            }
                        }
                    }

                    RouterEvent::FromClient { client_id, msg } => {
                        handle_client_msg(
                            client_id, msg, &mut clients, &mut pending,
                            &mut session_owner, &mut pool, &chrome_tx,
                            &pending_int_tx, &id_counter, chrome_pid, start,
                            &router_tx,
                        );
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }

    // _browser drops → kills Chrome. Không cần cleanup file.
    Ok(())
}

// ── Handle client message ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_client_msg(
    client_id: u64,
    msg: ClientMsg,
    clients: &mut HashMap<u64, ClientState>,
    pending: &mut HashMap<u64, PendingOwner>,
    _session_owner: &mut HashMap<String, u64>,
    pool: &mut PagePool,
    chrome_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending_int_tx: &mpsc::UnboundedSender<PendingInternal>,
    id_counter: &Arc<AtomicU64>,
    chrome_pid: u32,
    start: Instant,
    router_tx: &mpsc::UnboundedSender<RouterEvent>,
) {
    match msg {
        ClientMsg::Cdp { id, method, params, session_id } => {
            let chrome_id = id_counter.fetch_add(1, Ordering::Relaxed);
            pending.insert(chrome_id, PendingOwner::Client { client_id, original_id: id });

            let mut cmd = json!({"id": chrome_id, "method": method, "params": params});
            if let Some(sid) = &session_id {
                cmd["sessionId"] = json!(sid);
            }

            match serde_json::to_vec(&cmd) {
                Ok(bytes) => {
                    if chrome_tx.send(bytes).is_err() {
                        send_error(clients, client_id, id, "Chrome pipe closed");
                    }
                }
                Err(e) => send_error(clients, client_id, id, &format!("serialize: {e}")),
            }
        }

        ClientMsg::Acquire { id } => {
            let warm = pool.acquire();
            let tx = chrome_tx.clone();
            let ptx = pending_int_tx.clone();
            let idc = id_counter.clone();
            let client_tx = clients.get(&client_id).map(|c| c.tx.clone());

            tokio::spawn(async move {
                let page = match warm {
                    Some(p) => p,
                    None => match create_warm_page(&tx, &ptx, &idc, "about:blank").await {
                        Ok(p) => p,
                        Err(e) => {
                            if let Some(ctx) = &client_tx {
                                let _ = ctx.send(DaemonMsg::Error { id, message: format!("create page: {e}") });
                            }
                            return;
                        }
                    },
                };

                match attach_target(&tx, &ptx, &idc, &page.target_id).await {
                    Ok(session_id) => {
                        if let Some(ctx) = &client_tx {
                            let _ = ctx.send(DaemonMsg::Acquired {
                                id,
                                target_id: page.target_id.clone(),
                                session_id,
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(ctx) = &client_tx {
                            let _ = ctx.send(DaemonMsg::Error { id, message: format!("attach: {e}") });
                        }
                    }
                }
            });
        }

        ClientMsg::Release { id, target_id } => {
            pool.released();
            if let Some(cs) = clients.get_mut(&client_id) {
                let ctx_id = cs.sessions.values()
                    .find(|(tid, _)| tid == &target_id)
                    .map(|(_, cid)| cid.clone())
                    .unwrap_or_default();
                cs.sessions.retain(|_, (tid, _)| tid != &target_id);

                let tx = chrome_tx.clone();
                let ptx = pending_int_tx.clone();
                let idc = id_counter.clone();
                let client_tx = cs.tx.clone();

                tokio::spawn(async move {
                    close_page(&tx, &ptx, &idc, &target_id, &ctx_id).await;
                    let _ = client_tx.send(DaemonMsg::Released { id });
                });
            }
        }

        ClientMsg::Warmup { id, count, url } => {
            let tx = chrome_tx.clone();
            let ptx = pending_int_tx.clone();
            let idc = id_counter.clone();
            let client_tx = clients.get(&client_id).map(|c| c.tx.clone());
            let rtx = router_tx.clone();

            tokio::spawn(async move {
                let mut created = 0usize;
                for _ in 0..count {
                    match create_warm_page(&tx, &ptx, &idc, &url).await {
                        Ok(page) => {
                            let _ = rtx.send(RouterEvent::WarmPageReady(page));
                            created += 1;
                        }
                        Err(e) => warn!("warmup failed: {e}"),
                    }
                }
                if let Some(ctx) = client_tx {
                    let _ = ctx.send(DaemonMsg::WarmupDone { id, count: created });
                }
            });
        }

        ClientMsg::Status { id } => {
            if let Some(cs) = clients.get(&client_id) {
                let _ = cs.tx.send(DaemonMsg::StatusResponse {
                    id,
                    pool_available: pool.available(),
                    active_sessions: pool.active_count,
                    chrome_pid,
                    uptime_secs: start.elapsed().as_secs(),
                });
            }
        }
    }
}

fn send_error(clients: &HashMap<u64, ClientState>, client_id: u64, id: u64, msg: &str) {
    if let Some(cs) = clients.get(&client_id) {
        let _ = cs.tx.send(DaemonMsg::Error { id, message: msg.into() });
    }
}

// ── Client connection handler ────────────────────────────────────────────────

fn spawn_client_handler(
    client_id: u64,
    stream: tokio::net::UnixStream,
    router_tx: mpsc::UnboundedSender<RouterEvent>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let (client_msg_tx, mut client_msg_rx) = mpsc::unbounded_channel::<DaemonMsg>();

    let _ = router_tx.send(RouterEvent::ClientConnected {
        client_id,
        tx: client_msg_tx,
    });

    // Writer
    tokio::spawn(async move {
        while let Some(msg) = client_msg_rx.recv().await {
            if let Ok(json) = serde_json::to_vec(&msg) {
                if protocol::write_frame(&mut write_half, &json).await.is_err() {
                    break;
                }
            }
        }
    });

    // Reader
    let rtx = router_tx.clone();
    tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            match protocol::read_frame(&mut read_half).await {
                Ok(Some(data)) => match serde_json::from_slice::<ClientMsg>(&data) {
                    Ok(msg) => {
                        if rtx.send(RouterEvent::FromClient { client_id, msg }).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!(client_id, "bad message: {e}"),
                },
                Ok(None) => break,
                Err(e) => { debug!(client_id, "read error: {e}"); break; }
            }
        }
        info!(client_id, "disconnected");
        let _ = rtx.send(RouterEvent::ClientDisconnected { client_id });
    });
}
