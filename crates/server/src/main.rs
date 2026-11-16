//! 服务端命令行入口、配置加载和优雅关闭信号。
//! Server command-line entry point, configuration loading, and graceful shutdown signals.

use clap::Parser;
use ling_cli_help::{Context, Diagnostic, Language};
use ling_server::{Config, Server};
use std::path::PathBuf;

/// 启动参数；凭据不进入命令行参数。
/// Startup arguments; credentials are not accepted on the command line.
#[derive(Parser)]
#[command(name = "ling", version)]
struct Args {
    /// 本地帮助语言，默认按系统语言选择。
    /// Local help language, detected from the system by default.
    #[arg(long)]
    lang: Option<String>,
    /// 可选配置路径，缺省尝试工作目录 ling.toml，再兼容 linger.toml。
    /// Optional config path; tries ling.toml, then legacy linger.toml in the working directory.
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
async fn run() -> Result<(), Diagnostic> {
    let (args, language) = ling_cli_help::parse_localized::<Args>(true);
    let config = match args.config {
        Some(path) => Config::load(&path)
            .map_err(|error| Diagnostic::new(language, &error.code, Context::Server))?,
        None => {
            if std::path::Path::new("ling.toml").exists() {
                Config::load(std::path::Path::new("ling.toml"))
                    .map_err(|error| Diagnostic::new(language, &error.code, Context::Server))?
            } else if std::path::Path::new("linger.toml").exists() {
                Config::load(std::path::Path::new("linger.toml"))
                    .map_err(|error| Diagnostic::new(language, &error.code, Context::Server))?
            } else {
                Config::default()
            }
        }
    };
    let server = Server::bind(config)
        .await
        .map_err(|error| startup_error(error.as_ref(), language))?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| Diagnostic::new(language, "LOCAL_STARTUP", Context::Server))?;
    server
        .run(async {
            #[cfg(unix)]
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await
        .map_err(|_| Diagnostic::new(language, "LOCAL_STARTUP", Context::Server))?;
    Ok(())
}

/// 不展示路径、配置片段或底层异常；仅保留稳定码和翻译。
/// Never displays paths, config fragments or low-level errors; retains stable codes and translations only.
fn startup_error(error: &(dyn std::error::Error + 'static), language: Language) -> Diagnostic {
    let code = error
        .downcast_ref::<ling_types::DbError>()
        .map_or("LOCAL_STARTUP", |error| error.code.as_str());
    Diagnostic::new(language, code, Context::Server)
}
