//! 真实 CLI 子进程与 TCP 服务的端到端契约。
//! End-to-end contracts between real CLI subprocesses and the TCP service.

use ling_server::{Config, Server};
use std::{
    io::Write,
    process::{Command, Output, Stdio},
};
use tokio::{net::TcpListener, sync::oneshot};

/// 在临时端口启动可选认证服务。
/// Starts a service with optional authentication on an ephemeral port.
async fn start(auth: bool) -> (u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let mut config = Config::default();
    if auth {
        config.auth.enabled = true;
        config.auth.password = "test-secret".into();
    }
    start_config(config).await
}

/// 真实服务沿用完整配置验证，允许订阅测试固定分片及重投时限。
/// Real services retain complete validation while subscription tests fix shards and retry timing.
async fn start_config(config: Config) -> (u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Server::from_listener(listener, config).unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (port, tx, task)
}

/// 运行真实 CLI 子进程并收集输出，显式清理继承密码。
/// Runs a real CLI subprocess and collects output after clearing inherited passwords.
async fn cli(port: u16, source: &str, stdin: bool, password: Option<&str>) -> Output {
    let source = source.to_owned();
    let password = password.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
        command
            .args(["--port", &port.to_string()])
            .env_remove("LING_PASSWORD")
            .env_remove("LINGER_PASSWORD")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(password) = password {
            command.env("LINGER_PASSWORD", password);
        }
        if stdin {
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        } else {
            command.args(["-c", &source]).output().unwrap()
        }
    })
    .await
    .unwrap()
}

/// 标准输入逐行执行保持结果顺序，任一失败保留非零退出码。
/// Line-by-line stdin execution preserves result order and any failure retains a nonzero exit code.
#[tokio::test]
async fn real_cli_ping_unknown_command_and_batch_input_exit_status() {
    let (port, stop, task) = start(false).await;
    let output = cli(port, "PING \"你好 世界\"", false, None).await;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "\"你好 世界\"\n");
    assert!(output.stderr.is_empty());
    let output = cli(port, "GET missing", false, None).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("ERR_KEY_NOT_FOUND")
    );
    let output = cli(port, "SELECT 15\nSELECT 16\nPING\nQUIT\n", true, None).await;
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "true\n\"PONG\"\ntrue\n"
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("ERR_DB_OUT_OF_RANGE")
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 真正 Batch 与逐行输入分开；一项失败保留已完成项及非零退出码。
/// Actual Batch differs from line input; item errors preserve completed results and a nonzero exit code.
#[tokio::test]
async fn explicit_batch_and_operations_commands() {
    let (port, stop, task) = start(false).await;
    let output = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ling-cli"))
            .args(["--port", &port.to_string(), "--batch"])
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
            .write_all(b"SET batch null\nINCR batch\nGET batch\n")
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "true\nnull\n");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("ERR_TYPE_MISMATCH")
    );
    let output = cli(port, "CONFIG GET auth.password", false, None).await;
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("<redacted>")
    );
    let output = cli(port, "INFO stats", false, None).await;
    assert!(output.status.success());
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 真实认证失败通过 stderr 和退出码表达，不回显密码。
/// Real authentication failures use stderr and exit status without echoing passwords.
#[tokio::test]
async fn real_cli_auth_errors_do_not_echo_passwords() {
    let (port, stop, task) = start(true).await;
    let output = cli(port, "PING", false, Some("test-secret")).await;
    assert!(output.status.success());
    let output = cli(port, "PING", false, Some("wrong-secret")).await;
    assert!(!output.status.success());
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("ERR_AUTH_FAILED"));
    assert!(!text.contains("wrong-secret"));
    let output = cli(port, "PING", false, None).await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("ERR_UNAUTHENTICATED")
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// CLI 通过真实 TCP 保留十种类型、整数精度和数据库隔离。
/// CLI preserves all ten types, integer precision, and database isolation over real TCP.
#[tokio::test]
async fn typed_data_commands_work_through_sdk_and_tcp() {
    let (port, stop, task) = start(false).await;
    let values = [
        "null",
        "true",
        "int(-9223372036854775808)",
        "uint(18446744073709551615)",
        "float(inf)",
        "\"中 文\"",
        "bytes(0x00ff)",
        "list([null, int(1)])",
        "map({\"n\": uint(1)})",
        "set([int(1), uint(1)])",
    ];
    let mut source = String::new();
    let mut expected = String::new();
    for (index, value) in values.iter().enumerate() {
        source.push_str(&format!("SET k{index} {value}\nGET k{index}\n"));
        expected.push_str(&format!("true\n{value}\n"));
    }
    source.push_str("SET counter int(0)\nINCRBY counter 2\nSET items list([])\nRPUSH items null uint(3)\nLPOP items 1\nSELECT 1\nSET counter uint(9)\nSELECT 0\nGET counter\n");
    expected.push_str("true\nint(2)\ntrue\nuint(2)\nlist([null])\ntrue\ntrue\ntrue\nint(2)\n");
    let output = cli(port, &source, true, None).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    let output = cli(port, "INCR k0", false, None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ERR_TYPE_MISMATCH"));
    let output = cli(port, "UNIMPLEMENTED", false, None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ERR_UNKNOWN_COMMAND"));
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// CLI 跨库、遍历及清空命令共享 SDK 协议，无客户端数据库模拟。
/// CLI cross-DB, query, and flush commands share the SDK protocol without client-side database emulation.
#[tokio::test]
async fn cli_cross_database_queries_and_flush() {
    let (port, stop, task) = start(false).await;
    let output = cli(port,
        "SET a int(3)\nCOPY a b REPLACE DB 1\nSELECT 1\nGET b\nKEYS b\nMOVE b 2\nSWAPDB 1 2\nGET b\nFLUSHDB ASYNC\nDBSIZE\nSCAN 0 COUNT 10000\n",
        true, None).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(
        "true\ntrue\ntrue\nint(3)\nlist([\"b\"])\ntrue\ntrue\nint(3)\ntrue\nuint(0)\n"
    ));
    assert!(stdout.contains("\"cursor\": uint(0)"));
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 真实 CLI 保留 TTL 查询哨兵值，并把相对期限按 Int 送达服务端。
/// The real CLI preserves TTL sentinel values and sends relative deadlines as Int arguments.
#[tokio::test]
async fn cli_ttl_lifecycle_and_invalid_conditional_write() {
    let (port, stop, task) = start(false).await;
    let source = "SET ttl null NX PX 10000\nPERSIST ttl\nPTTL ttl\nPEXPIRE ttl -1\nPTTL ttl\nSET text \"PX\"\nGET text\n";
    let output = cli(port, source, true, None).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "true\ntrue\nint(-1)\ntrue\nint(-2)\ntrue\n\"PX\"\n"
    );
    let output = cli(port, "SET text null NX PX 0\nGET text\n", true, None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ERR_INVALID_ARGUMENT"));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "\"PX\"\n");
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 子进程异常或断言失败时关闭并回收，仅管理本测试启动的 CLI。
/// Reaps only this test's CLI if the process or an assertion fails.
struct SubscriptionProcess(std::process::Child);

impl Drop for SubscriptionProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 订阅退出路径必须分别验证真实 ACK 状态。
/// Subscription exits require independent verification of actual acknowledgement state.
#[derive(Clone, Copy)]
enum SubscriptionExit {
    Eof,
    Ack,
    AutoAck,
    OutputFailure,
    #[cfg(unix)]
    Cancel,
}

/// 用命名组的公开 INFO 确认订阅建立，不以睡眠或重复写入猜测就绪。
/// Uses public named-group INFO to confirm subscription readiness without sleep-based guesses or repeated writes.
async fn wait_for_cli_group(writer: &mut ling_client::Client) {
    use ling_client::Value;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let Value::Map(info) = writer
                .execute("INFO", vec![Value::String("events".into())])
                .await
                .unwrap()
            else {
                panic!("INFO must be a Map");
            };
            let Value::Map(events) = &info["events"] else {
                panic!("events must be a Map");
            };
            let Value::List(groups) = &events["consumer_groups"] else {
                panic!("consumer_groups must be a List");
            };
            if groups.iter().any(|group| {
                matches!(group, Value::Map(fields) if fields.get("name") == Some(&Value::String("n:cli-test".into())))
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("CLI subscription was not established");
}

/// 为退出和重投设置明确期限，并在失败时保留子进程 stderr。
/// Bounds exit and redelivery waits while retaining subprocess stderr on failure.
async fn subscription_exit_contract(action: SubscriptionExit) {
    use ling_client::{Client, Value};
    use std::io::{BufRead, BufReader, Read};
    use std::time::Duration;

    let mut config = Config::default();
    config.engine.shard_count = 1;
    config.events.consumer_timeout_ms = 200;
    config.events.retry_interval_ms = 20;
    let (port, stop, task) = start_config(config).await;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut writer = Client::connect(address, None, 0, Default::default())
        .await
        .unwrap();
    let mut observer = Client::connect(address, None, 0, Default::default())
        .await
        .unwrap()
        .subscribe(vec!["cli:*".into()], Some("observer"))
        .await
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ling-cli"));
    command
        .args([
            "--lang",
            "en-US",
            "--port",
            &port.to_string(),
            "--count",
            "1",
            "-c",
            "SUBSCRIBE cli:* GROUP cli-test",
        ])
        .env_remove("LING_PASSWORD")
        .env_remove("LINGER_PASSWORD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if matches!(
        action,
        SubscriptionExit::AutoAck | SubscriptionExit::OutputFailure
    ) {
        command.arg("--auto-ack");
    }
    let mut process = SubscriptionProcess(command.spawn().unwrap());
    let mut input = process.0.stdin.take().unwrap();
    let output = process.0.stdout.take().unwrap();
    let (sent, received) = oneshot::channel();
    let reader = if matches!(action, SubscriptionExit::OutputFailure) {
        // 关闭唯一读端，强制真实管道写失败，验证 auto-ack 不能先于输出。
        // Close the only reader to force a real pipe write failure before auto-ack.
        drop(output);
        None
    } else {
        Some(std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line);
            let _ = sent.send((result, line));
        }))
    };
    wait_for_cli_group(&mut writer).await;
    writer
        .execute("SET", vec![Value::String("cli:key".into()), Value::Null])
        .await
        .unwrap();
    let original = tokio::time::timeout(Duration::from_secs(5), observer.next_event())
        .await
        .unwrap()
        .unwrap();
    observer.ack(original.id).await.unwrap();
    if reader.is_some() {
        let (read, line) = tokio::time::timeout(Duration::from_secs(5), received)
            .await
            .expect("CLI did not display the event")
            .unwrap();
        assert!(read.unwrap() > 0);
        let id = ling_cli::parse_event_id(line.split_whitespace().next().unwrap()).unwrap();
        assert_eq!(id, original.id);
        match action {
            SubscriptionExit::Ack => {
                assert!(process.0.try_wait().unwrap().is_none());
                writeln!(input, "ACK {}", ling_cli::display_event_id(original.id)).unwrap();
            }
            #[cfg(unix)]
            SubscriptionExit::Cancel => {
                // 只向测试创建的子进程发 SIGINT，不广播至父进程或其他控制台。
                // Send SIGINT only to this test's subprocess, never its parent or another console.
                assert!(
                    Command::new("kill")
                        .args(["-INT", &process.0.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
            }
            _ => {}
        }
    }
    // 取消测试保留 stdin，避免真实信号与测试自己制造的 EOF 竞争。
    // Keep stdin open for cancellation so the real signal cannot race a test-created EOF.
    let input = if matches!(
        action,
        SubscriptionExit::Eof | SubscriptionExit::OutputFailure
    ) {
        drop(input);
        None
    } else {
        Some(input)
    };
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("CLI did not exit");
    drop(input);
    let mut stderr = String::new();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    match action {
        SubscriptionExit::Eof => {
            assert!(!status.success(), "{stderr}");
            assert!(
                stderr.contains("LOCAL_SUBSCRIPTION_UNACKNOWLEDGED"),
                "{stderr}"
            );
            assert!(stderr.contains("remains unacknowledged"), "{stderr}");
            assert!(stderr.contains("HELP ACK"), "{stderr}");
        }
        SubscriptionExit::OutputFailure => {
            assert!(!status.success(), "{stderr}");
            assert!(stderr.contains("LOCAL_IO"), "{stderr}");
        }
        _ => {
            assert!(status.success(), "{stderr}");
            assert!(stderr.is_empty(), "{stderr}");
        }
    }
    if let Some(reader) = reader {
        reader.join().unwrap();
    }

    let mut resumed = Client::connect(address, None, 0, Default::default())
        .await
        .unwrap()
        .subscribe(vec!["cli:*".into()], Some("cli-test"))
        .await
        .unwrap();
    let acknowledged = matches!(action, SubscriptionExit::Ack | SubscriptionExit::AutoAck);
    if acknowledged {
        // 新哨兵应为组的下一条，不能用短暂无事件推断旧事件已确认。
        // A new sentinel must be next; a short absence of events cannot prove acknowledgement.
        writer
            .execute(
                "SET",
                vec![Value::String("cli:sentinel".into()), Value::Null],
            )
            .await
            .unwrap();
    }
    let replay = tokio::time::timeout(Duration::from_secs(5), resumed.next_event())
        .await
        .expect("Named group did not deliver its expected event")
        .unwrap();
    if acknowledged {
        assert_eq!(replay.key, "cli:sentinel");
        assert_eq!(replay.id.sequence, original.id.sequence + 1);
    } else {
        assert_eq!(replay.id, original.id);
    }
    resumed.ack(replay.id).await.unwrap();
    resumed.close().await.unwrap();
    observer.close().await.unwrap();
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// EOF 明确失败且原事件重投；EOF fails explicitly and the original event is redelivered.
#[tokio::test]
async fn cli_subscription_eof_keeps_event_unacknowledged() {
    subscription_exit_contract(SubscriptionExit::Eof).await;
}

/// 显式 ACK 成功退出且原事件不重投；Explicit ACK exits successfully without redelivering the original event.
#[tokio::test]
async fn cli_subscription_requires_explicit_ack() {
    subscription_exit_contract(SubscriptionExit::Ack).await;
}

/// 成功输出后自动 ACK；Auto-ack follows successful output.
#[tokio::test]
async fn cli_subscription_auto_ack_follows_output() {
    subscription_exit_contract(SubscriptionExit::AutoAck).await;
}

/// 真实坏管道不触发自动 ACK；A real broken pipe cannot trigger auto-ack.
#[tokio::test]
async fn cli_subscription_output_failure_keeps_event_unacknowledged() {
    subscription_exit_contract(SubscriptionExit::OutputFailure).await;
}

/// Unix 真实 SIGINT 是取消而非 EOF，原事件仍可重投。
/// A real Unix SIGINT cancels rather than reporting EOF and leaves the event for redelivery.
#[cfg(unix)]
#[tokio::test]
async fn cli_subscription_ctrl_c_keeps_event_unacknowledged() {
    subscription_exit_contract(SubscriptionExit::Cancel).await;
}
