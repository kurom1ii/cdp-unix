# cdp-unix — Hướng dẫn sử dụng

## cdp-unix là gì?

Browser controller tốc độ cao viết bằng Rust, giao tiếp với Chrome qua CDP
(Chrome DevTools Protocol).

**Điểm khác biệt so với các tool khác:**

| Đặc điểm | Puppeteer/Playwright | cdp-unix |
|-----------|---------------------|----------|
| Giao tiếp với Chrome | WebSocket (TCP) | Pipe trực tiếp (fd 3/4) |
| Giao tiếp client↔daemon | Không có | Abstract Unix socket (kernel) |
| Cold start | Mỗi lần khởi động Chrome | Chrome giữ sẵn, warmup pool |
| File socket | — | Không tạo file `.sock` |
| Overhead | HTTP upgrade + WS frame | Zero — raw pipe + len-prefix |

## Kiến trúc

```
┌─────────┐  pipe fd3/fd4   ┌──────────────────────┐  @cdp-unix   ┌─────────┐
│ Chrome   │◀───────────────▶│       Daemon          │◀────────────▶│ Client  │
│ headless │  (null-byte     │                       │  abstract    │ (bạn)   │
│          │   delimited)    │  ┌─────────────────┐  │  Unix sock   │         │
└─────────┘                  │  │  Warmup Pool    │  │  (kernel)    └─────────┘
                             │  │  N pages sẵn    │  │
                             │  └─────────────────┘  │
                             └──────────────────────┘
```

**3 lớp giao tiếp, không lớp nào dùng network:**
1. **Chrome ↔ Daemon**: pipe (fd 3 đọc, fd 4 ghi), delimiter `\0`
2. **Daemon ↔ Client**: abstract Unix socket, frame `[4 byte length][JSON]`
3. **Không có file** nào tạo trên disk — tất cả trong kernel

## Cài đặt

### Yêu cầu
- Linux (abstract socket là tính năng của Linux kernel)
- Rust >= 1.70
- Chrome hoặc Chromium đã cài

### Build

```bash
cd cdp-unix
cargo build --release
```

Binary nằm ở `target/release/cdp-unix` (~3.7MB).

## Cách sử dụng

### 1. Khởi động daemon

```bash
# Mặc định: 4 pages warmup, socket tên "cdp-unix"
cdp-unix daemon

# Nhiều warmup hơn cho production
cdp-unix daemon --warmup 20

# Dùng tên socket khác (nhiều daemon cùng lúc)
cdp-unix daemon --name my-scraper --warmup 10

# Chỉ định Chrome path
cdp-unix daemon --chrome-path /usr/bin/chromium

# Thêm tham số Chrome
cdp-unix daemon --chrome-arg "--proxy-server=socks5://127.0.0.1:1080"
```

Daemon sẽ:
1. Khởi động Chrome headless với `--remote-debugging-pipe`
2. Mở abstract socket `@cdp-unix` trong kernel
3. Tạo N pages warmup (mỗi page = 1 BrowserContext riêng, isolated)
4. Chờ client kết nối

**Chrome chỉ khởi động 1 lần. Client connect/disconnect thoải mái.**

### 2. Kiểm tra trạng thái

```bash
cdp-unix status

# Output:
# ┌──────────────────────────────┐
# │  cdp-unix @ cdp-unix         │
# ├──────────────────────────────┤
# │  Chrome PID:   12345         │
# │  Uptime:       42s           │
# │  Pool:         4             │  ← pages sẵn sàng
# │  Active:       0             │  ← pages đang dùng
# └──────────────────────────────┘
```

### 3. Warmup thêm pages

```bash
# Warmup 10 pages nữa
cdp-unix warmup --count 10

# Warmup với URL cụ thể (pre-load trang)
cdp-unix warmup --count 5 --url "https://example.com"
```

### 4. Navigate tới URL

```bash
cdp-unix navigate https://example.com

# Output:
# acquire: 2ms          ← lấy page từ pool
# title: Example Domain
# total: 1523ms          ← bao gồm cả tải trang
```

### 5. Chạy CDP command tùy ý

```bash
# Evaluate JavaScript
cdp-unix exec --method "Runtime.evaluate" \
  --params '{"expression": "navigator.userAgent"}'

# Chụp screenshot (trả về base64)
cdp-unix exec --method "Page.captureScreenshot" \
  --params '{"format": "png"}'
```

### 6. Benchmark

```bash
cdp-unix bench --count 100

# Output:
# ┌───────────────────────────────┐
# │  100 cycles
# ├───────────────────────────────┤
# │  Total:      1.2s
# │  Avg:       12.3ms
# │  P50:       10.1ms
# │  P99:       25.4ms
# │  RPS:       82.3
# └───────────────────────────────┘
```

## Sử dụng từ code Rust

Thêm vào `Cargo.toml`:

```toml
[dependencies]
cdp-unix = { path = "../cdp-unix" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

```rust
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Kết nối daemon (đã chạy sẵn)
    let mut client = cdp_unix::client::CdpClient::connect("cdp-unix").await?;

    // Lấy page từ pool — nhanh vì đã warmup
    let (target_id, session_id) = client.acquire_page().await?;

    // Navigate
    client.cdp(
        "Page.navigate",
        json!({"url": "https://example.com"}),
        Some(&session_id),
    ).await?;

    // Đợi tải xong
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Lấy title
    let result = client.cdp(
        "Runtime.evaluate",
        json!({"expression": "document.title"}),
        Some(&session_id),
    ).await?;
    println!("Title: {}", result["result"]["value"]);

    // Chụp screenshot
    let screenshot = client.cdp(
        "Page.captureScreenshot",
        json!({"format": "png"}),
        Some(&session_id),
    ).await?;
    let base64_data = screenshot["data"].as_str().unwrap();
    println!("Screenshot size: {} bytes", base64_data.len());

    // Trả page về
    client.release_page(&target_id).await?;

    Ok(())
}
```

## Chạy tests

```bash
# Unit tests (không cần Chrome)
cargo test

# E2E tests (cần Chrome đã cài)
cargo test -- --ignored

# Chạy tất cả + hiện output
cargo test -- --nocapture

# Chỉ chạy 1 test cụ thể
cargo test test_e2e_daemon_full_cycle -- --ignored --nocapture
```

## Nhiều daemon cùng lúc

Mỗi daemon dùng tên socket khác nhau:

```bash
# Terminal 1: scraper
cdp-unix daemon --name scraper --warmup 20

# Terminal 2: renderer
cdp-unix daemon --name renderer --warmup 5

# Client kết nối đúng daemon
cdp-unix --name scraper status
cdp-unix --name renderer navigate https://example.com
```

## So sánh tốc độ

| Thao tác | cdp-unix | Puppeteer |
|----------|----------|-----------|
| Cold start Chrome | ~0ms (đã chạy sẵn) | ~500-2000ms |
| Acquire page (từ pool) | ~2-5ms | N/A |
| Acquire page (tạo mới) | ~50-100ms | ~100-300ms |
| Gửi CDP command | ~0.1ms (pipe) | ~1-5ms (WebSocket) |

## Xử lý lỗi thường gặp

```
"Chrome/Chromium not found"
→ Cài Chrome: sudo apt install google-chrome-stable
→ Hoặc chỉ định path: cdp-unix daemon --chrome-path /path/to/chrome

"failed to bind abstract socket"
→ Daemon khác đang chạy cùng tên. Dùng --name khác hoặc kill daemon cũ.

"daemon connection closed"
→ Daemon chưa chạy. Khởi động daemon trước.

"response timeout (30s)"
→ Chrome treo hoặc quá tải. Restart daemon.
```

## Cấu trúc source code

```
src/
├── main.rs        # CLI (clap) — daemon/status/warmup/navigate/bench
├── browser.rs     # Spawn Chrome, pipe reader/writer threads
├── daemon.rs      # Abstract socket server, router actor
├── pool.rs        # Page pool, warmup CDP operations
├── protocol.rs    # Message types (ClientMsg/DaemonMsg), frame I/O
├── client.rs      # Client library (connect, acquire, cdp, release)
└── socket.rs      # Abstract Unix socket helpers (Linux kernel namespace)
```
