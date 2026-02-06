// =============================================================================
// main.rs — CLI entry point
// =============================================================================
//
// Abstract Unix socket — không tạo file, giao tiếp trực tiếp qua kernel.
//
//   cdp-unix daemon                     # Start (mặc định @cdp-unix)
//   cdp-unix daemon --name myapp       # Dùng tên khác → @myapp
//   cdp-unix status                     # Xem trạng thái daemon
//   cdp-unix warmup --count 10          # Warmup thêm 10 pages
//   cdp-unix navigate https://x.com     # Acquire → navigate → title → release
//   cdp-unix bench --count 100          # Đo latency
// =============================================================================

mod browser;
mod client;
mod daemon;
mod pool;
mod protocol;
mod socket;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "cdp-unix", about = "CDP bridge qua abstract Unix socket — không file, không WebSocket")]
struct Cli {
    /// Tên abstract socket (mặc định: cdp-unix → @cdp-unix trong kernel)
    #[arg(long, default_value = "cdp-unix", global = true)]
    name: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Khởi động daemon (Chrome + abstract socket)
    Daemon {
        /// Đường dẫn Chrome/Chromium
        #[arg(long)]
        chrome_path: Option<String>,

        /// Số page warmup khi khởi động
        #[arg(long, default_value = "4")]
        warmup: usize,

        /// URL cho warmup pages
        #[arg(long, default_value = "about:blank")]
        warmup_url: String,

        /// Tham số Chrome bổ sung
        #[arg(long)]
        chrome_arg: Vec<String>,
    },

    /// Xem trạng thái daemon
    Status,

    /// Warmup thêm pages
    Warmup {
        #[arg(long, default_value = "4")]
        count: usize,

        #[arg(long, default_value = "about:blank")]
        url: String,
    },

    /// Acquire page → navigate → in title → release
    Navigate {
        /// URL đích
        url: String,
    },

    /// Gửi 1 CDP command
    Exec {
        /// CDP method (vd: "Page.navigate")
        #[arg(long)]
        method: String,

        /// JSON params
        #[arg(long, default_value = "{}")]
        params: String,
    },

    /// Benchmark acquire→release
    Bench {
        #[arg(long, default_value = "100")]
        count: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cdp_unix=info".parse().unwrap()),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon { chrome_path, warmup, warmup_url, chrome_arg } => {
            println!("╔═══════════════════════════════════════════════╗");
            println!("║           cdp-unix daemon starting           ║");
            println!("║  Chrome ←[pipe]→ Daemon ←[@{}]→ Client", cli.name);
            println!("║  Không file socket. Kernel namespace.        ║");
            println!("╚═══════════════════════════════════════════════╝");

            daemon::run_daemon(daemon::DaemonConfig {
                socket_name: cli.name,
                chrome_path,
                chrome_args: chrome_arg,
                warmup_count: warmup,
                warmup_url,
            }).await?;
        }

        Commands::Status => {
            let mut c = client::CdpClient::connect(&cli.name).await?;
            match c.status().await? {
                protocol::DaemonMsg::StatusResponse {
                    pool_available, active_sessions, chrome_pid, uptime_secs, ..
                } => {
                    println!("┌──────────────────────────────┐");
                    println!("│  cdp-unix @ {:<17}│", cli.name);
                    println!("├──────────────────────────────┤");
                    println!("│  Chrome PID:   {:<14}│", chrome_pid);
                    println!("│  Uptime:       {:<14}│", format!("{}s", uptime_secs));
                    println!("│  Pool:         {:<14}│", pool_available);
                    println!("│  Active:       {:<14}│", active_sessions);
                    println!("└──────────────────────────────┘");
                }
                other => println!("{other:?}"),
            }
        }

        Commands::Warmup { count, url } => {
            let mut c = client::CdpClient::connect(&cli.name).await?;
            println!("warmup {count} pages (url={url})...");
            let created = c.warmup(count, &url).await?;
            println!("done: {created}/{count}");
        }

        Commands::Navigate { url } => {
            let mut c = client::CdpClient::connect(&cli.name).await?;

            let t0 = std::time::Instant::now();
            let (target_id, session_id) = c.acquire_page().await?;
            println!("acquire: {}ms", t0.elapsed().as_millis());

            c.cdp("Page.navigate", json!({"url": &url}), Some(&session_id)).await?;
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

            let result = c.cdp(
                "Runtime.evaluate",
                json!({"expression": "document.title"}),
                Some(&session_id),
            ).await?;

            let title = result.get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("<no title>");

            println!("title: {title}");
            c.release_page(&target_id).await?;
            println!("total: {}ms", t0.elapsed().as_millis());
        }

        Commands::Exec { method, params } => {
            let mut c = client::CdpClient::connect(&cli.name).await?;
            let (target_id, session_id) = c.acquire_page().await?;
            println!("session: {session_id}");

            let params: serde_json::Value = serde_json::from_str(&params)?;
            let result = c.cdp(&method, params, Some(&session_id)).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);

            c.release_page(&target_id).await?;
        }

        Commands::Bench { count } => {
            let mut c = client::CdpClient::connect(&cli.name).await?;

            println!("warmup {count} pages...");
            c.warmup(count, "about:blank").await?;

            println!("bench {count} acquire→release...");
            let t0 = std::time::Instant::now();
            let mut latencies = Vec::with_capacity(count);

            for _ in 0..count {
                let t = std::time::Instant::now();
                let (tid, _) = c.acquire_page().await?;
                c.release_page(&tid).await?;
                latencies.push(t.elapsed());
            }

            let total = t0.elapsed();
            latencies.sort();
            let p50 = latencies[count / 2];
            let p99 = latencies[(count as f64 * 0.99) as usize];

            println!("┌───────────────────────────────┐");
            println!("│  {count} cycles");
            println!("├───────────────────────────────┤");
            println!("│  Total:  {:>10.1?}", total);
            println!("│  Avg:    {:>10.1?}", total / count as u32);
            println!("│  P50:    {:>10.1?}", p50);
            println!("│  P99:    {:>10.1?}", p99);
            println!("│  Min:    {:>10.1?}", latencies.first().unwrap());
            println!("│  Max:    {:>10.1?}", latencies.last().unwrap());
            println!("│  RPS:    {:>10.1}", count as f64 / total.as_secs_f64());
            println!("└───────────────────────────────┘");
        }
    }

    Ok(())
}
