use linger_protocol::{Body, Frame, Limits, Request, encode_frame, read_frame, write_frame};
use linger_server::{Config, Server};
use linger_types::Value;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

async fn start(
    config: Config,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server::from_listener(listener, config).unwrap();
    let addr = server.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx, task)
}

fn request(id: u64, name: &str, args: Vec<Value>) -> Frame {
    Frame {
        request_id: id,
        body: Body::Request(Request::new(name, args)),
    }
}

async fn call(stream: &mut TcpStream, id: u64, name: &str, args: Vec<Value>) -> Body {
    write_frame(stream, &request(id, name, args), Limits::default())
        .await
        .unwrap();
    let frame = read_frame(stream, Limits::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.request_id, id);
    frame.body
}

fn assert_error(body: Body, code: &str, fatal: bool) {
    match body {
        Body::Response(Err(e)) if !fatal => assert_eq!(e.code, code),
        Body::Error(e) if fatal => assert_eq!(e.code, code),
        other => panic!("Unexpected {other:?}"),
    }
}

#[tokio::test]
async fn hello_capabilities_and_command_errors_and_quit() {
    let (addr, stop, task) = start(Config::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let Body::Response(Ok(Value::Map(info))) =
        call(&mut stream, 1, "hello", vec![Value::UInt(1)]).await
    else {
        panic!()
    };
    assert_eq!(
        info["server_version"],
        Value::String(env!("CARGO_PKG_VERSION").into())
    );
    assert_eq!(info["events"], Value::Bool(false));
    assert_eq!(info["stream_id"], Value::Null);
    assert_eq!(
        call(&mut stream, 2, "ping", vec![]).await,
        Body::Response(Ok(Value::String("PONG".into())))
    );
    assert_error(
        call(&mut stream, 3, "GET", vec![Value::String("k".into())]).await,
        "ERR_UNKNOWN_COMMAND",
        false,
    );
    assert_error(
        call(&mut stream, 4, "HELLO", vec![Value::UInt(1)]).await,
        "ERR_CONNECTION_STATE",
        false,
    );
    assert_error(
        call(&mut stream, 5, "PING", vec![Value::Null]).await,
        "ERR_INVALID_ARGUMENT",
        false,
    );
    assert_error(
        call(&mut stream, 6, "SELECT", vec![Value::UInt(16)]).await,
        "ERR_DB_OUT_OF_RANGE",
        false,
    );
    assert_eq!(
        call(&mut stream, 7, "SELECT", vec![Value::UInt(15)]).await,
        Body::Response(Ok(Value::Bool(true)))
    );
    assert_eq!(
        call(&mut stream, 8, "QUIT", vec![]).await,
        Body::Response(Ok(Value::Bool(true)))
    );
    assert!(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .is_none()
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn authentication_and_select_barriers_follow_receive_order() {
    let mut config = Config::default();
    config.auth.enabled = true;
    config.auth.password = "secret".into();
    let (addr, stop, task) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_error(
        call(&mut stream, 2, "SELECT", vec![Value::UInt(1)]).await,
        "ERR_UNAUTHENTICATED",
        false,
    );
    assert_error(
        call(&mut stream, 3, "AUTH", vec![Value::String("wrong".into())]).await,
        "ERR_AUTH_FAILED",
        false,
    );
    let frames = [
        request(4, "AUTH", vec![Value::String("secret".into())]),
        request(5, "SELECT", vec![Value::UInt(1)]),
        request(6, "PING", vec![Value::String("after".into())]),
        request(7, "QUIT", vec![]),
    ];
    let bytes: Vec<_> = frames
        .iter()
        .flat_map(|f| encode_frame(f, Limits::default()).unwrap())
        .collect();
    stream.write_all(&bytes).await.unwrap();
    for id in 4..=7 {
        let response = read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.request_id, id);
        assert_eq!(
            response.body,
            Body::Response(Ok(if id == 6 {
                Value::String("after".into())
            } else {
                Value::Bool(true)
            }))
        );
    }
    assert!(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .is_none()
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_prehello_wrong_versions_and_reused_ids() {
    let (addr, stop, task) = start(Config::default()).await;
    for (name, arg, code) in [
        ("PING", vec![], "ERR_CONNECTION_STATE"),
        ("HELLO", vec![Value::UInt(2)], "ERR_UNSUPPORTED_VERSION"),
    ] {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        assert_error(call(&mut stream, 1, name, arg).await, code, true);
        assert!(
            read_frame(&mut stream, Limits::default())
                .await
                .unwrap()
                .is_none()
        );
    }
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut bytes = encode_frame(
        &request(1, "HELLO", vec![Value::UInt(1)]),
        Limits::default(),
    )
    .unwrap();
    bytes[2] = 9;
    stream.write_all(&bytes).await.unwrap();
    assert_error(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap()
            .body,
        "ERR_UNSUPPORTED_VERSION",
        true,
    );
    assert!(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .is_none()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_error(
        call(&mut stream, 1, "PING", vec![]).await,
        "ERR_PROTOCOL",
        true,
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn semantic_errors_keep_connection_and_structural_errors_close_it() {
    let (addr, stop, task) = start(Config::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    let mut bytes =
        encode_frame(&request(2, "PING", vec![Value::UInt(0)]), Limits::default()).unwrap();
    let tag = bytes.len() - 9;
    bytes[tag..].copy_from_slice(&[4, 0x7f, 0xf8, 0, 0, 0, 0, 0, 1]);
    stream.write_all(&bytes).await.unwrap();
    assert_error(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap()
            .body,
        "ERR_INVALID_VALUE",
        false,
    );
    assert_eq!(
        call(&mut stream, 3, "PING", vec![]).await,
        Body::Response(Ok(Value::String("PONG".into())))
    );
    let mut bytes = encode_frame(&request(4, "PING", vec![]), Limits::default()).unwrap();
    bytes[5] = 128;
    stream.write_all(&bytes).await.unwrap();
    assert_error(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap()
            .body,
        "ERR_PROTOCOL",
        true,
    );
    assert!(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .is_none()
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn response_budget_and_shutdown_with_live_connection() {
    let mut config = Config::default();
    config.server.max_response_bytes = 1024;
    let (addr, stop, task) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_error(
        call(
            &mut stream,
            2,
            "PING",
            vec![Value::String("x".repeat(1024))],
        )
        .await,
        "ERR_LIMIT_EXCEEDED",
        false,
    );
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn partial_frame_deadline_and_connection_limit() {
    let mut config = Config::default();
    config.server.max_connections = 1;
    config.server.request_timeout_ms = 100;
    let (addr, stop, task) = start(config).await;
    let mut first = TcpStream::connect(addr).await.unwrap();
    call(&mut first, 1, "HELLO", vec![Value::UInt(1)]).await;
    let mut second = TcpStream::connect(addr).await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            read_frame(&mut second, Limits::default())
        )
        .await
        .unwrap()
        .unwrap()
        .is_none()
    );
    first.write_all(b"L").await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            read_frame(&mut first, Limits::default())
        )
        .await
        .unwrap()
        .unwrap()
        .is_none()
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}
