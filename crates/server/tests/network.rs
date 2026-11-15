//! 真实 TCP 服务契约：连接状态、共享引擎、并发与预算失败。
//! Real TCP service contracts: connection state, shared engine, concurrency, and budget failures.

use linger_protocol::{Body, Frame, Limits, Request, encode_frame, read_frame, write_frame};
use linger_server::{Config, Server};
use linger_types::Value;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// 在临时端口启动隔离服务，返回显式停止通道。
/// Starts an isolated service on an ephemeral port with an explicit stop channel.
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

/// 构造指定请求编号的类型化请求帧。
/// Builds a typed request frame with the specified request number.
fn request(id: u64, name: &str, args: Vec<Value>) -> Frame {
    Frame {
        request_id: id,
        body: Body::Request(Request::new(name, args)),
    }
}

/// 发送并读取一帧，核对响应请求 ID。
/// Sends and reads one frame and checks its matching request ID.
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

/// 同时核对稳定错误码和普通或致命帧种类。
/// Checks both the stable error code and ordinary versus fatal frame kind.
fn assert_error(body: Body, code: &str, fatal: bool) {
    match body {
        Body::Response(Err(e)) if !fatal => assert_eq!(e.code, code),
        Body::Error(e) if fatal => assert_eq!(e.code, code),
        other => panic!("Unexpected {other:?}"),
    }
}

/// 握手能力、命令错误和 QUIT 关闭行为符合协议。
/// Handshake capabilities, command errors, and QUIT closure follow the protocol.
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
    assert_eq!(info["events"], Value::Bool(true));
    assert!(matches!(&info["stream_id"], Value::Bytes(id) if id.len()==16));
    assert_eq!(
        call(&mut stream, 2, "ping", vec![]).await,
        Body::Response(Ok(Value::String("PONG".into())))
    );
    assert_error(
        call(&mut stream, 3, "GET", vec![Value::String("k".into())]).await,
        "ERR_KEY_NOT_FOUND",
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

/// 粘包中的认证、选库和关闭屏障依接收顺序执行。
/// Authentication, selection, and close barriers in coalesced frames execute in receive order.
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

/// 握手前请求、错误版本和重用 ID 必须断开。
/// Pre-handshake requests, invalid versions, and reused IDs close the connection.
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

/// 完整业务错误保留连接，结构错误关闭连接。
/// Complete business errors retain the connection while structural errors close it.
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

/// 响应超限明确报错，服务停止时清理仍存活连接。
/// Oversized responses fail explicitly and shutdown cleans up live connections.
#[tokio::test]
async fn response_budget_and_shutdown_with_live_connection() {
    let mut config = Config::default();
    config.server.max_response_bytes = 1024;
    config.engine.max_key_bytes = 256;
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

/// 连接满载拒绝新连接，半帧超时释放已有连接。
/// Full connection capacity rejects newcomers and partial-frame timeouts release existing connections.
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

/// 多连接只能看到 MSET 整体状态，竞争自增不丢更新。
/// Multiple connections observe whole MSET states and increments lose no updates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_engine_serializes_writes_and_atomic_observations() {
    let (addr, stop, task) = start(Config::default()).await;
    let mut seed = TcpStream::connect(addr).await.unwrap();
    call(&mut seed, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_eq!(
        call(
            &mut seed,
            2,
            "MSET",
            vec![
                Value::String("counter".into()),
                Value::Int(0),
                Value::String("left".into()),
                Value::Int(0),
                Value::String("right".into()),
                Value::Int(0),
            ]
        )
        .await,
        Body::Response(Ok(Value::Bool(true)))
    );
    let mut workers = tokio::task::JoinSet::new();
    for worker in 0..4 {
        workers.spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
            for iteration in 0..40 {
                let id = 2 + iteration * 3;
                assert!(matches!(
                    call(
                        &mut stream,
                        id,
                        "INCR",
                        vec![Value::String("counter".into())]
                    )
                    .await,
                    Body::Response(Ok(Value::Int(_)))
                ));
                let stamp = Value::Int(worker * 100 + iteration as i64);
                assert_eq!(
                    call(
                        &mut stream,
                        id + 1,
                        "MSET",
                        vec![
                            Value::String("left".into()),
                            stamp.clone(),
                            Value::String("right".into()),
                            stamp,
                        ]
                    )
                    .await,
                    Body::Response(Ok(Value::Bool(true)))
                );
                let Body::Response(Ok(Value::List(items))) = call(
                    &mut stream,
                    id + 2,
                    "MGET",
                    vec![Value::String("left".into()), Value::String("right".into())],
                )
                .await
                else {
                    panic!()
                };
                assert_eq!(items[0], items[1]);
            }
        });
    }
    while let Some(result) = workers.join_next().await {
        result.unwrap();
    }
    assert_eq!(
        call(&mut seed, 3, "GET", vec![Value::String("counter".into())]).await,
        Body::Response(Ok(Value::Int(160)))
    );
    assert_error(
        call(&mut seed, 4, "UNIMPLEMENTED", vec![]).await,
        "ERR_UNKNOWN_COMMAND",
        false,
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// POP 响应过大时保留列表；选库只改变当前连接上下文。
/// An oversized POP preserves the list; SELECT changes only its connection context.
#[tokio::test]
async fn data_commands_preserve_state_on_response_limits_and_isolate_databases() {
    let mut config = Config::default();
    config.server.max_response_bytes = 1024;
    config.engine.max_key_bytes = 256;
    let (addr, stop, task) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_eq!(
        call(
            &mut stream,
            2,
            "SET",
            vec![
                Value::String("items".into()),
                Value::List(vec![Value::String("x".repeat(1100))])
            ]
        )
        .await,
        Body::Response(Ok(Value::Bool(true)))
    );
    assert_error(
        call(&mut stream, 3, "LPOP", vec![Value::String("items".into())]).await,
        "ERR_LIMIT_EXCEEDED",
        false,
    );
    assert_eq!(
        call(&mut stream, 4, "LLEN", vec![Value::String("items".into())]).await,
        Body::Response(Ok(Value::UInt(1)))
    );
    call(&mut stream, 5, "SELECT", vec![Value::UInt(1)]).await;
    assert_error(
        call(&mut stream, 6, "GET", vec![Value::String("items".into())]).await,
        "ERR_KEY_NOT_FOUND",
        false,
    );
    call(
        &mut stream,
        7,
        "SET",
        vec![Value::String("items".into()), Value::Null],
    )
    .await;
    assert_eq!(
        call(&mut stream, 8, "GET", vec![Value::String("items".into())]).await,
        Body::Response(Ok(Value::Null))
    );
    call(&mut stream, 9, "SELECT", vec![Value::UInt(0)]).await;
    assert_eq!(
        call(&mut stream, 10, "TYPE", vec![Value::String("items".into())]).await,
        Body::Response(Ok(Value::String("list".into())))
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 管理命令经真实 TCP 执行；SCAN 游标隔离于连接并在切库后失效。
/// Administration runs over real TCP; SCAN cursors are connection-local and invalid after database changes.
#[tokio::test]
async fn sharded_administration_and_connection_owned_scan() {
    let mut config = Config::default();
    config.engine.shard_count = 4;
    let (addr, stop, task) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut other = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    call(&mut other, 1, "HELLO", vec![Value::UInt(1)]).await;
    call(
        &mut stream,
        2,
        "MSET",
        vec![
            Value::String("a".into()),
            Value::Int(1),
            Value::String("b".into()),
            Value::Null,
            Value::String("c".into()),
            Value::Bool(true),
        ],
    )
    .await;
    assert_eq!(
        call(
            &mut stream,
            3,
            "COPY",
            vec![
                Value::String("a".into()),
                Value::String("copied".into()),
                Value::String("DB".into()),
                Value::UInt(1)
            ]
        )
        .await,
        Body::Response(Ok(Value::Bool(true)))
    );
    let Body::Response(Ok(Value::Map(page))) = call(
        &mut stream,
        4,
        "SCAN",
        vec![
            Value::UInt(0),
            Value::String("COUNT".into()),
            Value::UInt(1),
        ],
    )
    .await
    else {
        panic!()
    };
    let Value::UInt(cursor) = page["cursor"] else {
        panic!()
    };
    assert_ne!(cursor, 0);
    assert_error(
        call(&mut other, 2, "SCAN", vec![Value::UInt(cursor)]).await,
        "ERR_INVALID_CURSOR",
        false,
    );
    call(&mut stream, 5, "SELECT", vec![Value::UInt(1)]).await;
    assert_error(
        call(&mut stream, 6, "SCAN", vec![Value::UInt(cursor)]).await,
        "ERR_INVALID_CURSOR",
        false,
    );
    assert_eq!(
        call(&mut stream, 7, "GET", vec![Value::String("copied".into())]).await,
        Body::Response(Ok(Value::Int(1)))
    );
    call(
        &mut stream,
        8,
        "SWAPDB",
        vec![Value::UInt(0), Value::UInt(1)],
    )
    .await;
    assert_eq!(
        call(&mut stream, 9, "GET", vec![Value::String("b".into())]).await,
        Body::Response(Ok(Value::Null))
    );
    call(
        &mut stream,
        10,
        "FLUSHDB",
        vec![Value::String("ASYNC".into())],
    )
    .await;
    assert_eq!(
        call(&mut stream, 11, "DBSIZE", vec![]).await,
        Body::Response(Ok(Value::UInt(0)))
    );
    call(&mut stream, 12, "FLUSHALL", vec![]).await;
    assert_eq!(
        call(&mut other, 3, "DBSIZE", vec![]).await,
        Body::Response(Ok(Value::UInt(0)))
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 跨库内存淘汰从真实协议进入，容量失败不得清掉旧数据。
/// Cross-DB eviction enters through the real protocol; rejected writes must preserve existing data.
#[tokio::test]
async fn network_eviction_preserves_rejected_commands() {
    let mut config = Config::default();
    config.engine.shard_count = 2;
    config.engine.max_keys = 1;
    config.eviction.policy = "random".into();
    let (addr, stop, task) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    call(
        &mut stream,
        2,
        "SET",
        vec![Value::String("old".into()), Value::Null],
    )
    .await;
    assert_error(
        call(
            &mut stream,
            3,
            "MSET",
            vec![
                Value::String("a".into()),
                Value::Null,
                Value::String("b".into()),
                Value::Null,
            ],
        )
        .await,
        "ERR_OUT_OF_MEMORY",
        false,
    );
    assert_eq!(
        call(&mut stream, 4, "GET", vec![Value::String("old".into())]).await,
        Body::Response(Ok(Value::Null))
    );
    call(
        &mut stream,
        5,
        "SET",
        vec![Value::String("new".into()), Value::Null],
    )
    .await;
    assert_error(
        call(&mut stream, 6, "GET", vec![Value::String("old".into())]).await,
        "ERR_KEY_NOT_FOUND",
        false,
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// TTL 经真实 TCP 保持类型、条件校验与到期可见性，错误后仍能继续使用连接。
/// TTL over real TCP preserves types, conditional validation, and expiry visibility while errors keep the connection usable.
#[tokio::test]
async fn network_ttl_queries_and_expiry_boundary() {
    let (addr, stop, task) = start(Config::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    call(&mut stream, 1, "HELLO", vec![Value::UInt(1)]).await;
    let key = Value::String("ttl".into());
    assert_eq!(
        call(
            &mut stream,
            2,
            "SET",
            vec![
                key.clone(),
                Value::Null,
                Value::String("PX".into()),
                Value::Int(10000)
            ]
        )
        .await,
        Body::Response(Ok(Value::Bool(true)))
    );
    let Body::Response(Ok(Value::Int(remaining))) =
        call(&mut stream, 3, "PTTL", vec![key.clone()]).await
    else {
        panic!()
    };
    assert!((1..=10000).contains(&remaining));
    assert_error(
        call(
            &mut stream,
            4,
            "SET",
            vec![
                key.clone(),
                Value::Int(1),
                Value::String("NX".into()),
                Value::String("PX".into()),
                Value::Int(0),
            ],
        )
        .await,
        "ERR_INVALID_ARGUMENT",
        false,
    );
    assert_eq!(
        call(&mut stream, 5, "GET", vec![key.clone()]).await,
        Body::Response(Ok(Value::Null))
    );
    assert_eq!(
        call(&mut stream, 6, "PERSIST", vec![key.clone()]).await,
        Body::Response(Ok(Value::Bool(true)))
    );
    assert_eq!(
        call(&mut stream, 7, "PTTL", vec![key.clone()]).await,
        Body::Response(Ok(Value::Int(-1)))
    );
    call(&mut stream, 8, "PEXPIRE", vec![key.clone(), Value::Int(20)]).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut id = 9;
        loop {
            if call(&mut stream, id, "PTTL", vec![key.clone()]).await
                == Body::Response(Ok(Value::Int(-2)))
            {
                break;
            }
            id += 1;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 订阅响应先于事件，半个 ACK 不被推送轮询丢弃；订阅禁止普通数据命令。
/// Subscription responses precede events, polling preserves partial ACKs, and ordinary data commands are forbidden.
#[tokio::test]
async fn subscription_barrier_and_fragmented_ack_survive_event_pushes() {
    use std::time::Duration;
    let mut config = Config::default();
    config.engine.shard_count = 1;
    let (address, stop, task) = start(config).await;
    let mut subscriber = TcpStream::connect(address).await.unwrap();
    call(&mut subscriber, 1, "HELLO", vec![Value::UInt(1)]).await;
    assert_eq!(
        call(
            &mut subscriber,
            2,
            "SUBSCRIBE",
            vec![Value::List(vec![Value::String("*".into())])]
        )
        .await,
        Body::Response(Ok(Value::Bool(true)))
    );
    assert_error(
        call(&mut subscriber, 3, "SELECT", vec![Value::UInt(1)]).await,
        "ERR_CONNECTION_STATE",
        false,
    );
    let mut writer = TcpStream::connect(address).await.unwrap();
    call(&mut writer, 1, "HELLO", vec![Value::UInt(1)]).await;
    call(
        &mut writer,
        2,
        "SET",
        vec![Value::String("a".into()), Value::Null],
    )
    .await;
    let event = read_frame(&mut subscriber, Limits::default())
        .await
        .unwrap()
        .unwrap();
    let Body::Event(value) = event.body else {
        panic!()
    };
    let event = linger_types::Event::from_value(&value).unwrap();
    let ack = encode_frame(
        &Frame {
            request_id: 4,
            body: Body::Ack(event.id.to_value()),
        },
        Limits::default(),
    )
    .unwrap();
    subscriber.write_all(&ack[..9]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(35)).await;
    call(
        &mut writer,
        3,
        "SET",
        vec![Value::String("b".into()), Value::Null],
    )
    .await;
    let pushed = read_frame(&mut subscriber, Limits::default())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(pushed.body, Body::Event(_)));
    subscriber.write_all(&ack[9..]).await.unwrap();
    let response = read_frame(&mut subscriber, Limits::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.request_id, 4);
    assert_eq!(response.body, Body::Response(Ok(Value::Bool(true))));
    assert_eq!(
        call(&mut subscriber, 5, "UNSUBSCRIBE", vec![]).await,
        Body::Response(Ok(Value::Bool(true)))
    );
    stop.send(()).unwrap();
    task.await.unwrap();
}
