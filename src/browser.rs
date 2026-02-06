// =============================================================================
// browser.rs — Chrome process management + CDP pipe I/O
// =============================================================================
//
//   ┌──────────┐  pipe (fd3)    ┌──────────┐
//   │  Daemon   │ ─────────────▶│  Chrome   │
//   │  cmd_tx   │               │           │
//   │           │ ◀─────────────│           │
//   │  msg_rx   │  pipe (fd4)   └──────────┘
//   └──────────┘
//
// Chrome nhận CDP JSON qua fd 3, trả response/event qua fd 4.
// Delimiter = null byte (\0). Không WebSocket. Zero network overhead.
// =============================================================================

use anyhow::{Context, Result};
use os_pipe::{PipeReader, PipeWriter};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::IntoRawFd;
use std::process::{Child, Command, Stdio};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ── Types ────────────────────────────────────────────────────────────────────

/// Message parsed from Chrome's pipe output
#[derive(Debug)]
pub enum ChromeMsg {
    Response { id: u64, payload: serde_json::Value },
    Event { method: String, params: serde_json::Value, session_id: Option<String> },
}

/// Chrome process handle (for lifecycle management)
pub struct BrowserProcess {
    child: Child,
    pub pid: u32,
}

/// I/O channels to talk to Chrome
pub struct BrowserIO {
    /// Send raw JSON bytes to Chrome's pipe (thread-safe, cloneable)
    pub cmd_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Receive parsed messages from Chrome
    pub msg_rx: mpsc::UnboundedReceiver<ChromeMsg>,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl BrowserProcess {
    /// Spawn Chrome with `--remote-debugging-pipe`.
    /// Returns process handle + I/O channels (separated for flexible ownership).
    pub fn spawn(
        chrome_path: Option<&str>,
        extra_args: &[String],
    ) -> Result<(BrowserProcess, BrowserIO)> {
        let chrome = find_chrome(chrome_path)?;
        info!(path = %chrome, "spawning Chrome (pipe mode)");

        // ── Create 2 pipe pairs ─────────────────────────────────────────
        // cmd_pipe: we write → Chrome reads on fd 3
        // rsp_pipe: Chrome writes on fd 4 → we read
        let (cmd_reader, cmd_writer) = os_pipe::pipe()?;
        let (rsp_reader, rsp_writer) = os_pipe::pipe()?;

        // Grab raw fds for use in pre_exec (before they get consumed)
        let cmd_reader_fd = cmd_reader.into_raw_fd();
        let rsp_writer_fd = rsp_writer.into_raw_fd();

        // ── Build Chrome command ────────────────────────────────────────
        let mut cmd = Command::new(&chrome);
        cmd.args([
            "--remote-debugging-pipe",
            "--disable-gpu",
            "--disable-software-rasterizer",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--disable-translate",
            "--mute-audio",
            "--hide-scrollbars",
            "--disable-dev-shm-usage",
            "--disable-setuid-sandbox",
            "--no-sandbox",
        ]);
        cmd.args(extra_args);
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        // Setup fd 3 and fd 4 in child process
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::dup2(cmd_reader_fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(rsp_writer_fd, 4) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close originals (now dup'd to 3 and 4)
                libc::close(cmd_reader_fd);
                libc::close(rsp_writer_fd);
                Ok(())
            });
        }

        let mut child = cmd.spawn().context("failed to spawn Chrome")?;
        let pid = child.id();
        info!(pid, "Chrome started");

        // ── Log Chrome stderr in background ─────────────────────────────
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("chrome-stderr".into())
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().flatten() {
                        if !line.is_empty() {
                            debug!(target: "chrome", "{}", line);
                        }
                    }
                })?;
        }

        // ── Pipe reader thread: Chrome fd4 → channel ────────────────────
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("cdp-pipe-reader".into())
            .spawn(move || pipe_reader_loop(rsp_reader, msg_tx))?;

        // ── Pipe writer thread: channel → Chrome fd3 ────────────────────
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("cdp-pipe-writer".into())
            .spawn(move || pipe_writer_loop(cmd_writer, cmd_rx))?;

        Ok((
            BrowserProcess { child, pid },
            BrowserIO { cmd_tx, msg_rx },
        ))
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        info!(pid = self.pid, "shutting down Chrome");
        self.kill();
    }
}

// ── Pipe I/O loops (blocking threads) ────────────────────────────────────────

fn pipe_reader_loop(pipe: PipeReader, tx: mpsc::UnboundedSender<ChromeMsg>) {
    let mut reader = BufReader::with_capacity(256 * 1024, pipe);
    let mut buf = Vec::with_capacity(64 * 1024);

    loop {
        buf.clear();
        match reader.read_until(0, &mut buf) {
            Ok(0) => {
                info!("Chrome pipe EOF");
                break;
            }
            Ok(_) => {
                if buf.last() == Some(&0) {
                    buf.pop();
                }
                if buf.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<serde_json::Value>(&buf) {
                    Ok(val) => {
                        if tx.send(parse_chrome_msg(val)).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("malformed CDP JSON: {e}"),
                }
            }
            Err(e) => {
                error!("pipe read error: {e}");
                break;
            }
        }
    }
}

fn pipe_writer_loop(mut pipe: PipeWriter, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(msg) = rx.blocking_recv() {
        if pipe.write_all(&msg).is_err() {
            break;
        }
        if pipe.write_all(b"\0").is_err() {
            break;
        }
        let _ = pipe.flush();
    }
}

fn parse_chrome_msg(val: serde_json::Value) -> ChromeMsg {
    if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
        ChromeMsg::Response { id, payload: val }
    } else {
        ChromeMsg::Event {
            method: val
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("__unknown__")
                .to_string(),
            params: val.get("params").cloned().unwrap_or_default(),
            session_id: val
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from),
        }
    }
}

// ── Find Chrome binary ───────────────────────────────────────────────────────

fn find_chrome(explicit: Option<&str>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p.to_string());
    }

    let candidates = [
        "google-chrome-stable",
    ];

    for c in &candidates {
        if let Ok(output) = Command::new("which").arg(c).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(path);
                }
            }
        }
    }

    anyhow::bail!(
        "Chrome/Chromium not found. Set --chrome-path or install Chrome.\nSearched: {candidates:?}"
    )
}
