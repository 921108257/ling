//! 三语言共用会话向量，受控 TCP 对端验证响应匹配和不重放。
//! Shared session vectors use a controlled TCP peer to verify matching and no replay.

use ling_client::{Client, Error, LocalError, Options, Pool, PoolOptions, Request, Value};
use ling_protocol::{Body, Frame, Limits, ReadError, encode_frame, read_frame, write_frame};
use ling_types::{DbError, EventId};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};

/// 固定夹具不依赖被测编码器产生期望值。
/// The fixed fixture never derives its expected bytes from the tested encoder.
fn fixture_frame(name: &str) -> Vec<u8> {
    let row = include_str!("../../../tests/fixtures/protocol/frames.tsv")
        .lines()
        .find(|line| line.starts_with(&format!("{name}|")))
        .unwrap();
    let hex = row.split_once('|').unwrap().1.replace(' ', "");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// 对端读取设置上限，失败不能让测试永久等待。
/// Every peer read is bounded so failures cannot leave the test hanging.
async fn receive(stream: &mut TcpStream) -> Option<Frame> {
    match timeout(
        Duration::from_secs(3),
        read_frame(stream, Limits::default()),
    )
    .await
    .unwrap()
    {
        Ok(frame) => frame,
        Err(ReadError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            None
        }
        Err(error) => panic!("peer read: {error:?}"),
    }
}

async fn send(stream: &mut TcpStream, id: u64, body: Body) {
    write_frame(
        stream,
        &Frame {
            request_id: id,
            body,
        },
        Limits::default(),
    )
    .await
    .unwrap();
}

/// 空闲订阅可主动发心跳，关闭检查只排除新的业务请求。
/// Idle subscriptions may send heartbeats; closure checks exclude new business requests.
async fn expect_closed(stream: &mut TcpStream) {
    for _ in 0..4 {
        match receive(stream).await {
            None => return,
            Some(Frame {
                body: Body::Heartbeat,
                ..
            }) => {}
            Some(frame) => panic!("unexpected repeated request: {frame:?}"),
        }
    }
    panic!("peer did not close");
}

/// 同时验证初始化顺序与重建后的固定数据库。
/// Checks initialization order and the fixed database after rebuilding a connection.
async fn initialize(stream: &mut TcpStream, db: u16) {
    let hello = receive(stream).await.unwrap();
    assert_eq!(
        hello,
        Frame {
            request_id: 1,
            body: Body::Request(Request::new("HELLO", vec![Value::UInt(1)]))
        }
    );
    let mut identity = vec![0; 16];
    identity[15] = 1;
    send(
        stream,
        1,
        Body::Response(Ok(Value::Map(BTreeMap::from([
            ("protocol".into(), Value::UInt(1)),
            ("server_version".into(), Value::String("fixture".into())),
            ("events".into(), Value::Bool(true)),
            ("reliable_events".into(), Value::Bool(false)),
            ("stream_id".into(), Value::Bytes(identity)),
        ])))),
    )
    .await;
    assert_eq!(
        receive(stream).await.unwrap(),
        Frame {
            request_id: 2,
            body: Body::Request(Request::new("SELECT", vec![Value::UInt(db.into())]))
        }
    );
    send(stream, 2, Body::Response(Ok(Value::Bool(true)))).await;
}

fn success(mode: &str) -> Body {
    if mode == "batch" {
        Body::BatchResponse(vec![Ok(Value::UInt(1))])
    } else {
        Body::Response(Ok(if mode == "subscription" {
            Value::Bool(true)
        } else {
            Value::UInt(1)
        }))
    }
}

/// 向量中的服务端动作，最终必须看到客户端关闭且没有第二次业务请求。
/// Executes the fixture peer action and requires closure without another business request.
async fn peer(mut stream: TcpStream, mode: &str, action: &str) -> usize {
    let idle = mode == "idle-subscription";
    let subscribed = mode.ends_with("subscription");
    initialize(&mut stream, 0).await;
    let prepare = receive(&mut stream).await.unwrap();
    assert_eq!(prepare.request_id, 3);
    let Body::Request(request) = prepare.body else {
        panic!("preparation request")
    };
    assert_eq!(
        request.command,
        if subscribed { "SUBSCRIBE" } else { "PING" }
    );
    send(
        &mut stream,
        3,
        Body::Response(Ok(if subscribed {
            Value::Bool(true)
        } else {
            Value::String("PONG".into())
        })),
    )
    .await;
    if action == "closed" {
        if let Some(frame) = receive(&mut stream).await {
            assert!(matches!(frame.body, Body::Request(ref r) if r.command == "QUIT"));
            send(
                &mut stream,
                frame.request_id,
                Body::Response(Ok(Value::Bool(true))),
            )
            .await;
            assert!(receive(&mut stream).await.is_none());
        }
        return 0;
    }
    let count = usize::from(!idle);
    if !idle {
        let target = receive(&mut stream).await.unwrap();
        assert_eq!(target.request_id, 4);
        match (mode, &target.body) {
            ("ordinary", Body::Request(request)) => assert_eq!(request.command, "INCR"),
            ("batch", Body::BatchRequest(requests)) => assert_eq!(requests.len(), 1),
            ("subscription", Body::Ack(_)) => {}
            _ => panic!("wrong target"),
        }
    }
    match action {
        "disconnect" => return 1,
        "truncated" => {
            let bytes = encode_frame(
                &Frame {
                    request_id: 4,
                    body: success(mode),
                },
                Limits::default(),
            )
            .unwrap();
            stream.write_all(&bytes[..19]).await.unwrap();
            return count;
        }
        "old-response" => send(&mut stream, 3, Body::Response(Ok(Value::Bool(true)))).await,
        "old-batch" => {
            send(
                &mut stream,
                3,
                Body::BatchResponse(vec![Ok(Value::Bool(true))]),
            )
            .await
        }
        "heartbeat" => stream.write_all(&fixture_frame("heartbeat")).await.unwrap(),
        "event" => {
            stream.write_all(&fixture_frame("event")).await.unwrap();
            if mode != "subscription" {
                assert!(receive(&mut stream).await.is_none());
                return 1;
            }
            stream.write_all(&fixture_frame("heartbeat")).await.unwrap();
        }
        "zero-response" | "future-response" => {
            let mut bytes = encode_frame(
                &Frame {
                    request_id: 5,
                    body: Body::Response(Ok(Value::UInt(1))),
                },
                Limits::default(),
            )
            .unwrap();
            if action == "zero-response" {
                bytes[6..14].fill(0);
            }
            stream.write_all(&bytes).await.unwrap();
            expect_closed(&mut stream).await;
            return count;
        }
        "zero-error" | "future-error" => {
            send(
                &mut stream,
                if action == "zero-error" { 0 } else { 5 },
                Body::Error(DbError::new(ling_types::ErrorCode::Protocol, "")),
            )
            .await;
            expect_closed(&mut stream).await;
            return count;
        }
        _ => panic!("unknown peer action: {action}"),
    }
    if idle {
        stream.write_all(&fixture_frame("event")).await.unwrap();
    } else {
        send(&mut stream, 4, success(mode)).await;
    }
    expect_closed(&mut stream).await;
    count
}

fn options() -> Options {
    Options {
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        ..Options::default()
    }
}

fn verify(result: &Result<(), Error>, expected: &str, unknown: bool) {
    if expected.starts_with("ok") {
        assert!(result.is_ok(), "{result:?}");
        return;
    }
    let error = result.as_ref().unwrap_err();
    assert_eq!(error.outcome_unknown(), unknown, "{error:?}");
    assert!(
        match (expected, error) {
            (
                "protocol",
                Error::Local {
                    kind: LocalError::Protocol,
                    ..
                },
            )
            | (
                "disconnected",
                Error::Local {
                    kind: LocalError::Disconnected,
                    ..
                },
            )
            | (
                "incomplete",
                Error::Local {
                    kind: LocalError::Disconnected | LocalError::Protocol,
                    ..
                },
            )
            | (
                "closed",
                Error::Local {
                    kind: LocalError::Closed,
                    ..
                },
            )
            | (
                "connect",
                Error::Local {
                    kind: LocalError::Connect | LocalError::Timeout,
                    ..
                },
            ) => true,
            ("ERR_PROTOCOL", Error::Server(error)) => error.code == "ERR_PROTOCOL",
            _ => false,
        },
        "expected {expected}: {error:?}"
    );
}

/// 失效租约不复用，下一次显式调用重新握手且固定数据库为七。
/// A failed lease is discarded; the next explicit call handshakes again in database seven.
async fn pool_case(action: &str) -> usize {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reset = action == "pool-reset";
    let task = tokio::spawn(async move {
        let mut count = 0;
        for attempt in 0..if reset { 2 } else { 1 } {
            let (mut stream, _) = listener.accept().await.unwrap();
            initialize(&mut stream, 7).await;
            let target = receive(&mut stream).await.unwrap();
            assert_eq!(target.request_id, 3);
            assert!(matches!(target.body, Body::Request(ref r) if r.command == "INCR"));
            count += 1;
            if reset && attempt == 0 {
                continue;
            }
            send(&mut stream, 3, Body::Response(Ok(Value::UInt(1)))).await;
            assert!(receive(&mut stream).await.is_none());
        }
        count
    });
    let pool = Pool::new(
        address,
        None,
        7,
        options(),
        PoolOptions {
            max_connections: 1,
            ..PoolOptions::default()
        },
    )
    .unwrap();
    if reset {
        let error = pool
            .execute("INCR", vec![Value::String("counter".into())])
            .await
            .unwrap_err();
        assert!(error.outcome_unknown());
    } else {
        let mut lease = pool.acquire().await.unwrap();
        assert!(
            !lease
                .execute("SELECT", vec![Value::UInt(0)])
                .await
                .unwrap_err()
                .outcome_unknown()
        );
    }
    assert_eq!(
        pool.execute("INCR", vec![Value::String("counter".into())])
            .await
            .unwrap(),
        Value::UInt(1)
    );
    pool.close();
    task.await.unwrap()
}

#[tokio::test]
async fn shared_session_contracts() {
    for line in include_str!("../../../tests/fixtures/protocol/sessions.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        let row: Vec<_> = line.split('|').collect();
        let (id, mode, action) = (row[0], row[1], row[2]);
        let count = if mode == "pool" {
            pool_case(action).await
        } else {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            if action == "refused" {
                drop(listener);
                let result = Client::connect(address, None, 0, options())
                    .await
                    .map(|_| ());
                verify(&result, row[3], row[4] == "true");
                println!("LING_CONTRACT\tsessions\t{id}\tpassed");
                continue;
            }
            let owned_mode = mode.to_string();
            let owned_action = action.to_string();
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let count = peer(stream, &owned_mode, &owned_action).await;
                if matches!(owned_action.as_str(), "disconnect" | "truncated") {
                    // 保留监听器以观察偷偷重连，而非靠拒绝连接掩盖重放。
                    // Keep listening to observe an implicit reconnect instead of hiding replay behind refusal.
                    assert!(
                        timeout(Duration::from_millis(150), listener.accept())
                            .await
                            .is_err()
                    );
                }
                count
            });
            let mut client = Client::connect(address, None, 0, options()).await.unwrap();
            let (result, closed) = if mode.ends_with("subscription") {
                let mut subscriber = client
                    .subscribe(vec!["*".into()], Some("sessions"))
                    .await
                    .unwrap();
                let result = if mode == "idle-subscription" {
                    subscriber.next_event().await.map(|event| {
                        assert_eq!(event.id.sequence, 1);
                    })
                } else {
                    subscriber
                        .ack(EventId {
                            stream_id: 1,
                            db: 0,
                            shard: 0,
                            sequence: 1,
                        })
                        .await
                };
                if row[3] == "ok-event" && mode != "idle-subscription" {
                    assert_eq!(subscriber.next_event().await.unwrap().id.sequence, 1);
                }
                let closed = subscriber.is_closed();
                drop(subscriber);
                (result, closed)
            } else {
                client.ping(None).await.unwrap();
                let result = if action == "closed" {
                    client.close().await.unwrap();
                    client
                        .execute("INCR", vec![Value::String("counter".into())])
                        .await
                        .map(|_| ())
                } else if mode == "batch" {
                    client
                        .batch(vec![Request::new(
                            "INCR",
                            vec![Value::String("counter".into())],
                        )])
                        .await
                        .map(|values| {
                            assert_eq!(values, vec![Ok(Value::UInt(1))]);
                        })
                } else {
                    client
                        .execute("INCR", vec![Value::String("counter".into())])
                        .await
                        .map(|value| {
                            assert_eq!(value, Value::UInt(1));
                        })
                };
                let closed = client.is_closed();
                drop(client);
                (result, closed)
            };
            verify(&result, row[3], row[4] == "true");
            assert_eq!(closed, row[5] == "true", "{id}");
            task.await.unwrap()
        };
        assert_eq!(count, row[6].parse::<usize>().unwrap(), "{id}");
        println!("LING_CONTRACT\tsessions\t{id}\tpassed");
    }
}
