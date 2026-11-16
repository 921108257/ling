//! 服务端配置及启动失败使用安全本地化诊断。
//! Server configuration and startup failures use safe localized diagnostics.

use std::{
    path::PathBuf,
    process::{Command, Output},
};

/// 收集失败诊断且不接受成功标准输出。
/// Collects failed diagnostics without accepting successful stdout.
fn error(output: Output, code: &str, examples: &str) -> String {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.starts_with(&format!("{code}: ")), "{text}");
    assert!(text.contains(&format!("\n\n{examples}\n")), "{text}");
    text
}

/// 显式配置路径失败的七语消息不暴露文件名或文件内容。
/// Seven-language explicit-config failures expose neither paths nor contents.
#[test]
fn missing_config_has_seven_localized_diagnostics() {
    for (locale, examples) in [
        ("en-US", "Usage examples"),
        ("zh-CN", "使用示例"),
        ("ko-KR", "사용 예"),
        ("ja-JP", "使用例"),
        ("ru-RU", "Примеры использования"),
        ("de-DE", "Anwendungsbeispiele"),
        ("fr-FR", "Exemples d’utilisation"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ling"))
            .args([
                "--lang",
                locale,
                "--config",
                "private-config-secret-does-not-exist.toml",
            ])
            .output()
            .unwrap();
        let text = error(output, "ERR_CONFIG_INVALID", examples);
        assert!(!text.contains("private-config-secret"));
        assert!(text.contains("ling --config ling.toml"));
    }
}

/// 错误 TOML 内含秘密不出现在诊断中；绑定占用端口也不展示底层异常。
/// Invalid TOML secrets never reach diagnostics, and occupied ports do not expose raw exceptions.
#[test]
fn invalid_config_and_bind_failures_are_sanitized() {
    let path: PathBuf =
        std::env::temp_dir().join(format!("ling-diagnostic-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        "[auth]\npassword = \"private-file-secret\"\nunknown = 3\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ling"))
        .args(["--lang", "zh-CN", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let text = error(output, "ERR_CONFIG_INVALID", "使用示例");
    assert!(!text.contains("private-file-secret"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        &path,
        format!(
            "[server]\nport = {}\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ling"))
        .args(["--lang", "en-US", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    error(output, "LOCAL_STARTUP", "Usage examples");
    std::fs::remove_file(path).unwrap();
}
