// =============================================================================
// examples/scraper.rs — Scrape nhiều trang ĐỒNG THỜI bằng tokio
// =============================================================================
//
// Chạy:
//   1. Terminal 1:  cargo run -- daemon --warmup 10
//   2. Terminal 2:
//      cargo run --example scraper                              # URLs mặc định
//      cargo run --example scraper -- --file examples/test_domains.json  # từ file
//      cargo run --example scraper -- https://example.com       # 1 URL
//
// File JSON hỗ trợ 2 format:
//   - ["https://a.com", "https://b.com"]          (mảng string)
//   - [{"domain": "a.com"}, {"domain": "b.com"}]  (mảng object có field "domain")
//
// Mỗi URL chạy song song trong 1 tokio task riêng.
// =============================================================================

use anyhow::Result;
use serde_json::json;

/// Kết quả scrape 1 trang
#[derive(Debug)]
struct PageInfo {
    url: String,
    title: String,
    description: String,
    h1: String,
    links_count: u64,
    load_time_ms: u128,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load URLs: từ file JSON (--file path) hoặc dùng danh sách mặc định
    let urls: Vec<String> = match std::env::args().nth(1).as_deref() {
        Some("--file") => {
            let path = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "examples/test_domains.json".to_string());
            let data = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("không đọc được {path}: {e}"))?;
            load_urls_from_json(&data)?
        }
        Some(url) => vec![url.to_string()],
        None => vec![
            "https://example.com".into(),
            "https://httpbin.org/html".into(),
            "https://www.rust-lang.org".into(),
            "https://tokio.rs".into(),
            "https://cloudflare.com".into(),
            "https://github.com/circa10a/easy-soap-request".into(),
        ],
    };

    println!("╔═══════════════════════════════════════╗");
    println!("║  cdp-unix scraper — {} URLs      ║", urls.len());
    println!("║  (async concurrent)                   ║");
    println!("╚═══════════════════════════════════════╝\n");

    // Warmup đủ pages trước
    let mut client = cdp_unix::client::CdpClient::connect("cdp-unix").await?;
    println!("→ Warmup {} pages...", urls.len());
    client.warmup(urls.len(), "about:blank").await?;
    drop(client);

    // Spawn tất cả tasks đồng thời — mỗi task tạo CdpClient riêng
    let total_start = std::time::Instant::now();
    println!("→ Scraping {} URLs đồng thời...\n", urls.len());

    let mut handles = Vec::new();
    for url in &urls {
        let url = url.to_string();
        handles.push(tokio::spawn(async move { scrape_page(&url).await }));
    }

    // Thu thập kết quả
    let mut results = Vec::new();
    for handle in handles {
        match handle.await.unwrap() {
            Ok(info) => {
                println!(
                    "  ✓ {} — \"{}\" ({}ms)",
                    info.url, info.title, info.load_time_ms
                );
                results.push(info);
            }
            Err(e) => {
                println!("  ✗ lỗi: {}", e);
            }
        }
    }

    let total_ms = total_start.elapsed().as_millis();

    // In kết quả
    println!("\n┌{}┐", "─".repeat(70));
    println!("│  KẾT QUẢ SCRAPE (async concurrent)");
    println!("├{}┤", "─".repeat(70));

    for info in &results {
        println!("│");
        println!("│  URL:         {}", info.url);
        println!("│  Title:       {}", info.title);
        println!("│  Description: {}", truncate(&info.description, 60));
        println!("│  H1:          {}", info.h1);
        println!("│  Links:       {}", info.links_count);
        println!("│  Load time:   {}ms", info.load_time_ms);
    }

    println!("│");
    println!(
        "│  Tổng: {} trang trong {}ms (chạy đồng thời)",
        results.len(),
        total_ms
    );
    if !results.is_empty() {
        let sum_single: u128 = results.iter().map(|r| r.load_time_ms).sum();
        let max_single = results.iter().map(|r| r.load_time_ms).max().unwrap_or(0);
        println!(
            "│  Nếu tuần tự: ~{}ms | Tiết kiệm: ~{}ms",
            sum_single,
            sum_single.saturating_sub(total_ms)
        );
        println!("│  Trang chậm nhất: {}ms", max_single);
    }
    println!("└{}┘", "─".repeat(70));

    Ok(())
}

/// Scrape 1 trang — mỗi task tạo CdpClient riêng để chạy song song
async fn scrape_page(url: &str) -> Result<PageInfo> {
    let start = std::time::Instant::now();

    let mut client = cdp_unix::client::CdpClient::connect("cdp-unix").await?;
    let (target_id, session_id) = client.acquire_page().await?;

    // Navigate
    client
        .cdp("Page.navigate", json!({"url": url}), Some(&session_id))
        .await?;

    // Đợi load 1s
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Lấy title
    let title = eval_string(&mut client, &session_id, "document.title").await;

    // Lấy meta description
    let description = eval_string(
        &mut client,
        &session_id,
        r#"(document.querySelector('meta[name="description"]') || {}).content || ''"#,
    )
    .await;

    // Lấy h1
    let h1 = eval_string(
        &mut client,
        &session_id,
        "(document.querySelector('h1') || {}).textContent || ''",
    )
    .await;

    // Đếm links
    let links_count = eval_number(
        &mut client,
        &session_id,
        "document.querySelectorAll('a').length",
    )
    .await;

    let load_time_ms = start.elapsed().as_millis();

    // Release page
    client.release_page(&target_id).await?;

    Ok(PageInfo {
        url: url.to_string(),
        title,
        description,
        h1,
        links_count,
        load_time_ms,
    })
}

async fn eval_string(
    client: &mut cdp_unix::client::CdpClient,
    session_id: &str,
    expression: &str,
) -> String {
    match client
        .cdp(
            "Runtime.evaluate",
            json!({"expression": expression}),
            Some(session_id),
        )
        .await
    {
        Ok(result) => result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        Err(_) => String::new(),
    }
}

async fn eval_number(
    client: &mut cdp_unix::client::CdpClient,
    session_id: &str,
    expression: &str,
) -> u64 {
    match client
        .cdp(
            "Runtime.evaluate",
            json!({"expression": expression}),
            Some(session_id),
        )
        .await
    {
        Ok(result) => result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Parse URLs từ JSON file — hỗ trợ 2 format:
///   - `["https://a.com", ...]`                     (mảng string)
///   - `[{"domain": "a.com", ...}, ...]`            (mảng object, lấy field "domain")
fn load_urls_from_json(data: &str) -> Result<Vec<String>> {
    let val: serde_json::Value = serde_json::from_str(data)?;
    let arr = val.as_array().ok_or_else(|| anyhow::anyhow!("JSON phải là mảng"))?;

    let mut urls = Vec::new();
    for item in arr {
        if let Some(s) = item.as_str() {
            // Format: ["https://a.com", ...]
            urls.push(s.to_string());
        } else if let Some(domain) = item.get("domain").and_then(|v| v.as_str()) {
            // Format: [{"domain": "a.com", ...}, ...]
            let url = if domain.starts_with("http://") || domain.starts_with("https://") {
                domain.to_string()
            } else {
                format!("https://{domain}")
            };
            urls.push(url);
        }
    }

    if urls.is_empty() {
        anyhow::bail!("không tìm thấy URL nào trong JSON");
    }
    Ok(urls)
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
