//! 错误码覆盖、翻译完整性与安全上下文的纯展示契约。
//! Pure presentation contracts for code coverage, complete translations and safe context.

use ling_cli_help::{Context, Diagnostic, Language};
use std::collections::BTreeSet;

/// 七语表包含全部稳定服务端错误与本地分类，文案不为空。
/// The seven-language table covers every stable server error and local category with nonempty messages.
#[test]
fn all_registered_errors_are_translated() {
    let table = include_str!("../src/diagnostics.tsv");
    let mut codes = BTreeSet::new();
    for line in table.lines() {
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 8);
        assert!(columns.iter().all(|value| !value.trim().is_empty()));
        assert!(codes.insert(columns[0]));
    }
    let registry = include_str!("../../types/src/error.rs");
    for line in registry.lines() {
        if let Some((_, code)) = line.split_once("=> \"") {
            let code = code.split('"').next().unwrap();
            assert!(codes.contains(code), "missing {code}");
        }
    }
}

/// 只展示合法错误码和目录中的命令，不回显原始输入。
/// Displays only valid error codes and catalog commands without echoing original input.
#[test]
fn diagnostic_context_and_unknown_outcome_are_safe() {
    let text = Diagnostic::command(
        Language::English,
        "ERR_TYPE_MISMATCH",
        "LINDEX private-secret 1",
    )
    .to_string();
    assert!(text.contains("HELP LINDEX"));
    assert!(!text.contains("private-secret"));
    let text = Diagnostic::command(
        Language::Chinese,
        "LOCAL_PARSE",
        "STE private-secret int(bad)",
    )
    .to_string();
    assert!(text.contains("HELP SET"));
    assert!(!text.contains("private-secret"));
    let text = Diagnostic::new(
        Language::English,
        "ERR_COMMIT_UNKNOWN",
        Context::Command("SET"),
    )
    .to_string();
    assert!(text.contains("may have committed"));
    assert!(text.contains("Do not replay"));
    let text = Diagnostic::new(
        Language::English,
        "ERR_FAKE\nprivate-secret",
        Context::Client,
    )
    .to_string();
    assert!(text.starts_with("ERR_SERVER:"));
    assert!(!text.contains("private-secret"));
}

/// 精确命令复用帮助中的操作示例，初始化类型和执行位置不能被家族示例替换。
/// Exact commands reuse help examples without losing initialization types or execution context.
#[test]
fn command_intent_selects_exact_examples() {
    for (command, expected) in [
        ("INCRFLOAT", "INCRFLOAT ratio float(0.5)"),
        ("MSET", "MSET first null second uint(2)"),
        ("CONFIG GET log.*", "CONFIG GET log.*"),
        ("CONFIG SET log.level bad", "CONFIG SET log.level debug"),
        ("LPUSH", "LPUSH"),
        ("FSET", "FSET"),
    ] {
        let text =
            Diagnostic::command(Language::English, "ERR_INVALID_ARGUMENT", command).to_string();
        assert!(text.contains(expected), "{command}: {text}");
        assert!(text.contains("same ling-cli session"));
    }
    let text = Diagnostic::command(Language::Chinese, "ERR_INVALID_ACK", "ACK").to_string();
    assert!(text.contains("EVENT_ID"));
    assert!(text.contains("该订阅"));
}
