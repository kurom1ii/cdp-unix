// =============================================================================
// socket.rs — Abstract Unix socket helpers (Linux)
// =============================================================================
//
// Abstract Unix socket = socket nằm trong kernel namespace.
//   - Không tạo file .sock trên filesystem
//   - Tự động cleanup khi process chết
//   - Nhanh hơn file-based socket (bỏ qua VFS layer)
//   - Tên bắt đầu bằng null byte \0 trong sun_path
//
// Yêu cầu: Linux, Rust >= 1.70
// =============================================================================

use anyhow::{Context, Result};
use std::os::unix::net::{
    SocketAddr,
    UnixListener as StdUnixListener,
    UnixStream as StdUnixStream,
};

#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;

/// Tên mặc định cho abstract socket
pub const DEFAULT_SOCKET_NAME: &str = "cdp-unix";

/// Tạo abstract socket address từ tên.
/// Tên sẽ nằm trong kernel namespace, không tạo file.
#[cfg(target_os = "linux")]
pub fn abstract_addr(name: &str) -> Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
        .context(format!("failed to create abstract socket addr: {name}"))
}

/// Bind + listen trên abstract socket. Trả về tokio UnixListener.
#[cfg(target_os = "linux")]
pub fn listen(name: &str) -> Result<tokio::net::UnixListener> {
    let addr = abstract_addr(name)?;
    let std_listener = StdUnixListener::bind_addr(&addr)
        .context(format!("failed to bind abstract socket @{name}"))?;
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(std_listener)?;
    Ok(listener)
}

/// Connect tới abstract socket. Trả về tokio UnixStream.
#[cfg(target_os = "linux")]
pub fn connect(name: &str) -> Result<tokio::net::UnixStream> {
    let addr = abstract_addr(name)?;
    let std_stream = StdUnixStream::connect_addr(&addr)
        .context(format!("failed to connect to abstract socket @{name}"))?;
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(std_stream)?;
    Ok(stream)
}
