//! 服务端本地帮助语言和无配置启动的子进程契约。
//! Subprocess contracts for localized server help without loading configuration.

use std::{
    collections::HashSet,
    io::ErrorKind,
    net::TcpListener,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

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

/// 本地代理陷阱检测意外联网，并限制帮助进程运行时间。
/// A local proxy trap detects accidental network requests and bounds help execution time.
fn offline_output(command: &mut Command) -> Output {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let address = format!("http://{}", proxy.local_addr().unwrap());
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(variable, &address);
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        match proxy.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Ok(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("local help unexpectedly attempted a network request");
            }
            Err(error) => panic!("proxy trap failed: {error}"),
        }
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("local help did not exit within five seconds");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 执行成功的帮助进程并读取其 UTF-8 输出。
/// Runs a successful help process and decodes its UTF-8 output.
fn text(command: &mut Command) -> String {
    let output = offline_output(command);
    assert!(
        output.status.success(),
        "server help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

/// 根帮助仍使用标准开关，主题帮助由测试显式提供入口。
/// Root help uses the standard flag; topic-help tests supply their own entry points.
fn help(command: &mut Command) -> String {
    text(command.arg("--help"))
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

/// 七语更新帮助的三个真实入口完全同文，既不读取配置也不访问发行站点。
/// Three real update-help entry points match in seven locales without reading config or reaching the release site.
#[test]
fn update_help_entry_points_are_identical_and_offline() {
    let mut translations = HashSet::new();
    for locale in [
        "en-US", "zh-CN", "ko-KR", "ja-JP", "ru-RU", "de-DE", "fr-FR",
    ] {
        let expected = text(server().args(["--lang", locale, "update", "--help"]));
        assert_eq!(
            text(server().args(["help", "update", "--lang", locale])),
            expected
        );
        assert_eq!(
            text(server().args(["--help", "update", "--lang", locale])),
            expected
        );
        for option in [
            "ling update [OPTIONS]",
            "--check",
            "--version <VERSION>",
            "--stable",
            "--lang",
        ] {
            assert!(expected.contains(option), "{locale}: missing {option}");
        }
        assert!(!expected.contains("missing-help-contract-config.toml"));
        assert!(
            translations.insert(expected),
            "duplicated update help for {locale}"
        );
    }
}

/// 未知主题在本地安全失败，不回显自由输入或尝试网络与配置。
/// Unknown topics fail locally without echoing free-form input or attempting networking or config loading.
#[test]
fn unknown_server_help_topics_are_safe_and_offline() {
    for arguments in [
        vec!["help", "private-update-help-topic"],
        vec!["--help", "private-update-help-topic"],
        vec!["help", "update", "private-update-help-topic"],
    ] {
        let output = offline_output(server().args(arguments).args(["--lang", "en-US"]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.starts_with("LOCAL_HELP_TOPIC: "));
        assert!(!error.contains("private-update-help-topic"));
        assert!(!error.contains("missing-help-contract-config"));
    }
}
