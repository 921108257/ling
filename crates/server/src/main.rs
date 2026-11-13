use clap::Parser;
use linger_server::{Config, Server};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "linger-cache-db 阶段 1 网络服务")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let config = match args.config {
        Some(path) => Config::load(&path)?,
        None => {
            if std::path::Path::new("linger.toml").exists() {
                Config::load(std::path::Path::new("linger.toml"))?
            } else {
                Config::default()
            }
        }
    };
    let server = Server::bind(config).await?;
    eprintln!(
        "linger-server {} 监听 {}（阶段 1，事件禁用）",
        env!("CARGO_PKG_VERSION"),
        server.local_addr()?
    );
    server
        .run(async {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("register SIGTERM");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
    Ok(())
}
