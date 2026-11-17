//! 真实 TCP 资源耗尽与恢复验收；仅耗尽实例配置预算，不耗尽宿主机资源。
//! Real TCP resource exhaustion and recovery; exhausts instance budgets, never host resources.

use ling_client::{Client, Error, Options, Value};
use ling_protocol::{Body, Frame, Limits, Request, encode_frame, read_frame, write_frame};
use ling_server::{Config, Server};
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// 独立服务任务及退出通道；Isolated service task and shutdown channel.
async fn start(
    config: Config,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, config).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    (address, stop, task)
}

/// 核对稳定服务端错误码，不能把断线当作预算拒绝。
/// Checks a stable server code rather than accepting a disconnected socket as budget rejection.
fn code(error: Error, expected: &str) {
    let Error::Server(error) = error else {
        panic!("Expected server error")
    };
    assert_eq!(error.code, expected);
}

/// 配额拒绝不修改已有值，DEL 后归还预算；五种淘汰策略均保持预算。
/// Quota rejection preserves existing values and DEL returns budget; every eviction policy stays bounded.
#[tokio::test]
async fn bounded_memory_eviction_and_recovery() {
    for policy in ["noeviction", "lru", "lfu", "random", "ttl-first"] {
        let mut config = Config::default();
        config.database.databases = 1;
        config.engine.shard_count = 2;
        config.engine.max_memory_bytes = 32768;
        config.engine.max_keys = 8;
        config.eviction.policy = policy.into();
        config.events.enabled = false;
        let (address, stop, task) = start(config).await;
        let mut client = Client::connect(address, None, 0, Options::default())
            .await
            .unwrap();
        let mut rejected = false;
        for i in 0..100 {
            let mut args = vec![
                Value::String(format!("key:{i}")),
                Value::Bytes(vec![42; 1024]),
            ];
            if policy == "ttl-first" {
                args.extend([Value::String("PX".into()), Value::Int(60000)]);
            }
            match client.execute("SET", args).await {
                Ok(value) => assert_eq!(value, Value::Bool(true)),
                Err(error) => {
                    code(error, "ERR_OUT_OF_MEMORY");
                    rejected = true;
                    break;
                }
            }
        }
        assert_eq!(rejected, policy == "noeviction");
        let Value::UInt(keys) = client.execute("DBSIZE", vec![]).await.unwrap() else {
            panic!()
        };
        assert!(keys <= 8 && keys > 0);
        let Value::Map(info) = client
            .execute("INFO", vec![Value::String("memory".into())])
            .await
            .unwrap()
        else {
            panic!()
        };
        let Value::Map(memory) = &info["memory"] else {
            panic!()
        };
        let Value::UInt(bytes) = memory["managed_bytes"] else {
            panic!()
        };
        assert!(bytes <= 32768);
        // 无法容纳的替换不能提前淘汰其他条目；An oversized replacement cannot evict other entries first.
        code(
            client
                .execute(
                    "SET",
                    vec![
                        Value::String("oversized".into()),
                        Value::Bytes(vec![0; 65536]),
                    ],
                )
                .await
                .unwrap_err(),
            "ERR_OUT_OF_MEMORY",
        );
        assert_eq!(
            client.execute("DBSIZE", vec![]).await.unwrap(),
            Value::UInt(keys)
        );
        if policy == "noeviction" {
            assert_eq!(
                client
                    .execute("GET", vec![Value::String("key:0".into())])
                    .await
                    .unwrap(),
                Value::Bytes(vec![42; 1024])
            );
        }
        client.execute("FLUSHDB", vec![]).await.unwrap();
        client
            .execute("SET", vec![Value::String("after".into()), Value::Null])
            .await
            .unwrap();
        assert_eq!(
            client
                .execute("GET", vec![Value::String("after".into())])
                .await
                .unwrap(),
            Value::Null
        );
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

/// 接收半帧占满连接预算后超时释放；超大帧在读负载前拒绝。
/// Partial frames fill connection capacity then time out; oversized frames fail before reading payloads.
#[tokio::test]
async fn connection_and_frame_budgets_recover() {
    let mut config = Config::default();
    config.server.max_connections = 4;
    config.server.request_timeout_ms = 150;
    config.server.max_request_bytes = 1024;
    config.events.enabled = false;
    let (address, stop, task) = start(config).await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(b"L").await.unwrap();
        held.push(socket);
    }
    for socket in &mut held {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            read_frame(socket, Limits::default()),
        )
        .await
        .unwrap();
        assert!(matches!(result, Ok(None) | Err(_)));
    }
    let mut client = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    assert_eq!(
        client.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    let mut raw = TcpStream::connect(address).await.unwrap();
    let hello = Frame {
        request_id: 1,
        body: Body::Request(Request::new("HELLO", vec![Value::UInt(1)])),
    };
    write_frame(&mut raw, &hello, Limits::default())
        .await
        .unwrap();
    read_frame(&mut raw, Limits::default())
        .await
        .unwrap()
        .unwrap();
    let mut header = encode_frame(&hello, Limits::default()).unwrap()[..18].to_vec();
    header[6..14].copy_from_slice(&2u64.to_be_bytes());
    header[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
    raw.write_all(&header).await.unwrap();
    let frame = tokio::time::timeout(
        Duration::from_secs(1),
        read_frame(&mut raw, Limits::default()),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(frame.body, Body::Error(_)));
    assert_eq!(
        client.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

/// 测试 Journal 生命周期仅清理本次临时目录。
/// Journal fixture lifecycle removes only this test's temporary directory.
#[cfg(unix)]
struct Directory(PathBuf);
#[cfg(unix)]
impl Drop for Directory {
    /// 测试失败时也清理；Cleans up after failed tests too.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 日志容量耗尽保留最后成功值，预留 ACK 可用；慢组窗口保持有界。
/// Journal exhaustion preserves the last committed value and ACK reserve; slow-group windows stay bounded.
#[cfg(unix)]
#[tokio::test]
async fn journal_full_preserves_state_and_ack_capacity() {
    let dir = Directory(
        std::env::temp_dir().join(format!("linger-resource-{:032x}", rand::random::<u128>())),
    );
    let mut config = Config::default();
    config.database.databases = 1;
    config.engine.shard_count = 1;
    config.events.mode = "reliable".into();
    config.events.max_pending_events = 4;
    config.event_journal.dir = dir.0.clone();
    config.event_journal.max_bytes = 1024 * 1024;
    let (address, stop, task) = start(config).await;
    let mut writer = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let mut subscriber = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap()
        .subscribe(vec!["*".into()], Some("slow"))
        .await
        .unwrap();
    let key = Value::String("large-key:".to_owned() + &"x".repeat(990));
    let mut last = 0;
    let mut full = false;
    for n in 1..1000 {
        match writer
            .execute("SET", vec![key.clone(), Value::Int(n)])
            .await
        {
            Ok(_) => last = n,
            Err(error) => {
                code(error, "ERR_JOURNAL_FULL");
                full = true;
                break;
            }
        }
    }
    assert!(full && last > 0);
    assert_eq!(
        writer.execute("GET", vec![key]).await.unwrap(),
        Value::Int(last)
    );
    let event = subscriber.next_event().await.unwrap();
    subscriber.ack(event.id).await.unwrap();
    let Value::Map(info) = writer
        .execute("INFO", vec![Value::String("events".into())])
        .await
        .unwrap()
    else {
        panic!()
    };
    let Value::Map(events) = &info["events"] else {
        panic!()
    };
    let Value::UInt(pending) = events["pending"] else {
        panic!()
    };
    assert!(pending <= 4);
    let Value::UInt(bytes) = events["journal_bytes"] else {
        panic!()
    };
    assert!(bytes <= 1024 * 1024 / 2);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

/// 慢读端不能永久占用连接；写超时回收后其他客户端继续工作。
/// A stalled reader cannot retain a connection forever; write timeout reclaims it while peers keep working.
#[tokio::test]
async fn stalled_reader_releases_connection_without_blocking_peers() {
    let mut config = Config::default();
    config.server.max_connections = 4;
    config.server.request_timeout_ms = 100;
    config.server.write_buffer_bytes = 1024;
    config.events.enabled = false;
    let (address, stop, task) = start(config).await;
    let mut client = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    client
        .execute(
            "SET",
            vec![
                Value::String("large".into()),
                Value::Bytes(vec![17; 512 * 1024]),
            ],
        )
        .await
        .unwrap();
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1024).unwrap();
    let mut slow = socket.connect(address).await.unwrap();
    write_frame(
        &mut slow,
        &Frame {
            request_id: 1,
            body: Body::Request(Request::new("HELLO", vec![Value::UInt(1)])),
        },
        Limits::default(),
    )
    .await
    .unwrap();
    read_frame(&mut slow, Limits::default())
        .await
        .unwrap()
        .unwrap();
    for id in 2..66 {
        write_frame(
            &mut slow,
            &Frame {
                request_id: id,
                body: Body::Request(Request::new("GET", vec![Value::String("large".into())])),
            },
            Limits::default(),
        )
        .await
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Value::Map(info) = client
                .execute("INFO", vec![Value::String("clients".into())])
                .await
                .unwrap()
            else {
                panic!()
            };
            let Value::Map(clients) = &info["clients"] else {
                panic!()
            };
            if clients["connected"] == Value::UInt(1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
