//! 源码构建关闭更新时保留可诊断的命令入口。
//! Keeps a diagnostic command entry when source builds disable self updates.

use clap::Args as ClapArgs;
use ling_cli_help::{Context, Diagnostic, Language};

/// 与完整更新器接受相同参数；解析后始终报告编译时禁用。
/// Accepts the same arguments as the full updater and reports compile-time disablement.
#[derive(ClapArgs)]
pub(super) struct Args {
    /// 仅检查版本；Check for an update.
    #[arg(long)]
    check: bool,
    /// 指定目标版本；Select a target version.
    #[arg(long = "version", value_name = "VERSION")]
    target_version: Option<String>,
    /// 仅选择稳定版；Select stable releases only.
    #[arg(long)]
    stable: bool,
}

/// 禁用时不访问网络或文件系统；No network or filesystem access when disabled.
pub(super) fn run(args: Args, language: Language) -> Result<(), Diagnostic> {
    let Args {
        check,
        target_version,
        stable,
    } = args;
    let _ = (check, target_version, stable);
    Err(Diagnostic::new(
        language,
        "LOCAL_UPDATE_DISABLED",
        Context::Update,
    ))
}
