//! 服务端命令行入口、配置加载和优雅关闭信号。
//! Server command-line entry point, configuration loading, and graceful shutdown signals.

use clap::Parser;
use linger_server::{Config, Server};
use std::path::PathBuf;

/// 启动参数；凭据不进入命令行参数。
/// Startup arguments; credentials are not accepted on the command line.
#[derive(Parser)]
#[command(version, about = "linger-cache-db 阶段 3 固定分片网络服务")]
struct Args {
    /// 可选配置路径，缺省尝试工作目录 linger.toml。
    /// Optional config path; defaults to linger.toml in the working directory.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// 将失败转为非零退出状态，不输出敏感输入。
/// Converts failures to nonzero exit status without printing sensitive inputs.
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

/// 装配配置与运行时资源，完成退出前的连接清理。
/// Assembles configuration and runtime resources and cleans up connections before exit.
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
        "linger-server {} 监听 {}（阶段 3，{} 分片/库，事件禁用）",
        env!("CARGO_PKG_VERSION"),
        server.local_addr()?,
        server.shard_count()
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
