// =============================================================================
// pool.rs — Pre-warmed page pool
// =============================================================================
//
// Warmup tạo sẵn N page (mỗi page = 1 BrowserContext + 1 Target).
// Khi client acquire → lấy page từ pool → attach → trả session_id.
// Client dùng xong → release → close target + context.
// Pool tự refill nếu cần.
//
// Vì dùng BrowserContext riêng, mỗi page hoàn toàn isolated (cookies, storage).
// =============================================================================

use anyhow::Result;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

// ── Types ────────────────────────────────────────────────────────────────────

/// A pre-warmed page sitting in the pool
#[derive(Debug, Clone)]
pub struct WarmPage {
    pub browser_context_id: String,
    pub target_id: String,
}

/// Page pool
pub struct PagePool {
    pages: VecDeque<WarmPage>,
    pub active_count: usize,
}

impl PagePool {
    pub fn new() -> Self {
        Self {
            pages: VecDeque::new(),
            active_count: 0,
        }
    }

    pub fn available(&self) -> usize {
        self.pages.len()
    }

    pub fn push(&mut self, page: WarmPage) {
        self.pages.push_back(page);
    }

    pub fn acquire(&mut self) -> Option<WarmPage> {
        let page = self.pages.pop_front()?;
        self.active_count += 1;
        Some(page)
    }

    pub fn released(&mut self) {
        self.active_count = self.active_count.saturating_sub(1);
    }
}

// ── CDP helper: send command to Chrome via pipe ──────────────────────────────

/// Gửi 1 CDP command qua Chrome pipe và chờ response.
/// `chrome_tx` là channel ghi raw JSON bytes vào pipe writer thread.
/// `id_counter` dùng để tạo unique id.
/// `pending_tx` để register pending response handler.
pub async fn cdp_call(
    chrome_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending_tx: &mpsc::UnboundedSender<PendingInternal>,
    method: &str,
    params: serde_json::Value,
    session_id: Option<&str>,
    id_counter: &AtomicU64,
) -> Result<serde_json::Value> {
    let id = id_counter.fetch_add(1, Ordering::Relaxed);

    let mut cmd = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    if let Some(sid) = session_id {
        cmd["sessionId"] = json!(sid);
    }

    let (resp_tx, resp_rx) = oneshot::channel();
    pending_tx
        .send(PendingInternal { chrome_id: id, resp_tx })
        .map_err(|_| anyhow::anyhow!("pending channel closed"))?;

    let bytes = serde_json::to_vec(&cmd)?;
    chrome_tx
        .send(bytes)
        .map_err(|_| anyhow::anyhow!("chrome pipe channel closed"))?;

    match tokio::time::timeout(std::time::Duration::from_secs(30), resp_rx).await {
        Ok(Ok(val)) => {
            if let Some(err) = val.get("error") {
                anyhow::bail!("CDP error: {err}");
            }
            Ok(val.get("result").cloned().unwrap_or_default())
        }
        Ok(Err(_)) => anyhow::bail!("response channel dropped"),
        Err(_) => anyhow::bail!("CDP call '{method}' timed out (30s)"),
    }
}

/// Register a pending internal CDP response
pub struct PendingInternal {
    pub chrome_id: u64,
    pub resp_tx: oneshot::Sender<serde_json::Value>,
}

// ── Warmup operations ────────────────────────────────────────────────────────

/// Create a single pre-warmed page
pub async fn create_warm_page(
    chrome_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending_tx: &mpsc::UnboundedSender<PendingInternal>,
    id_counter: &AtomicU64,
    url: &str,
) -> Result<WarmPage> {
    // 1. Create isolated browser context
    let ctx = cdp_call(
        chrome_tx,
        pending_tx,
        "Target.createBrowserContext",
        json!({"disposeOnDetach": false}),
        None,
        id_counter,
    )
    .await?;

    let browser_context_id = ctx["browserContextId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing browserContextId"))?
        .to_string();

    // 2. Create page target in that context
    let target = cdp_call(
        chrome_tx,
        pending_tx,
        "Target.createTarget",
        json!({
            "url": url,
            "browserContextId": &browser_context_id,
        }),
        None,
        id_counter,
    )
    .await?;

    let target_id = target["targetId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing targetId"))?
        .to_string();

    debug!(target_id = %target_id, ctx = %browser_context_id, "warm page created");

    Ok(WarmPage {
        browser_context_id,
        target_id,
    })
}

/// Attach to target → get flat sessionId
pub async fn attach_target(
    chrome_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending_tx: &mpsc::UnboundedSender<PendingInternal>,
    id_counter: &AtomicU64,
    target_id: &str,
) -> Result<String> {
    let result = cdp_call(
        chrome_tx,
        pending_tx,
        "Target.attachToTarget",
        json!({"targetId": target_id, "flatten": true}),
        None,
        id_counter,
    )
    .await?;

    result["sessionId"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("missing sessionId"))
}

/// Close a target and dispose its browser context
pub async fn close_page(
    chrome_tx: &mpsc::UnboundedSender<Vec<u8>>,
    pending_tx: &mpsc::UnboundedSender<PendingInternal>,
    id_counter: &AtomicU64,
    target_id: &str,
    browser_context_id: &str,
) {
    let _ = cdp_call(
        chrome_tx,
        pending_tx,
        "Target.closeTarget",
        json!({"targetId": target_id}),
        None,
        id_counter,
    )
    .await;

    if !browser_context_id.is_empty() {
        let _ = cdp_call(
            chrome_tx,
            pending_tx,
            "Target.disposeBrowserContext",
            json!({"browserContextId": browser_context_id}),
            None,
            id_counter,
        )
        .await;
    }
}
