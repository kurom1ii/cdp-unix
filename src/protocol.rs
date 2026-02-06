// =============================================================================
// protocol.rs — Message types + length-prefixed framing over Unix socket
// =============================================================================
//
// Wire format:  [4 bytes: payload length (u32 big-endian)] [JSON payload]
//
// Tất cả message giữa client ↔ daemon đều đi qua format này.
// CDP pipe (daemon ↔ Chrome) dùng null-byte delimiter — xử lý ở browser.rs.
// =============================================================================

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── Client → Daemon ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    /// Forward a raw CDP command to Chrome
    #[serde(rename = "cdp")]
    Cdp {
        id: u64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },

    /// Acquire a pre-warmed page from the pool
    #[serde(rename = "acquire")]
    Acquire { id: u64 },

    /// Release a page (close target or return to pool)
    #[serde(rename = "release")]
    Release { id: u64, target_id: String },

    /// Request daemon to pre-warm N pages
    #[serde(rename = "warmup")]
    Warmup {
        id: u64,
        #[serde(default = "default_warmup_count")]
        count: usize,
        #[serde(default = "default_warmup_url")]
        url: String,
    },

    /// Query daemon status
    #[serde(rename = "status")]
    Status { id: u64 },
}

fn default_warmup_count() -> usize {
    4
}
fn default_warmup_url() -> String {
    "about:blank".into()
}

// ── Daemon → Client ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMsg {
    /// CDP response forwarded from Chrome
    #[serde(rename = "cdp_response")]
    CdpResponse {
        id: u64,
        payload: serde_json::Value, // contains "result" or "error"
    },

    /// CDP event forwarded from Chrome
    #[serde(rename = "cdp_event")]
    CdpEvent {
        method: String,
        #[serde(default)]
        params: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },

    /// A pre-warmed page has been acquired
    #[serde(rename = "acquired")]
    Acquired {
        id: u64,
        target_id: String,
        session_id: String,
    },

    /// Page released
    #[serde(rename = "released")]
    Released { id: u64 },

    /// Warmup completed
    #[serde(rename = "warmup_done")]
    WarmupDone { id: u64, count: usize },

    /// Daemon status
    #[serde(rename = "status")]
    StatusResponse {
        id: u64,
        pool_available: usize,
        active_sessions: usize,
        chrome_pid: u32,
        uptime_secs: u64,
    },

    /// Error
    #[serde(rename = "error")]
    Error { id: u64, message: String },
}

// ── Frame I/O (length-prefixed) ──────────────────────────────────────────────

const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MiB — generous for screenshots etc.

/// Write a length-prefixed frame
pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read a length-prefixed frame.  Returns `None` on clean EOF.
pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {len} bytes (max {MAX_FRAME_SIZE})");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Serialize + write a DaemonMsg frame
pub async fn send_daemon_msg<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &DaemonMsg) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    write_frame(w, &json).await
}
