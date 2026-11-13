use linger_server::{Config, Server};
use std::{
    io::Write,
    process::{Command, Output, Stdio},
};
use tokio::{net::TcpListener, sync::oneshot};

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
            .contains("ERR_UNKNOWN_COMMAND")
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
