// =============================================================================
// tests/e2e.rs — Test end-to-end cdp-unix
// =============================================================================
//
// Chạy: cargo test              (unit tests — không cần Chrome)
//        cargo test -- --ignored  (e2e tests — cần Chrome đã cài)
//
// Unit tests kiểm tra:
//   - Protocol frame encoding/decoding
//   - Abstract socket connect/listen
//   - Page pool logic
//   - Message serialization
//
// E2E tests (ignored mặc định, cần Chrome):
//   - Daemon startup + warmup
//   - Client connect + acquire + CDP command + release
//   - Benchmark latency
// =============================================================================

use std::time::Duration;

// ─── Unit test: Protocol framing ─────────────────────────────────────────────

#[tokio::test]
async fn test_frame_roundtrip() {
    // Tạo pipe (giả lập socket) để test frame read/write
    let (mut reader, mut writer) = tokio::io::duplex(64 * 1024);

    let payload = b"hello world from cdp-unix";

    // Spawn writer
    let write_handle = tokio::spawn(async move {
        cdp_unix::protocol::write_frame(&mut writer, payload).await.unwrap();
        cdp_unix::protocol::write_frame(&mut writer, b"second frame").await.unwrap();
        cdp_unix::protocol::write_frame(&mut writer, b"").await.unwrap(); // empty frame
        drop(writer); // close → reader gets EOF
    });

    // Read frames
    let frame1 = cdp_unix::protocol::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(frame1, b"hello world from cdp-unix");

    let frame2 = cdp_unix::protocol::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(frame2, b"second frame");

    let frame3 = cdp_unix::protocol::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(frame3, b""); // empty is valid

    // EOF → None
    let frame4 = cdp_unix::protocol::read_frame(&mut reader).await.unwrap();
    assert!(frame4.is_none());

    write_handle.await.unwrap();
}

#[tokio::test]
async fn test_frame_large_payload() {
    let (mut reader, mut writer) = tokio::io::duplex(1024 * 1024);

    // 1MB payload
    let payload = vec![42u8; 1_000_000];
    let payload_clone = payload.clone();

    tokio::spawn(async move {
        cdp_unix::protocol::write_frame(&mut writer, &payload_clone).await.unwrap();
    });

    let frame = cdp_unix::protocol::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(frame.len(), 1_000_000);
    assert!(frame.iter().all(|&b| b == 42));
}

// ─── Unit test: Message serialization ────────────────────────────────────────

#[test]
fn test_client_msg_serialize() {
    let msg = cdp_unix::protocol::ClientMsg::Cdp {
        id: 1,
        method: "Page.navigate".to_string(),
        params: serde_json::json!({"url": "https://example.com"}),
        session_id: Some("sess-123".to_string()),
    };

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"cdp\""));
    assert!(json.contains("Page.navigate"));
    assert!(json.contains("sess-123"));

    // Roundtrip
    let parsed: cdp_unix::protocol::ClientMsg = serde_json::from_str(&json).unwrap();
    match parsed {
        cdp_unix::protocol::ClientMsg::Cdp { id, method, session_id, .. } => {
            assert_eq!(id, 1);
            assert_eq!(method, "Page.navigate");
            assert_eq!(session_id.unwrap(), "sess-123");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn test_client_msg_acquire() {
    let msg = cdp_unix::protocol::ClientMsg::Acquire { id: 42 };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"acquire\""));
    assert!(json.contains("42"));
}

#[test]
fn test_daemon_msg_serialize() {
    let msg = cdp_unix::protocol::DaemonMsg::Acquired {
        id: 1,
        target_id: "T-123".to_string(),
        session_id: "S-456".to_string(),
    };

    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"acquired\""));
    assert!(json.contains("T-123"));
    assert!(json.contains("S-456"));
}

#[test]
fn test_daemon_msg_status() {
    let msg = cdp_unix::protocol::DaemonMsg::StatusResponse {
        id: 1,
        pool_available: 5,
        active_sessions: 3,
        chrome_pid: 12345,
        uptime_secs: 60,
    };

    let json = serde_json::to_string(&msg).unwrap();
    let parsed: cdp_unix::protocol::DaemonMsg = serde_json::from_str(&json).unwrap();
    match parsed {
        cdp_unix::protocol::DaemonMsg::StatusResponse { pool_available, active_sessions, .. } => {
            assert_eq!(pool_available, 5);
            assert_eq!(active_sessions, 3);
        }
        _ => panic!("wrong variant"),
    }
}

// ─── Unit test: Page pool ────────────────────────────────────────────────────

#[test]
fn test_pool_basic() {
    let mut pool = cdp_unix::pool::PagePool::new();
    assert_eq!(pool.available(), 0);
    assert_eq!(pool.active_count, 0);

    // Push 3 pages
    for i in 0..3 {
        pool.push(cdp_unix::pool::WarmPage {
            browser_context_id: format!("ctx-{i}"),
            target_id: format!("target-{i}"),
        });
    }
    assert_eq!(pool.available(), 3);

    // Acquire 2
    let p1 = pool.acquire().unwrap();
    assert_eq!(p1.target_id, "target-0"); // FIFO
    assert_eq!(pool.available(), 2);
    assert_eq!(pool.active_count, 1);

    let p2 = pool.acquire().unwrap();
    assert_eq!(p2.target_id, "target-1");
    assert_eq!(pool.available(), 1);
    assert_eq!(pool.active_count, 2);

    // Release 1
    pool.released();
    assert_eq!(pool.active_count, 1);

    // Acquire last
    let p3 = pool.acquire().unwrap();
    assert_eq!(p3.target_id, "target-2");

    // Pool empty
    assert!(pool.acquire().is_none());
    assert_eq!(pool.available(), 0);
}

// ─── Unit test: Abstract socket ──────────────────────────────────────────────

#[tokio::test]
async fn test_abstract_socket_roundtrip() {
    // Dùng tên unique để tránh conflict
    let name = format!("cdp-unix-test-{}", std::process::id());

    // Listen
    let listener = cdp_unix::socket::listen(&name).unwrap();

    // Connect trong task khác
    let name_clone = name.clone();
    let client_handle = tokio::spawn(async move {
        // Đợi listener sẵn sàng
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = cdp_unix::socket::connect(&name_clone).unwrap();
        let (_, mut writer) = stream.into_split();
        cdp_unix::protocol::write_frame(&mut writer, b"xin chao").await.unwrap();
    });

    // Accept + read
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, _) = stream.into_split();
    let frame = cdp_unix::protocol::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(frame, b"xin chao");

    client_handle.await.unwrap();
}

#[tokio::test]
async fn test_abstract_socket_multiple_clients() {
    let name = format!("cdp-unix-multi-{}", std::process::id());
    let listener = cdp_unix::socket::listen(&name).unwrap();

    let name_c = name.clone();
    // Spawn 3 clients
    for i in 0..3u32 {
        let n = name_c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stream = cdp_unix::socket::connect(&n).unwrap();
            let (_, mut w) = stream.into_split();
            let msg = format!("client-{i}");
            cdp_unix::protocol::write_frame(&mut w, msg.as_bytes()).await.unwrap();
        });
    }

    let mut received = Vec::new();
    for _ in 0..3 {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _) = stream.into_split();
        let frame = cdp_unix::protocol::read_frame(&mut r).await.unwrap().unwrap();
        received.push(String::from_utf8(frame).unwrap());
    }

    received.sort();
    assert_eq!(received, vec!["client-0", "client-1", "client-2"]);
}

// ─── E2E test: cần Chrome ────────────────────────────────────────────────────

#[tokio::test]
#[ignore] // chạy: cargo test -- --ignored
async fn test_e2e_daemon_full_cycle() {
    use serde_json::json;

    let socket_name = format!("cdp-unix-e2e-{}", std::process::id());

    // Start daemon trong background task
    let sn = socket_name.clone();
    let daemon_handle = tokio::spawn(async move {
        cdp_unix::daemon::run_daemon(cdp_unix::daemon::DaemonConfig {
            socket_name: sn,
            chrome_path: None,
            chrome_args: vec![],
            warmup_count: 2,
            warmup_url: "about:blank".to_string(),
        }).await.unwrap();
    });

    // Đợi daemon khởi động
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Connect client
    let mut client = cdp_unix::client::CdpClient::connect(&socket_name).await
        .expect("không kết nối được daemon — Chrome đã cài chưa?");

    // Status
    let status = client.status().await.unwrap();
    println!("status: {status:?}");

    // Acquire page
    let (target_id, session_id) = client.acquire_page().await.unwrap();
    println!("acquired: target={target_id} session={session_id}");
    assert!(!target_id.is_empty());
    assert!(!session_id.is_empty());

    // Navigate
    let nav_result = client.cdp(
        "Page.navigate",
        json!({"url": "data:text/html,<title>Test Page</title><h1>Hello</h1>"}),
        Some(&session_id),
    ).await.unwrap();
    println!("navigate result: {nav_result}");

    // Đợi load
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Lấy title bằng Runtime.evaluate
    let eval_result = client.cdp(
        "Runtime.evaluate",
        json!({"expression": "document.title"}),
        Some(&session_id),
    ).await.unwrap();
    println!("eval result: {eval_result}");

    let title = eval_result.get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(title, "Test Page");

    // Lấy HTML content
    let html_result = client.cdp(
        "Runtime.evaluate",
        json!({"expression": "document.querySelector('h1').textContent"}),
        Some(&session_id),
    ).await.unwrap();
    let h1 = html_result.get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(h1, "Hello");

    // Release
    client.release_page(&target_id).await.unwrap();
    println!("released");

    // Warmup thêm
    let warmed = client.warmup(3, "about:blank").await.unwrap();
    assert_eq!(warmed, 3);
    println!("warmed up {warmed} pages");

    // Acquire page thứ 2 (từ pool vừa warmup)
    let t = std::time::Instant::now();
    let (tid2, sid2) = client.acquire_page().await.unwrap();
    let acquire_ms = t.elapsed().as_millis();
    println!("second acquire: {acquire_ms}ms");

    // Navigate page thứ 2
    client.cdp(
        "Page.navigate",
        json!({"url": "data:text/html,<title>Page Two</title>"}),
        Some(&sid2),
    ).await.unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    let result = client.cdp(
        "Runtime.evaluate",
        json!({"expression": "document.title"}),
        Some(&sid2),
    ).await.unwrap();
    let title2 = result.get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(title2, "Page Two");

    client.release_page(&tid2).await.unwrap();

    // Done — daemon sẽ tiếp tục chạy nhưng test kết thúc
    daemon_handle.abort();
    println!("\n=== E2E TEST PASSED ===");
}

#[tokio::test]
#[ignore]
async fn test_e2e_benchmark_acquire_release() {
    let socket_name = format!("cdp-unix-bench-{}", std::process::id());

    let sn = socket_name.clone();
    let daemon_handle = tokio::spawn(async move {
        cdp_unix::daemon::run_daemon(cdp_unix::daemon::DaemonConfig {
            socket_name: sn,
            chrome_path: None,
            chrome_args: vec![],
            warmup_count: 10,
            warmup_url: "about:blank".to_string(),
        }).await.unwrap();
    });

    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut client = cdp_unix::client::CdpClient::connect(&socket_name).await
        .expect("không kết nối được daemon");

    let count = 10;
    let t0 = std::time::Instant::now();

    for _ in 0..count {
        let (tid, _) = client.acquire_page().await.unwrap();
        client.release_page(&tid).await.unwrap();
    }

    let total = t0.elapsed();
    let avg = total / count;
    println!("\n=== BENCHMARK: {count} acquire→release ===");
    println!("total: {total:?}");
    println!("avg:   {avg:?}");
    println!("rps:   {:.1}", count as f64 / total.as_secs_f64());

    daemon_handle.abort();
}
