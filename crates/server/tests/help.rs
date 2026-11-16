//! 服务端本地帮助语言和无配置启动的子进程契约。
//! Subprocess contracts for localized server help without loading configuration.

use std::{collections::HashSet, process::Command};

/// 固定英语环境并提供不存在的配置，确保帮助不读取配置或监听。
/// Fixes an English locale and absent config, ensuring help neither loads nor binds.
fn server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ling"));
    command
        .args(["--config", "missing-help-contract-config.toml"])
        .env_remove("LING_LANG")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env("LC_ALL", "C");
    command
}

/// 执行成功的帮助进程并读取其 UTF-8 输出。
/// Runs a successful help process and decodes its UTF-8 output.
fn help(command: &mut Command) -> String {
    let output = command.arg("--help").output().unwrap();
    assert!(
        output.status.success(),
        "server help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

/// 服务端七种语言均有独立帮助；参数优先于环境，不支持语言回退英语。
/// Server help distinguishes seven locales; flags override env and unsupported locales use English.
#[test]
fn server_help_is_local_and_localized() {
    let english = help(server().args(["--lang", "en-US"]));
    assert_eq!(help(&mut server()), english);
    assert!(english.contains("ling"));
    assert!(english.contains("--config"));
    let mut versions = HashSet::new();
    for locale in [
        "en-US", "zh-CN", "ko-KR", "ja-JP", "ru-RU", "de-DE", "fr-FR",
    ] {
        let text = help(server().args(["--lang", locale]));
        assert!(text.contains("--config"));
        assert!(versions.insert(text), "duplicated server help for {locale}");
    }
    assert_eq!(help(server().env("LC_ALL", "eo_EO.UTF-8")), english);
    assert_eq!(
        help(server().args(["--lang", "en-US"]).env("LING_LANG", "zh-CN")),
        english
    );
    let chinese = help(server().args(["--lang", "zh-CN"]));
    assert_eq!(help(server().env("LC_ALL", "zh_CN.UTF-8")), chinese);
    assert_eq!(
        help(
            server()
                .env("LING_LANG", "zh-CN")
                .env("LC_ALL", "en_US.UTF-8")
        ),
        chinese
    );
}
