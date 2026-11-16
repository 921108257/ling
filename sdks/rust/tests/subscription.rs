//! 真实 TCP 订阅、ACK 恢复与取消契约；缓存冷启动始终为空。
//! Real TCP subscriptions, ACK recovery, and cancellation contracts; cold caches always start empty.

use ling_client::{Client, Event, EventKind, Options, Value};
use ling_server::{Config, Server};
use std::{fs, path::PathBuf, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};

/// 临时持久目录；Temporary persistence directory.
struct Directory(PathBuf);
impl Directory {
    /// 并行测试隔离；Parallel-test isolation.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "linger-sdk-events-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )))
    }
}
impl Drop for Directory {
    /// 只清理测试自建目录；Only cleans the test-created directory.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// 启动固定拓扑服务，可复用同一 Journal；Starts fixed topology, optionally reusing a Journal.
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
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (address, tx, task)
}
/// 已初始化的普通连接；Initialized ordinary connection.
async fn client(address: std::net::SocketAddr) -> Client {
    Client::connect(address, None, 0, Options::default())
        .await
        .unwrap()
}
/// 将失败挂起限制在测试期限内；Bounds stalled test failures.
async fn next(sub: &mut ling_client::Subscriber) -> Event {
    tokio::time::timeout(Duration::from_secs(2), sub.next_event())
        .await
        .unwrap()
        .unwrap()
}

/// 真实重启恢复 ACK 空洞但不恢复缓存。
/// Real restarts recover ACK holes without restoring cached values.
#[tokio::test]
async fn reliable_ack_holes_recover_and_cache_starts_empty() {
    let dir = Directory::new();
    let mut config = Config::default();
    config.engine.shard_count = 1;
    config.events.mode = "reliable".into();
    // 本例验证重启后的 ACK 空洞恢复，避免慢磁盘把初次投递与合法的超时重投交错。
    // This tests ACK-hole recovery after restart, not retry timing on a slow CI disk.
    config.events.consumer_timeout_ms = 30_000;
    config.events.retry_interval_ms = 30_000;
    config.event_journal.dir = dir.0.clone();
    let (address, stop, task) = start(config.clone()).await;
    let mut writer = client(address).await;
    let mut sub = client(address)
        .await
        .subscribe(vec!["*".into()], Some("workers"))
        .await
        .unwrap();
    let mut independent = client(address)
        .await
        .subscribe(vec!["*".into()], Some("audit"))
        .await
        .unwrap();
    for key in ["a", "b", "c"] {
        writer
            .execute("SET", vec![Value::String(key.into()), Value::Null])
            .await
            .unwrap();
    }
    let first = next(&mut sub).await;
    let second = next(&mut sub).await;
    assert_eq!((first.key.as_str(), second.key.as_str()), ("a", "b"));
    sub.ack(second.id).await.unwrap();
    assert_eq!(next(&mut independent).await.id, first.id);
    drop(sub);
    drop(independent);
    writer.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();

    let (address, stop, task) = start(config).await;
    let mut writer = client(address).await;
    assert_eq!(
        writer.execute("DBSIZE", vec![]).await.unwrap(),
        Value::UInt(0)
    );
    let mut sub = client(address)
        .await
        .subscribe(vec!["*".into()], Some("workers"))
        .await
        .unwrap();
    let replay = next(&mut sub).await;
    assert_eq!(replay.id, first.id);
    sub.ack(replay.id).await.unwrap();
    let third = next(&mut sub).await;
    assert_eq!(third.key, "c");
    sub.ack(third.id).await.unwrap();
    writer
        .execute("SET", vec![Value::String("d".into()), Value::Null])
        .await
        .unwrap();
    let fourth = next(&mut sub).await;
    assert_eq!(fourth.id.sequence, 4);
    assert_eq!(fourth.id.stream_id, first.id.stream_id);
    assert_eq!(fourth.kind, EventKind::Created);
    sub.ack(fourth.id).await.unwrap();
    sub.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 取消释放连接，ACK 错误不被隐藏。
/// Cancellation releases connections and ACK errors remain explicit.
#[tokio::test]
async fn cancellation_releases_connection_and_old_ack_errors_remain_explicit() {
    let mut config = Config::default();
    config.engine.shard_count = 1;
    let (address, stop, task) = start(config).await;
    let mut sub = client(address)
        .await
        .subscribe(vec!["match:*".into()], Some("g"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), sub.next_event())
            .await
            .is_err()
    );
    assert!(sub.is_closed());
    let mut sub = client(address)
        .await
        .subscribe(vec!["match:*".into()], Some("g"))
        .await
        .unwrap();
    let mut writer = client(address).await;
    writer
        .execute("SET", vec![Value::String("ignored".into()), Value::Null])
        .await
        .unwrap();
    writer
        .execute("SET", vec![Value::String("match:key".into()), Value::Null])
        .await
        .unwrap();
    let event = next(&mut sub).await;
    let wrong = ling_client::EventId {
        stream_id: event.id.stream_id ^ 1,
        ..event.id
    };
    assert!(
        sub.ack(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("ERR_STREAM_MISMATCH")
    );
    assert!(!sub.is_closed());
    sub.ack(event.id).await.unwrap();
    sub.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap();
}

/// 旧流身份在建立组前显式校验，错误不会静默重置消费位置。
/// Saved stream identities are checked before creating groups, without silently resetting consumption.
#[tokio::test]
async fn saved_stream_mismatch_closes_before_subscribing() {
    let (address, stop, task) = start(Config::default()).await;
    let mut client = client(address).await;
    let Some(Value::Bytes(bytes)) = client.capabilities().get("stream_id") else {
        panic!()
    };
    let stream = u128::from_be_bytes(bytes.as_slice().try_into().unwrap());
    client.require_event_stream(stream).unwrap();
    assert!(
        client
            .require_event_stream(stream ^ 1)
            .unwrap_err()
            .to_string()
            .contains("ERR_STREAM_MISMATCH")
    );
    assert!(client.is_closed());
    stop.send(()).unwrap();
    task.await.unwrap();
}
