//! 真实服务验证池容量、固定库上下文和取消后的资源归还。
//! Real-server contracts for pool capacity, fixed database context, and cancellation cleanup.

use ling_client::{Options, Pool, PoolOptions, Value};
use ling_server::{Config, Server};
use std::time::Duration;
use tokio::{net::TcpListener, sync::oneshot};

/// 容量含租约，切库在发送前拒绝，关闭唤醒等待者。
/// Capacity includes leases, database changes fail before sending, and closing wakes waiters.
#[tokio::test]
async fn fixed_database_capacity_and_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, Config::default()).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let options = PoolOptions {
        max_connections: 1,
        acquire_timeout: Duration::from_millis(20),
    };
    let a = Pool::new(address, None, 0, Options::default(), options.clone()).unwrap();
    let b = Pool::new(address, None, 1, Options::default(), options).unwrap();
    a.execute(
        "SET",
        vec![Value::String("same".into()), Value::UInt(u64::MAX)],
    )
    .await
    .unwrap();
    assert!(
        b.execute("GET", vec![Value::String("same".into())])
            .await
            .is_err()
    );
    let mut lease = a.acquire().await.unwrap();
    assert!(lease.execute("SELECT", vec![Value::UInt(1)]).await.is_err());
    let error = a.acquire().await.err().unwrap();
    assert!(!error.outcome_unknown());
    drop(lease);
    assert_eq!(
        a.execute("GET", vec![Value::String("same".into())])
            .await
            .unwrap(),
        Value::UInt(u64::MAX)
    );
    let lease = a.acquire().await.unwrap();
    let queued = a.clone();
    let waiter = tokio::spawn(async move { queued.acquire().await.is_err() });
    a.close();
    assert!(waiter.await.unwrap());
    drop(lease);
    assert!(a.execute("PING", vec![]).await.is_err());
    b.close();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

/// 请求取消使连接失效，归还后下一调用重新完整初始化。
/// Cancelling a request invalidates its connection; the next call initializes a replacement fully.
#[tokio::test]
async fn cancelled_acquire_releases_capacity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let pool = Pool::new(
        address,
        None,
        0,
        Options::default(),
        PoolOptions {
            max_connections: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let clone = pool.clone();
    let pending = tokio::spawn(async move { clone.acquire().await.is_ok() });
    let (socket, _) = listener.accept().await.unwrap();
    pending.abort();
    let _ = pending.await;
    drop(socket);
    let clone = pool.clone();
    let pending = tokio::spawn(async move { clone.acquire().await.is_ok() });
    let (socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    pool.close();
    assert!(!pending.await.unwrap());
    drop(socket);
}
