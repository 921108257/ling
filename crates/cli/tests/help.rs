//! 本地帮助、语言选择和延迟连接的真实 CLI 契约。
//! Real CLI contracts for local help, language selection, and lazy connections.

use std::{
    collections::HashSet,
    io::Write,
    process::{Command, Output, Stdio},
};

/// 清理外部环境，使用不可能提供数据库服务的端口。
/// Clears inherited settings and uses a port that cannot serve a database.
fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
    command
        .args(["--port", "0"])
        .env_remove("LING_LANG")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env("LC_ALL", "C")
        .env_remove("LING_PASSWORD")
        .env_remove("LINGER_PASSWORD")
        .stdin(Stdio::null());
    command
}

/// 要求成功且 stderr 为空，再读取 UTF-8 输出。
/// Requires success and empty stderr before decoding UTF-8 output.
fn successful_text(output: Output) -> String {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

/// 将标准输入写给真实进程并收集结果。
/// Writes standard input to a real process and collects its result.
fn with_input(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// 帮助与速查无需服务或凭据，不把密码写入输出。
/// Help and references need no service or credentials and never print passwords.
#[test]
fn local_help_is_available_without_a_server() {
    for args in [
        vec!["--help"],
        vec!["--cheatsheet"],
        vec!["-c", "HELP"],
        vec!["-c", "HELP SET"],
    ] {
        let text = successful_text(
            cli()
                .args(args)
                .env("LING_PASSWORD", "help-contract-secret")
                .output()
                .unwrap(),
        );
        assert!(!text.trim().is_empty());
        assert!(!text.contains("help-contract-secret"));
    }
    let reference = successful_text(cli().arg("--cheatsheet").output().unwrap());
    for command in ["SET", "SCAN", "FGET", "SUNIONSTORE", "ACK", "SHUTDOWN"] {
        assert!(reference.contains(command), "missing {command}");
    }
    let set = successful_text(cli().args(["-c", "HELP SET"]).output().unwrap());
    for term in ["SET", "PX", "NX", "XX"] {
        assert!(set.contains(term), "SET help omits {term}");
    }
}

/// 七种语言有独立输出，显式语言和环境优先级可在子进程观察。
/// Seven locales have distinct output and subprocesses expose locale precedence.
#[test]
fn help_locales_and_precedence_are_observable() {
    let mut versions = HashSet::new();
    for language in [
        "en-US", "zh-CN", "ko-KR", "ja-JP", "ru-RU", "de-DE", "fr-FR",
    ] {
        let text = successful_text(cli().args(["--lang", language, "--help"]).output().unwrap());
        assert!(text.contains("ling-cli"));
        assert!(text.contains("--cheatsheet"));
        assert!(text.contains("--tls-server-name"));
        assert!(versions.insert(text), "duplicated help for {language}");
    }
    let english = successful_text(cli().args(["--lang", "en-US", "--help"]).output().unwrap());
    let chinese = successful_text(cli().args(["--lang", "zh-CN", "--help"]).output().unwrap());
    assert_eq!(
        successful_text(cli().arg("--help").output().unwrap()),
        english
    );
    for locale in ["eo_EO.UTF-8", "zh_TW.UTF-8", "zh-Hant-TW", "POSIX"] {
        assert_eq!(
            successful_text(cli().arg("--help").env("LC_ALL", locale).output().unwrap()),
            english,
            "unexpected fallback for {locale}"
        );
    }
    assert_eq!(
        successful_text(
            cli()
                .args(["--lang", "en-US", "--help"])
                .env("LING_LANG", "zh-CN")
                .env("LC_ALL", "ja_JP.UTF-8")
                .output()
                .unwrap()
        ),
        english
    );
    assert_eq!(
        successful_text(
            cli()
                .arg("--help")
                .env("LING_LANG", "zh-CN")
                .env("LC_ALL", "en_US.UTF-8")
                .output()
                .unwrap()
        ),
        chinese
    );
    assert_eq!(
        successful_text(
            cli()
                .arg("--help")
                .env("LC_ALL", "zh_CN.UTF-8")
                .env("LC_MESSAGES", "en_US.UTF-8")
                .env("LANG", "ja_JP.UTF-8")
                .output()
                .unwrap()
        ),
        chinese
    );
    assert_eq!(
        successful_text(
            cli()
                .arg("--help")
                .env_remove("LC_ALL")
                .env("LC_MESSAGES", "zh_CN.UTF-8")
                .env("LANG", "en_US.UTF-8")
                .output()
                .unwrap()
        ),
        chinese
    );
    assert_eq!(
        successful_text(
            cli()
                .arg("--help")
                .env_remove("LC_ALL")
                .env("LANG", "zh_CN.UTF-8")
                .output()
                .unwrap()
        ),
        chinese
    );

    // 帮助开关不截断语言覆盖，命令内容中的 --lang 也不是启动选项。
    // Help flags do not truncate overrides, and --lang inside command text is not a startup flag.
    let japanese = successful_text(cli().args(["--lang", "ja-JP", "--help"]).output().unwrap());
    assert_eq!(
        successful_text(cli().args(["--help", "--lang", "ja-JP"]).output().unwrap()),
        japanese
    );
    assert_eq!(
        successful_text(
            cli()
                .args(["-c", "HELP --lang zh-CN", "--help", "--lang", "ja-JP"])
                .output()
                .unwrap()
        ),
        japanese
    );
}

/// 普通输入的 HELP 和 QUIT 不触发连接；未知主题失败但不回显原文。
/// HELP and QUIT in regular input stay offline; unknown topics fail without echoes.
#[test]
fn input_help_and_errors_do_not_connect_or_echo_input() {
    let text = successful_text(with_input(cli(), "HELP SET\nHELP SCAN\nQUIT\n"));
    assert!(text.contains("SET"));
    assert!(text.contains("SCAN"));
    let secret_topic = "private-help-topic-7f418";
    let output = cli()
        .args(["-c", &format!("HELP {secret_topic}")])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains(secret_topic)
    );
}

/// 批量中的 HELP 在网络连接前拒绝，先前的数据命令也未发送。
/// HELP in a batch is rejected before connecting, so prior data commands stay unsent.
#[test]
fn batch_help_is_rejected_before_connecting() {
    let mut command = cli();
    command.arg("--batch");
    let output = with_input(command, "SET prior null\nHELP SET\n");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("HELP"));
    assert!(!error.contains("prior"));
}

/// 内置详情的所有示例都使用真实 CLI 语法，占位事件 ID 单独排除。
/// Every detail example uses actual CLI syntax, apart from the explicit event-ID placeholder.
#[test]
fn built_in_help_examples_use_real_cli_grammar() {
    for row in include_str!("../../cli-help/src/command-layout.tsv").lines() {
        let columns: Vec<_> = row.split('\t').collect();
        for source in columns[3].split("\\n") {
            if source.starts_with("ling-cli ") || source == "ACK EVENT_ID" {
                continue;
            }
            ling_cli::parse_command(source)
                .unwrap_or_else(|_| panic!("invalid built-in help example for {}", columns[0]));
        }
    }
}

/// 新密码变量优先，旧变量仅在新变量缺失时生效。
/// The new password variable takes precedence; legacy credentials only fill its absence.
#[tokio::test]
async fn password_precedence_and_legacy_fallback() {
    let mut config = ling_server::Config::default();
    config.auth.enabled = true;
    config.auth.password = "password-contract-secret".into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = ling_server::Server::from_listener(listener, config).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(async move {
        server
            .run(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let results = tokio::task::spawn_blocking(move || {
        [
            (Some("password-contract-secret"), "wrong-legacy", true),
            (Some("wrong-current"), "password-contract-secret", false),
            (None, "password-contract-secret", true),
        ]
        .map(|(current, legacy, success)| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
            command
                .args(["--port", &port.to_string(), "-c", "PING"])
                .env_remove("LING_PASSWORD")
                .env("LINGER_PASSWORD", legacy)
                .stdin(Stdio::null());
            if let Some(password) = current {
                command.env("LING_PASSWORD", password);
            }
            (command.output().unwrap(), success)
        })
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    server_task.await.unwrap();
    for (output, success) in results {
        assert_eq!(output.status.success(), success);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if success {
            assert_eq!(stdout.trim(), "\"PONG\"");
        } else {
            assert!(stdout.is_empty());
            assert!(stderr.contains("ERR_AUTH_FAILED"));
        }
        for password in ["password-contract-secret", "wrong-legacy", "wrong-current"] {
            assert!(!stdout.contains(password));
            assert!(!stderr.contains(password));
        }
    }
}
