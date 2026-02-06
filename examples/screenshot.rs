// =============================================================================
// examples/screenshot.rs — Chụp screenshot 1 trang web
// =============================================================================
//
// Chạy:
//   1. Mở terminal 1:  cargo run -- daemon
//   2. Mở terminal 2:  cargo run --example screenshot
//
// Kết quả: file screenshot.png trong thư mục hiện tại
// =============================================================================

use anyhow::Result;
use serde_json::json;
use std::io::Write;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".to_string());

    println!("→ Kết nối daemon @cdp-unix...");
    let mut client = cdp_unix::client::CdpClient::connect("cdp-unix").await?;

    // Lấy page từ pool
    let t0 = std::time::Instant::now();
    let (target_id, session_id) = client.acquire_page().await?;
    println!("→ Acquire page: {}ms", t0.elapsed().as_millis());

    // Bật Page events (cần cho Page.loadEventFired)
    client
        .cdp("Page.enable", json!({}), Some(&session_id))
        .await?;

    // Navigate
    println!("→ Navigate tới: {url}");
    let nav = client
        .cdp("Page.navigate", json!({"url": &url}), Some(&session_id))
        .await?;
    println!("  frameId: {}", nav.get("frameId").unwrap_or(&json!("?")));

    // Đợi trang load (đơn giản dùng sleep, production nên dùng Page.loadEventFired)
    println!("→ Đợi trang load...");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Lấy title
    let title_result = client
        .cdp(
            "Runtime.evaluate",
            json!({"expression": "document.title"}),
            Some(&session_id),
        )
        .await?;
    let title = title_result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("<không có title>");
    println!("→ Title: {title}");

    // Lấy kích thước trang
    let size_result = client
        .cdp(
            "Runtime.evaluate",
            json!({"expression": "JSON.stringify({w: document.documentElement.scrollWidth, h: document.documentElement.scrollHeight})"}),
            Some(&session_id),
        )
        .await?;
    let size_str = size_result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    println!("→ Kích thước: {size_str}");

    // Set viewport
    client
        .cdp(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 1280,
                "height": 720,
                "deviceScaleFactor": 2,
                "mobile": false,
            }),
            Some(&session_id),
        )
        .await?;

    // Chụp screenshot
    println!("→ Chụp screenshot...");
    let screenshot = client
        .cdp(
            "Page.captureScreenshot",
            json!({"format": "png", "quality": 100}),
            Some(&session_id),
        )
        .await?;

    let base64_data = screenshot
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("không có screenshot data"))?;

    // Decode base64 → file
    let png_bytes = base64_decode(base64_data)?;
    let output_path = "screenshot.png";
    let mut file = std::fs::File::create(output_path)?;
    file.write_all(&png_bytes)?;
    println!("→ Đã lưu: {output_path} ({} bytes)", png_bytes.len());

    // Trả page
    client.release_page(&target_id).await?;
    println!("→ Done! Tổng thời gian: {}ms", t0.elapsed().as_millis());

    Ok(())
}

/// Simple base64 decoder (không cần thêm dependency)
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }

    let input = input.as_bytes();
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;

    for &byte in input {
        if byte == b'=' || byte == b'\n' || byte == b'\r' {
            continue;
        }
        let val = lookup[byte as usize];
        if val == 255 {
            continue;
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(output)
}
