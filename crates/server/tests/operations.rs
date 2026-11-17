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
    assert_info_fields(&info);
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
    assert_observability_metrics(&text);
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

/// 同一字段向量校验 INFO 类型与直方图结构，UInt 不经 JSON 转换。
/// Shared field vectors verify INFO types and histogram structure without converting UInt through JSON.
fn assert_info_fields(info: &Value) {
    let Value::Map(sections) = info else {
        panic!("INFO must be a Map")
    };
    for line in include_str!("../../../tests/fixtures/protocol/info-fields.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let fields: Vec<_> = line.split('|').collect();
        let Value::Map(section) = &sections[fields[0]] else {
            panic!("section must be a Map")
        };
        let value = &section[fields[1]];
        let valid = match fields[2] {
            "UInt" => matches!(value, Value::UInt(_)),
            "Bool" => matches!(value, Value::Bool(_)),
            "Float" => matches!(value, Value::Float(_)),
            "UIntOrNull" => matches!(value, Value::UInt(_) | Value::Null),
            "FloatOrNull" => matches!(value, Value::Float(_) | Value::Null),
            "Histogram" => {
                let Value::Map(histogram) = value else {
                    panic!("histogram must be a Map")
                };
                let (
                    Value::List(bounds),
                    Value::List(buckets),
                    Value::UInt(count),
                    Value::UInt(_sum),
                ) = (
                    &histogram["bounds"],
                    &histogram["buckets"],
                    &histogram["count"],
                    &histogram["sum"],
                )
                else {
                    panic!("invalid histogram fields")
                };
                assert_eq!(bounds.len(), 6);
                assert_eq!(buckets.len(), 7);
                let total = buckets
                    .iter()
                    .map(|value| match value {
                        Value::UInt(value) => *value,
                        _ => panic!(),
                    })
                    .sum::<u64>();
                assert_eq!(*count, total);
                true
            }
            _ => panic!("unknown fixture type"),
        };
        assert!(valid, "{line}: {value:?}");
    }
}

/// 对外名称、类型与固定桶以文本契约冻结，不产生业务标签。
/// Public names, types, and fixed buckets are frozen as text contracts without business labels.
fn assert_observability_metrics(text: &str) {
    for (name, kind) in [
        ("execution_wait_seconds", "histogram"),
        ("execution_occupied", "gauge"),
        ("execution_queued", "gauge"),
        ("execution_wait_timeouts_total", "counter"),
        ("execution_wait_cancelled_total", "counter"),
        ("maintenance_cycles_total", "counter"),
        ("maintenance_reclaimed_total", "counter"),
        ("maintenance_deferred_total", "counter"),
        ("accept_errors_total", "counter"),
        ("connections_rejected_total", "counter"),
        ("connection_panics_total", "counter"),
        ("first_frame_timeouts_total", "counter"),
        ("auth_failures_total", "counter"),
        ("subscriptions", "gauge"),
        ("subscription_polls_total", "counter"),
        ("subscription_empty_polls_total", "counter"),
        ("subscription_empty_poll_ratio", "gauge"),
        ("journal_fsync_duration_seconds", "histogram"),
        ("journal_transaction_bytes", "histogram"),
        ("journal_compaction_duration_seconds", "histogram"),
        ("journal_compactions_total", "counter"),
        ("journal_compaction_successes_total", "counter"),
        ("journal_compaction_failures_total", "counter"),
        ("process_rss_available", "gauge"),
        ("process_cpu_available", "gauge"),
    ] {
        assert!(
            text.contains(&format!("# TYPE linger_{name} {kind}\n")),
            "missing {name}"
        );
    }
    for bound in ["0.00001", "0.0001", "0.001", "0.01", "0.1", "1", "+Inf"] {
        assert!(text.contains(&format!(
            "linger_execution_wait_seconds_bucket{{le=\"{bound}\"}}"
        )));
    }
    for bound in ["256", "1024", "4096", "16384", "65536", "1048576", "+Inf"] {
        assert!(text.contains(&format!(
            "linger_journal_transaction_bytes_bucket{{le=\"{bound}\"}}"
        )));
    }
    assert!(text.contains("linger_accept_errors_total{listener=\"data\"}"));
    assert!(text.contains("linger_accept_errors_total{listener=\"metrics\"}"));
}

/// 真实连接核对计时禁用仍有廉价计数、订阅数与实际空轮询口径。
/// Real connections verify disabled timing keeps cheap counters and tracks subscriptions and actual empty polls.
#[tokio::test]
async fn disabled_timing_and_subscription_metrics() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, Config::default()).unwrap();
    let task = tokio::spawn(server.run(std::future::pending()));
    let mut client = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    let mut subscriber = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap()
        .subscribe(vec!["*".into()], None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let info = client.execute("INFO", vec![]).await.unwrap();
            assert_info_fields(&info);
            let Value::Map(sections) = info else { panic!() };
            let Value::Map(stats) = &sections["stats"] else {
                panic!()
            };
            assert_eq!(stats["timing_enabled"], Value::Bool(false));
            for name in ["request_duration_us", "execution_wait_us"] {
                let Value::Map(histogram) = &stats[name] else {
                    panic!()
                };
                assert_eq!(histogram["count"], Value::UInt(0));
                assert_eq!(histogram["sum"], Value::UInt(0));
            }
            let Value::Map(clients) = &sections["clients"] else {
                panic!()
            };
            assert_eq!(clients["subscriptions"], Value::UInt(1));
            let Value::Map(events) = &sections["events"] else {
                panic!()
            };
            let Value::Float(ratio) = events["subscription_empty_poll_ratio"] else {
                panic!()
            };
            assert!((0.0..=1.0).contains(&ratio.get()));
            // 字段不是原子快照，等到本次空轮询的两个计数均可见。
            // Fields are not an atomic snapshot; wait until both counters expose this empty poll.
            if events["subscription_polls"] != Value::UInt(0)
                && events["subscription_polls"] == events["subscription_empty_polls"]
                && ratio.get() == 1.0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    subscriber.close().await.unwrap();
    let Value::Map(sections) = client
        .execute("INFO", vec![Value::String("clients".into())])
        .await
        .unwrap()
    else {
        panic!()
    };
    let Value::Map(clients) = &sections["clients"] else {
        panic!()
    };
    assert_eq!(clients["subscriptions"], Value::UInt(0));
    client.execute("SHUTDOWN", vec![]).await.unwrap();
    task.await.unwrap().unwrap();
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
