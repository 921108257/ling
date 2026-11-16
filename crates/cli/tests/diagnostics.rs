//! 真实 CLI 错误布局、语言、上下文和敏感信息隔离契约。
//! Real CLI contracts for diagnostic layout, locale, context and sensitive-data isolation.

use ling_protocol::{Body, Frame, Limits, read_frame, write_frame};
use ling_types::Value;
use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Output, Stdio},
};

/// 清除环境覆盖，默认使用英语且不连接可用服务。
/// Clears locale overrides, defaulting to English and an unavailable service.
fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
    for variable in [
        "LING_LANG",
        "LC_MESSAGES",
        "LANG",
        "LING_PASSWORD",
        "LINGER_PASSWORD",
    ] {
        command.env_remove(variable);
    }
    command
        .env("LC_ALL", "C")
        .args(["--port", "0"])
        .stdin(Stdio::null());
    command
}

/// 要求失败无成功输出，输出必须为统一诊断格式。
/// Requires failure without successful output, using the uniform diagnostic layout.
fn diagnostic(output: Output, code: &str, heading: &str) -> String {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.starts_with(&format!("{code}: ")), "{text}");
    assert!(text.contains(&format!("\n\n{heading}\n  ")), "{text}");
    assert!(text.ends_with('\n'));
    text
}

/// 语法失败在联网前报告，七种语言分别翻译错误及分区标题。
/// Syntax failures report before connecting, with error and section translations in all seven languages.
#[test]
fn syntax_errors_are_offline_localized_and_contextual() {
    for (locale, examples, related) in [
        ("en-US", "Usage examples", "Related commands"),
        ("zh-CN", "使用示例", "相关命令"),
        ("ko-KR", "사용 예", "관련 명령"),
        ("ja-JP", "使用例", "関連コマンド"),
        ("ru-RU", "Примеры использования", "Связанные команды"),
        ("de-DE", "Anwendungsbeispiele", "Verwandte Befehle"),
        ("fr-FR", "Exemples d’utilisation", "Commandes associées"),
    ] {
        let text = diagnostic(
            cli()
                .args([
                    "--lang",
                    locale,
                    "-c",
                    "SET private-key-private-value int(bad)",
                ])
                .output()
                .unwrap(),
            "LOCAL_PARSE",
            examples,
        );
        assert!(text.contains(&format!("\n\n{related}\n")));
        assert!(text.contains("HELP SET"));
        assert!(!text.contains("private-key-private-value"));
        assert!(!text.contains("LOCAL_CONNECT"));
    }
}

/// clap 不输出非法值、完整业务命令或密码，即使错误来自冲突或无效数值。
/// clap never emits invalid values, complete business commands or passwords, including conflicts and invalid numbers.
#[test]
fn startup_errors_do_not_echo_values() {
    let secret = "private-argument-secret";
    let text = diagnostic(
        cli()
            .args(["--lang", "zh-CN", "--host", secret])
            .output()
            .unwrap(),
        "LOCAL_ARGUMENT",
        "使用示例",
    );
    assert!(!text.contains(secret));
    let text = diagnostic(
        cli()
            .args(["--host", secret, "--lang", "ja-JP"])
            .output()
            .unwrap(),
        "LOCAL_ARGUMENT",
        "使用例",
    );
    assert!(!text.contains(secret));
    let text = diagnostic(
        cli()
            .args(["--private-argument-secret", "--lang", "ja-JP"])
            .output()
            .unwrap(),
        "LOCAL_ARGUMENT",
        "使用例",
    );
    assert!(!text.contains(secret));
    assert!(text.contains("--host 127.0.0.1"));
    let text = diagnostic(
        cli()
            .args([
                "--lang",
                "en-US",
                "--batch",
                "-c",
                &format!("AUTH {secret}"),
            ])
            .output()
            .unwrap(),
        "LOCAL_ARGUMENT",
        "Usage examples",
    );
    assert!(!text.contains(secret));
    let text = diagnostic(
        cli()
            .args(["--lang", "ja-JP", "--private-argument-secret"])
            .output()
            .unwrap(),
        "LOCAL_ARGUMENT",
        "使用例",
    );
    assert!(!text.contains(secret));
}

/// 本地输入限制、UTF-8 和 Batch HELP 使用同一诊断且不会先连接。
/// Local input caps, UTF-8 and Batch HELP use uniform diagnostics without connecting first.
#[test]
fn input_and_batch_errors_are_structured() {
    for (batch, input, code) in [
        (false, vec![0xff, b'\n'], "LOCAL_UTF8"),
        (
            true,
            b"SET prior null\nHELP SET\n".to_vec(),
            "LOCAL_BATCH_HELP",
        ),
        (true, b"SET private int(bad)\n".to_vec(), "LOCAL_PARSE"),
        (true, b"PING\n".repeat(1001), "LOCAL_INPUT_LIMIT"),
    ] {
        let mut command = cli();
        if batch {
            command.arg("--batch");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let text = diagnostic(child.wait_with_output().unwrap(), code, "Usage examples");
        assert!(!text.contains("prior"));
        assert!(!text.contains("private"));
    }
}

/// 真实服务端错误只展示稳定错误码和本地译文，保守拼写建议来自内置命令。
/// Real server errors expose only stable codes and local text; conservative spelling suggestions use built-in commands.
#[tokio::test]
async fn server_errors_are_translated_and_suggest_known_commands() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = ling_server::Server::from_listener(listener, Default::default()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    for (source, code, term) in [
        ("GET private-key-secret", "ERR_KEY_NOT_FOUND", "HELP GET"),
        (
            "STE private-key-secret null",
            "ERR_UNKNOWN_COMMAND",
            "HELP SET",
        ),
    ] {
        let source = source.to_owned();
        let output = tokio::task::spawn_blocking(move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
            command
                .args([
                    "--lang",
                    "zh-CN",
                    "--port",
                    &port.to_string(),
                    "-c",
                    &source,
                ])
                .env_remove("LING_PASSWORD")
                .env_remove("LINGER_PASSWORD")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let text = diagnostic(output, code, "使用示例");
        assert!(text.contains(term), "{text}");
        assert!(!text.contains("private-key-secret"));
    }
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

/// 对端在收到写入后断开：展示结果未知和禁止自动重放，保留执行意图。
/// Peer disconnect after a write shows unknown outcome and no automatic replay while retaining command intent.
#[tokio::test]
async fn lost_write_response_preserves_unknown_outcome() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        for value in [
            Value::Map(BTreeMap::from([
                ("protocol".into(), Value::UInt(1)),
                ("server_version".into(), Value::String("test".into())),
                ("events".into(), Value::Bool(false)),
                ("reliable_events".into(), Value::Bool(false)),
                ("stream_id".into(), Value::Null),
            ])),
            Value::Bool(true),
        ] {
            let frame = read_frame(&mut stream, Limits::default())
                .await
                .unwrap()
                .unwrap();
            write_frame(
                &mut stream,
                &Frame {
                    request_id: frame.request_id,
                    body: Body::Response(Ok(value)),
                },
                Limits::default(),
            )
            .await
            .unwrap();
        }
        let frame = read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(frame.body, Body::Request(request) if request.command == "SET"));
    });
    let output = tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
        command
            .args([
                "--lang",
                "en-US",
                "--port",
                &port.to_string(),
                "-c",
                "SET private-key null",
            ])
            .env_remove("LING_PASSWORD")
            .env_remove("LINGER_PASSWORD")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    peer.await.unwrap();
    let text = diagnostic(output, "LOCAL_DISCONNECTED", "Usage examples");
    assert!(text.contains("may have committed"));
    assert!(text.contains("Do not replay"));
    assert!(text.contains("HELP SET"));
    assert!(!text.contains("private-key"));
}

/// 订阅控制错误及无效编码不会被成功退出掩盖，并提供 ACK 上下文。
/// Subscription-control errors and invalid encoding cannot be hidden by successful exit and retain ACK context.
#[tokio::test]
async fn subscription_control_errors_keep_nonzero_exit_status() {
    use std::io::BufRead;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = ling_server::Server::from_listener(listener, Default::default()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let mut writer = ling_client::Client::connect(address, None, 0, Default::default())
        .await
        .unwrap();
    for (control, code) in [
        (
            b"GET private-secret\nUNSUBSCRIBE\n".to_vec(),
            "LOCAL_SUBSCRIPTION_COMMAND",
        ),
        (vec![0xff, b'\n'], "LOCAL_UTF8"),
    ] {
        let (ready, mut received) = tokio::sync::oneshot::channel();
        let process = tokio::task::spawn_blocking(move || {
            let mut child = Command::new(env!("CARGO_BIN_EXE_ling-cli"))
                .args([
                    "--lang",
                    "en-US",
                    "--port",
                    &address.port().to_string(),
                    "-c",
                    "SUBSCRIBE diagnostic:*",
                ])
                .env_remove("LING_PASSWORD")
                .env_remove("LINGER_PASSWORD")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut event = String::new();
            reader.read_line(&mut event).unwrap();
            assert!(!event.is_empty());
            ready.send(()).unwrap();
            child.stdin.take().unwrap().write_all(&control).unwrap();
            child.wait_with_output().unwrap()
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for value in 0..1000 {
                writer
                    .execute(
                        "SET",
                        vec![Value::String("diagnostic:event".into()), Value::Int(value)],
                    )
                    .await
                    .unwrap();
                tokio::select! {
                    result = &mut received => { result.unwrap(); return; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {}
                }
            }
            panic!("subscription received no event");
        })
        .await
        .unwrap();
        let text = diagnostic(process.await.unwrap(), code, "Usage examples");
        assert!(text.contains("HELP ACK"));
        assert!(!text.contains("private-secret"));
    }
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    server_task.await.unwrap().unwrap();
}

/// 仅将规范的逐项失败本地化；成功项中的相似用户 Map 不得被当作错误。
/// Localizes only contract-defined item failures; similar user Maps inside successful values remain data.
#[tokio::test]
async fn item_failures_are_localized_without_reinterpreting_user_values() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = ling_server::Server::from_listener(listener, Default::default()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let mut writer = ling_client::Client::connect(address, None, 0, Default::default())
        .await
        .unwrap();
    let user = Value::Map(BTreeMap::from([
        ("ok".into(), Value::Bool(false)),
        ("code".into(), Value::String("ERR_KEY_NOT_FOUND".into())),
        (
            "message".into(),
            Value::String("private-user-message".into()),
        ),
    ]));
    writer
        .execute(
            "SET",
            vec![Value::String("diagnostic:user".into()), user.clone()],
        )
        .await
        .unwrap();
    writer
        .execute(
            "SET",
            vec![
                Value::String("diagnostic:map".into()),
                Value::Map(BTreeMap::from([("nested".into(), user.clone())])),
            ],
        )
        .await
        .unwrap();
    for (locale, heading) in [("en-US", "Usage examples"), ("zh-CN", "使用示例")] {
        for (source, code) in [
            ("MGET diagnostic:user absent", "ERR_KEY_NOT_FOUND"),
            ("FGET diagnostic:map nested absent", "ERR_FIELD_NOT_FOUND"),
        ] {
            let output = tokio::task::spawn_blocking(move || {
                Command::new(env!("CARGO_BIN_EXE_ling-cli"))
                    .args([
                        "--lang",
                        locale,
                        "--port",
                        &address.port().to_string(),
                        "-c",
                        source,
                    ])
                    .env_remove("LING_PASSWORD")
                    .env_remove("LINGER_PASSWORD")
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(!output.status.success());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.starts_with(&format!("{code}: ")));
            assert!(stderr.contains(heading));
            assert!(!stderr.contains("private-user-message"));
            let value =
                ling_cli::parse_value(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
            let Value::List(items) = value else {
                panic!("missing result List")
            };
            let Value::Map(success) = &items[0] else {
                panic!("missing success envelope")
            };
            assert_eq!(success.get("ok"), Some(&Value::Bool(true)));
            assert_eq!(success.get("value"), Some(&user));
            let Value::Map(failure) = &items[1] else {
                panic!("missing error envelope")
            };
            assert_eq!(failure.get("ok"), Some(&Value::Bool(false)));
            assert_eq!(failure.get("code"), Some(&Value::String(code.into())));
            let expected = ling_cli_help::Diagnostic::new(
                ling_cli_help::Language::from_locale(locale),
                code,
                ling_cli_help::Context::Client,
            );
            assert_eq!(
                failure.get("message"),
                Some(&Value::String(expected.message().into()))
            );
        }
    }
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_ling-cli"))
            .args([
                "--lang",
                "en-US",
                "--port",
                &address.port().to_string(),
                "-c",
                "GET diagnostic:user",
            ])
            .env_remove("LING_PASSWORD")
            .env_remove("LINGER_PASSWORD")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        ling_cli::parse_value(std::str::from_utf8(&output.stdout).unwrap()).unwrap(),
        user
    );
    let output = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ling-cli"))
            .args([
                "--lang",
                "zh-CN",
                "--port",
                &address.port().to_string(),
                "--batch",
            ])
            .env_remove("LING_PASSWORD")
            .env_remove("LINGER_PASSWORD")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"MGET diagnostic:user absent\nFGET diagnostic:map nested absent\n")
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
    assert!(stdout.contains("private-user-message"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("ERR_KEY_NOT_FOUND:"));
    assert!(stderr.contains("ERR_FIELD_NOT_FOUND:"));
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_ling-cli"))
            .args([
                "--lang",
                "en-US",
                "--port",
                &address.port().to_string(),
                "-c",
                "CONFIG SET server.port 1234",
            ])
            .env_remove("LING_PASSWORD")
            .env_remove("LINGER_PASSWORD")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let text = diagnostic(output, "ERR_CONFIG_IMMUTABLE", "Usage examples");
    assert!(text.contains("HELP CONFIG SET"));
    assert!(text.contains("CONFIG SET log.level debug"));
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    server_task.await.unwrap().unwrap();
}
