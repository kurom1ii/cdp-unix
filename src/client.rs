// =============================================================================
// client.rs — Client library: kết nối daemon qua abstract Unix socket
// =============================================================================
//
// Usage:
//   let mut client = CdpClient::connect("cdp-unix").await?;
//   let (target_id, session_id) = client.acquire_page().await?;
//   let result = client.cdp("Page.navigate", json!({"url": "..."}), Some(&session_id)).await?;
//   client.release_page(&target_id).await?;
//
// Không cần file .sock — kết nối trực tiếp qua kernel namespace.
// =============================================================================

use crate::protocol::{self, ClientMsg, DaemonMsg};
use crate::socket;
use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct CdpClient {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    msg_rx: mpsc::UnboundedReceiver<DaemonMsg>,
    next_id: u64,
}

impl CdpClient {
    /// Kết nối tới daemon qua abstract Unix socket.
    /// `name` = tên socket (mặc định "cdp-unix" → @cdp-unix trong kernel).
    pub async fn connect(name: &str) -> Result<Self> {
        let stream = socket::connect(name)?;
        let (read_half, mut write_half) = stream.into_split();

        // Writer task
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = write_rx.recv().await {
                if protocol::write_frame(&mut write_half, &data).await.is_err() {
                    break;
                }
            }
        });

        // Reader task
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMsg>();
        tokio::spawn(async move {
            let mut read_half = read_half;
            loop {
                match protocol::read_frame(&mut read_half).await {
                    Ok(Some(data)) => match serde_json::from_slice::<DaemonMsg>(&data) {
                        Ok(msg) => {
                            if msg_tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(e) => warn!("bad daemon message: {e}"),
                    },
                    Ok(None) => break,
                    Err(e) => { debug!("read error: {e}"); break; }
                }
            }
        });

        Ok(CdpClient { write_tx, msg_rx, next_id: 1 })
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&self, msg: &ClientMsg) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        self.write_tx.send(json).map_err(|_| anyhow::anyhow!("daemon connection closed"))?;
        Ok(())
    }

    async fn recv_response(&mut self, expected_id: u64) -> Result<DaemonMsg> {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(30), self.msg_rx.recv()).await {
                Ok(Some(msg)) => {
                    let msg_id = match &msg {
                        DaemonMsg::CdpResponse { id, .. } => Some(*id),
                        DaemonMsg::Acquired { id, .. } => Some(*id),
                        DaemonMsg::Released { id, .. } => Some(*id),
                        DaemonMsg::WarmupDone { id, .. } => Some(*id),
                        DaemonMsg::StatusResponse { id, .. } => Some(*id),
                        DaemonMsg::Error { id, .. } => Some(*id),
                        DaemonMsg::CdpEvent { .. } => None,
                    };
                    if msg_id == Some(expected_id) {
                        return Ok(msg);
                    }
                }
                Ok(None) => anyhow::bail!("daemon connection closed"),
                Err(_) => anyhow::bail!("response timeout (30s)"),
            }
        }
    }

    // ── High-level API ───────────────────────────────────────────────────

    /// Lấy 1 page từ pool (đã warmup). Trả về (target_id, session_id).
    pub async fn acquire_page(&mut self) -> Result<(String, String)> {
        let id = self.alloc_id();
        self.send(&ClientMsg::Acquire { id }).await?;
        match self.recv_response(id).await? {
            DaemonMsg::Acquired { target_id, session_id, .. } => Ok((target_id, session_id)),
            DaemonMsg::Error { message, .. } => anyhow::bail!("acquire failed: {message}"),
            other => anyhow::bail!("unexpected: {other:?}"),
        }
    }

    /// Trả page về (close target + context)
    pub async fn release_page(&mut self, target_id: &str) -> Result<()> {
        let id = self.alloc_id();
        self.send(&ClientMsg::Release { id, target_id: target_id.to_string() }).await?;
        self.recv_response(id).await?;
        Ok(())
    }

    /// Gửi CDP command, chờ response
    pub async fn cdp(
        &mut self,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<serde_json::Value> {
        let id = self.alloc_id();
        self.send(&ClientMsg::Cdp {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.map(String::from),
        }).await?;

        match self.recv_response(id).await? {
            DaemonMsg::CdpResponse { payload, .. } => {
                if let Some(err) = payload.get("error") {
                    anyhow::bail!("CDP error: {err}");
                }
                Ok(payload.get("result").cloned().unwrap_or_default())
            }
            DaemonMsg::Error { message, .. } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected: {other:?}"),
        }
    }

    /// Yêu cầu daemon warmup thêm N pages
    pub async fn warmup(&mut self, count: usize, url: &str) -> Result<usize> {
        let id = self.alloc_id();
        self.send(&ClientMsg::Warmup { id, count, url: url.to_string() }).await?;
        match self.recv_response(id).await? {
            DaemonMsg::WarmupDone { count, .. } => Ok(count),
            DaemonMsg::Error { message, .. } => anyhow::bail!("{message}"),
            other => anyhow::bail!("unexpected: {other:?}"),
        }
    }

    /// Hỏi trạng thái daemon
    pub async fn status(&mut self) -> Result<DaemonMsg> {
        let id = self.alloc_id();
        self.send(&ClientMsg::Status { id }).await?;
        self.recv_response(id).await
    }
}
