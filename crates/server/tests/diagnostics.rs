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

/// 更新入口在配置之前执行；不可达代理防止错误路径依赖外部网络。
/// Updates run before config loading; unreachable proxies keep error paths independent of external networking.
fn updater(locale: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ling"));
    command
        .args([
            "--lang",
            locale,
            "--config",
            "private-update-config-does-not-exist.toml",
            "update",
        ])
        .env_remove("LING_LANG")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env("LC_ALL", "C")
        .env("HTTP_PROXY", "http://127.0.0.1:0")
        .env("HTTPS_PROXY", "http://127.0.0.1:0")
        .env("ALL_PROXY", "http://127.0.0.1:0")
        .env("http_proxy", "http://127.0.0.1:0")
        .env("https_proxy", "http://127.0.0.1:0")
        .env("all_proxy", "http://127.0.0.1:0")
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    command
}

/// 非法版本和降级在任何网络进度输出前拒绝，七语诊断保留正确更新指导。
/// Invalid versions and downgrades fail before network progress output, retaining update guidance in seven locales.
#[test]
#[cfg(feature = "self-update")]
fn invalid_update_versions_fail_before_download_and_config_loading() {
    for (locale, examples) in [
        ("en-US", "Usage examples"),
        ("zh-CN", "使用示例"),
        ("ko-KR", "사용 예"),
        ("ja-JP", "使用例"),
        ("ru-RU", "Примеры использования"),
        ("de-DE", "Anwendungsbeispiele"),
        ("fr-FR", "Exemples d’utilisation"),
    ] {
        for version in ["private-update-version-secret", "0.0.0", "v0.0.0"] {
            let output = updater(locale)
                .args(["--version", version, "--check"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let text = error(output, "LOCAL_UPDATE_VERSION", examples);
            assert!(text.contains("ling update --check"));
            assert!(text.contains("ling help update"));
            assert!(!text.contains("private-update"));
            assert!(!text.contains("ling --config"));
        }
    }
}

/// 禁用构建在网络和配置加载前给出明确原因；Disabled builds explain the missing feature before any I/O.
#[test]
#[cfg(not(feature = "self-update"))]
fn disabled_update_has_explicit_local_diagnostic() {
    for arguments in [
        vec!["--check"],
        vec!["--version", "0.0.0"],
        vec!["--stable"],
    ] {
        let text = error(
            updater("en-US").args(arguments).output().unwrap(),
            "LOCAL_UPDATE_DISABLED",
            "Usage examples",
        );
        assert!(text.contains("self-update"));
        assert!(!text.contains("private-update"));
    }
}

/// clap 更新参数错误使用更新上下文，不显示秘密参数或数据库连接示例。
/// Clap update-argument failures use update guidance without echoing secrets or database connection examples.
#[test]
fn invalid_update_arguments_keep_update_context_in_every_language() {
    for (locale, examples) in [
        ("en-US", "Usage examples"),
        ("zh-CN", "使用示例"),
        ("ko-KR", "사용 예"),
        ("ja-JP", "使用例"),
        ("ru-RU", "Примеры использования"),
        ("de-DE", "Anwendungsbeispiele"),
        ("fr-FR", "Exemples d’utilisation"),
    ] {
        for arguments in [
            vec!["--version"],
            vec!["--private-update-option", "private-update-argument-secret"],
            vec!["--check=private-update-argument-secret"],
            vec!["private-update-argument-secret"],
        ] {
            let output = updater(locale).args(arguments).output().unwrap();
            assert_eq!(output.status.code(), Some(2));
            let text = error(output, "LOCAL_ARGUMENT", examples);
            assert!(text.contains("ling update --check"), "{text}");
            assert!(text.contains("ling help update"), "{text}");
            assert!(!text.contains("private-update"));
            assert!(!text.contains("ling-cli"));
            assert!(!text.contains("ling --config"));
        }
    }
}
