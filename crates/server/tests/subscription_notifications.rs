//! 真实 TCP 的通知派发、兼容开关、ACK 窗口与租约唤醒契约。
//! Real TCP contracts for notification delivery, legacy polling, ACK windows, and lease wakeups.

use ling_client::{Client, Options, Value};
use ling_protocol::{Body, Frame, Limits, Request, read_frame, write_frame};
use ling_server::{Config, Server};
use std::time::Duration;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// 每项测试拥有临时端口及显式退出通道。
/// Each test owns an ephemeral port and an explicit shutdown channel.
async fn start(
    config: Config,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, config).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    (address, stop, task)
}

/// 从服务端既有生命周期计数读取派发次数，不以 CPU 或时延推测空轮询。
/// Reads existing dispatch counters instead of inferring empty polls from CPU or latency.
async fn polls(client: &mut Client) -> u64 {
    let Value::Map(info) = client
        .execute("INFO", vec![Value::String("events".into())])
        .await
        .unwrap()
    else {
        panic!()
    };
    let Value::Map(events) = &info["events"] else {
        panic!()
    };
    let Value::UInt(polls) = events["subscription_polls"] else {
        panic!()
    };
    polls
}

/// 订阅追平后默认路径没有周期派发，关闭开关保留旧的 10 ms 行为。
/// The default path has no periodic idle dispatch; disabling it retains the legacy 10 ms behavior.
#[tokio::test]
async fn idle_notifications_do_not_dispatch_and_legacy_polling_still_does() {
    for enabled in [true, false] {
        let mut config = Config::default();
        config.events.notification_enabled = enabled;
        let (address, stop, task) = start(config).await;
        let mut writer = Client::connect(address, None, 0, Options::default())
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute(
                    "CONFIG",
                    vec![
                        Value::String("GET".into()),
                        Value::String("events.notification_enabled".into())
                    ]
                )
                .await
                .unwrap(),
            Value::Map(std::collections::BTreeMap::from([(
                "events.notification_enabled".into(),
                Value::Bool(enabled)
            )]))
        );
        let subscriber = Client::connect(address, None, 0, Options::default())
            .await
            .unwrap();
        let mut subscriber = subscriber
            .subscribe(vec!["*".into()], Some("idle"))
            .await
            .unwrap();
        // 等初次检查真实完成再测空闲，避免把订阅初始化误记为周期派发。
        // Wait for the actual initial check so initialization is not counted as periodic work.
        tokio::time::timeout(Duration::from_secs(2), async {
            while polls(&mut writer).await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let before = polls(&mut writer).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after = polls(&mut writer).await;
        if enabled {
            assert_eq!(after, before);
        } else {
            assert!(after >= before + 3);
        }
        subscriber.close().await.unwrap();
        writer.close().await.unwrap();
        stop.send(()).unwrap();
        task.await.unwrap();
    }
}

/// 满窗口在 ACK 后立即恢复，新消费者在断开后的准确重投期限继续获取原 ID。
/// ACKs reopen a full window, and a new consumer receives the original ID after a disconnected lease becomes due.
#[tokio::test]
async fn acknowledgements_and_disconnect_wake_shared_group() {
    let mut config = Config::default();
    config.events.max_pending_events = 1;
    config.events.retry_interval_ms = 60;
    config.events.consumer_timeout_ms = 1000;
    let (address, stop, task) = start(config).await;
    let mut writer = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let one = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let mut one = one
        .subscribe(vec!["*".into()], Some("shared"))
        .await
        .unwrap();
    for key in ["a", "b"] {
        writer
            .execute("SET", vec![Value::String(key.into()), Value::Null])
            .await
            .unwrap();
    }
    let first = tokio::time::timeout(Duration::from_secs(2), one.next_event())
        .await
        .unwrap()
        .unwrap();
    one.ack(first.id).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), one.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.id, second.id);
    let two = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let mut two = two
        .subscribe(vec!["*".into()], Some("shared"))
        .await
        .unwrap();
    one.close().await.unwrap();
    let retried = tokio::time::timeout(Duration::from_secs(2), two.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retried.id, second.id);
    two.ack(retried.id).await.unwrap();
    two.close().await.unwrap();
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 已有组积压也必须先收到订阅响应，随后才能收到事件。
/// Even a group with a backlog receives its subscribe response before any event.
#[tokio::test]
async fn subscribe_response_precedes_backlog_events() {
    let (address, stop, task) = start(Config::default()).await;
    let mut writer = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let seed = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let mut seed = seed
        .subscribe(vec!["*".into()], Some("backlog"))
        .await
        .unwrap();
    seed.close().await.unwrap();
    writer
        .execute("SET", vec![Value::String("queued".into()), Value::Null])
        .await
        .unwrap();
    let mut stream = TcpStream::connect(address).await.unwrap();
    for (id, name, arguments) in [
        (1, "HELLO", vec![Value::UInt(1)]),
        (
            2,
            "SUBSCRIBE",
            vec![
                Value::List(vec![Value::String("*".into())]),
                Value::String("GROUP".into()),
                Value::String("backlog".into()),
            ],
        ),
    ] {
        write_frame(
            &mut stream,
            &Frame {
                request_id: id,
                body: Body::Request(Request::new(name, arguments)),
            },
            Limits::default(),
        )
        .await
        .unwrap();
        let response = read_frame(&mut stream, Limits::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.request_id, id);
        assert!(matches!(response.body, Body::Response(Ok(_))));
    }
    let event = tokio::time::timeout(
        Duration::from_secs(2),
        read_frame(&mut stream, Limits::default()),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(event.request_id, 0);
    assert!(matches!(event.body, Body::Event(_)));
    drop(stream);
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}
