//! 真实 CLI 子进程与 TCP 服务的端到端契约。
//! End-to-end contracts between real CLI subprocesses and the TCP service.

use linger_server::{Config, Server};
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_linger-cli"));
        command
            .args(["--port", &port.to_string()])
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
