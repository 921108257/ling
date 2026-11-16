//! 控制面真实网络验收：重载整体拒绝、脱敏、指标、批量预算和关闭。
//! Real-network operations contracts: atomic reload rejection, redaction, metrics, batch budgets, and shutdown.

use ling_client::{Client, Options, Request, Value};
use ling_server::{Config, Server};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// 测试文件仅属于当前随机目录；Files belong exclusively to a random test directory.
struct Directory(PathBuf);
impl Drop for Directory {
    /// 测试失败也清理配置；Cleans configuration even if the test fails.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
/// 重载只应用日志级别，静态项变化不得部分生效；关闭先应答再排空。
/// Reload only applies log level, rejects static changes without partial effects, and replies before shutdown.
#[tokio::test]
async fn configuration_metrics_and_shutdown() {
    let dir =
        Directory(std::env::temp_dir().join(format!("linger-ops-{:032x}", rand::random::<u128>())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let metrics = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = metrics.local_addr().unwrap().port();
    drop(metrics);
    let path = dir.0.join("server.toml");
    let source = format!(
        "[engine]\nshard_count=2\n[auth]\nenabled=true\npassword=\"never-expose\"\n[metrics]\nenabled=true\nport={port}\n[log]\nlevel=\"warn\"\n"
    );
    std::fs::write(&path, &source).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, Config::load(&path).unwrap()).unwrap();
    let task = tokio::spawn(server.run(std::future::pending()));
    let mut client = Client::connect(address, Some("never-expose"), 0, Options::default())
        .await
        .unwrap();
    let get = |s: &str| vec![Value::String("get".into()), Value::String(s.into())];
    let query = client.execute("CONFIG", get("*")).await.unwrap();
    assert!(!format!("{query:?}").contains("never-expose"));
    let Value::Map(config) = query else { panic!() };
    assert_eq!(
        config.get("auth.password"),
        Some(&Value::String("<redacted>".into()))
    );
    assert_eq!(config.get("engine.shard_count"), Some(&Value::UInt(2)));
    std::fs::write(&path, source.replace("warn", "debug")).unwrap();
    client
        .execute("CONFIG", vec![Value::String("RELOAD".into())])
        .await
        .unwrap();
    let level = client.execute("CONFIG", get("log.level")).await.unwrap();
    assert_eq!(
        level,
        Value::Map(std::collections::BTreeMap::from([(
            "log.level".into(),
            Value::String("debug".into())
        )]))
    );
    std::fs::write(
        &path,
        source
            .replace("warn", "error")
            .replace("shard_count=2", "shard_count=3"),
    )
    .unwrap();
    let error = client
        .execute("CONFIG", vec![Value::String("RELOAD".into())])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ERR_CONFIG_IMMUTABLE"));
    assert_eq!(
        client.execute("CONFIG", get("log.level")).await.unwrap(),
        level
    );
    client
        .execute(
            "SET",
            vec![
                Value::String("secret-key".into()),
                Value::String("secret-value".into()),
            ],
        )
        .await
        .unwrap();
    let info = client.execute("INFO", vec![]).await.unwrap();
    assert!(!format!("{info:?}").contains("secret-"));
    let Value::Map(memory) = client
        .execute("INFO", vec![Value::String("MEMORY".into())])
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(memory.contains_key("memory"));
    assert!(
        client
            .execute("INFO", vec![Value::String("invalid".into())])
            .await
            .unwrap_err()
            .to_string()
            .contains("ERR_INVALID_ARGUMENT")
    );
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut text = String::new();
    socket.read_to_string(&mut text).await.unwrap();
    assert!(text.starts_with("HTTP/1.1 200"));
    assert!(text.contains("linger_requests_total"));
    assert!(!text.contains("secret-"));
    assert_eq!(
        client.execute("SHUTDOWN", vec![]).await.unwrap(),
        Value::Bool(true)
    );
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(TcpStream::connect(address).await.is_err());
}

/// 批量先验证白名单，错误不回滚已完成项，响应不足不能先弹出再失败。
/// Batches prevalidate admission, preserve completed items, and never pop before response-budget failure.
#[tokio::test]
async fn batch_admission_partial_results_and_pop_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = Config::default();
    config.server.max_response_bytes = 2048;
    let server = Server::from_listener(listener, config).unwrap();
    let task = tokio::spawn(server.run(std::future::pending()));
    let mut client = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let key = Value::String("batch-key".into());
    assert!(
        client
            .batch(vec![
                Request::new("SET", vec![key.clone(), Value::Null]),
                Request::new("SELECT", vec![Value::UInt(1)])
            ])
            .await
            .is_err()
    );
    assert!(client.execute("GET", vec![key.clone()]).await.is_err());
    let results = client
        .batch(vec![
            Request::new("SET", vec![key.clone(), Value::Null]),
            Request::new("INCR", vec![key.clone()]),
            Request::new("GET", vec![key.clone()]),
        ])
        .await
        .unwrap();
    assert_eq!(results[0], Ok(Value::Bool(true)));
    assert!(results[1].is_err());
    assert_eq!(results[2], Ok(Value::Null));
    let large = Value::Bytes(vec![42; 1950]);
    client
        .execute("SET", vec![key.clone(), Value::List(vec![large])])
        .await
        .unwrap();
    let mut requests = vec![Request::new("LPOP", vec![key.clone()])];
    requests.extend((0..8).map(|_| Request::new("GET", vec![Value::String("absent".into())])));
    let results = client.batch(requests).await.unwrap();
    assert_eq!(results[0].as_ref().unwrap_err().code, "ERR_LIMIT_EXCEEDED");
    assert_eq!(
        client.execute("LLEN", vec![key]).await.unwrap(),
        Value::UInt(1)
    );
    client.execute("SHUTDOWN", vec![]).await.unwrap();
    task.await.unwrap().unwrap();
}
