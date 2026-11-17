//! 外部驱动提供服务端的 Rust 互操作入口，不在普通 workspace 测试中静默跳过。
//! Rust interoperability entry point using a harness-supplied server, without silent workspace skips.

use ling_client::{Client, ClientTls, Error, Options, Pool, PoolOptions, Request, Value};
use ling_protocol::{Limits, decode_frame, decode_value, encode_frame, encode_value};
use std::{net::SocketAddr, path::Path, time::Duration};

/// 即使断言 panic 也输出当前失败 ID，驱动另外检测尚未执行的 ID。
/// Reports the active failed ID on panic; the harness separately detects unexecuted IDs.
struct Case<'a> {
    scope: &'a str,
    id: &'a str,
    passed: bool,
}
impl<'a> Case<'a> {
    /// 记录当前用例；Records the active case.
    fn new(scope: &'a str, id: &'a str) -> Self {
        Self {
            scope,
            id,
            passed: false,
        }
    }
    /// 只在全部断言后报告成功；Reports success only after all assertions.
    fn pass(mut self) {
        self.passed = true;
    }
}
impl Drop for Case<'_> {
    /// 对每个 ID 输出唯一结果；Emits exactly one result for each ID.
    fn drop(&mut self) {
        println!(
            "LING_CONTRACT\t{}\t{}\t{}",
            self.scope,
            self.id,
            if self.passed { "passed" } else { "failed" }
        );
    }
}

/// 使用与其他语言完全相同的文件顺序和 ID。
/// Uses the exact same fixture ordering and IDs as the other languages.
fn rows(root: &Path, name: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(root.join(format!("{name}.tsv")))
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.split('|')
                .map(|field| field.trim().to_owned())
                .collect()
        })
        .collect()
}
/// 预期字节来自独立夹具；Expected bytes come from independent fixtures.
fn bytes(text: &str) -> Vec<u8> {
    let hex = text.replace(' ', "");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// 先验证冻结字节与错误，再连接驱动提供的 TCP 或双向 TLS 服务。
/// Checks frozen bytes and errors before connecting to the harness TCP or mutual-TLS server.
#[tokio::main]
async fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/protocol");
    let limits = Limits::default();
    for row in rows(&root, "values") {
        let case = Case::new("values", &row[0]);
        let expected = bytes(&row[1]);
        assert_eq!(
            encode_value(&decode_value(&expected, limits).unwrap(), limits).unwrap(),
            expected
        );
        case.pass();
    }
    for row in rows(&root, "invalid-values") {
        let case = Case::new("invalid-values", &row[0]);
        let error = decode_value(&bytes(&row[3]), limits).unwrap_err();
        assert_eq!(error.error.code, row[2]);
        assert_eq!(error.fatal, row[1] == "true");
        case.pass();
    }
    for row in rows(&root, "frames") {
        let case = Case::new("frames", &row[0]);
        let expected = bytes(&row[1]);
        let frame = decode_frame(&expected, limits).unwrap();
        // Map 字段无序，编码器允许规范化顺序；独立输入的完整帧语义必须保留。
        // Map fields are unordered and may be canonicalized; preserve the independent input frame's semantics.
        assert_eq!(
            decode_frame(&encode_frame(&frame, limits).unwrap(), limits).unwrap(),
            frame
        );
        case.pass();
    }
    let address: SocketAddr = std::env::var("LINGER_TEST_ADDR")
        .expect("run tests/sdk_contracts.py to supply LINGER_TEST_ADDR")
        .parse()
        .unwrap();
    let options = Options {
        request_timeout: Duration::from_secs(2),
        tls: std::env::var_os("LINGER_TEST_CA").map(|ca| ClientTls {
            server_name: "localhost".into(),
            ca_file: Some(ca.into()),
            cert_file: std::env::var_os("LINGER_TEST_CERT").map(Into::into),
            key_file: std::env::var_os("LINGER_TEST_KEY").map(Into::into),
        }),
        ..Options::default()
    };
    let mut client = Client::connect(address, None, 0, options.clone())
        .await
        .unwrap();
    for row in rows(&root, "commands") {
        let case = Case::new("commands", &row[0]);
        let Value::List(arguments) = decode_value(&bytes(&row[2]), limits).unwrap() else {
            panic!("argument list")
        };
        let result = client.execute(&row[1], arguments).await;
        if row[3].starts_with("ERR_") {
            let Err(Error::Server(error)) = result else {
                panic!("expected server error")
            };
            assert_eq!(error.code, row[3]);
        } else {
            assert_eq!(
                result.unwrap(),
                decode_value(&bytes(&row[3]), limits).unwrap()
            );
        }
        case.pass();
    }
    let Value::Map(info) = client.execute("INFO", vec![]).await.unwrap() else {
        panic!("INFO map")
    };
    for row in rows(&root, "info-fields") {
        let id = format!("{}.{}", row[0], row[1]);
        let case = Case::new("info-fields", &id);
        let Value::Map(section) = &info[&row[0]] else {
            panic!("INFO section")
        };
        let value = &section[&row[1]];
        assert!(
            match row[2].as_str() {
                "UInt" => matches!(value, Value::UInt(_)),
                "Bool" => matches!(value, Value::Bool(_)),
                "Float" => matches!(value, Value::Float(_)),
                "UIntOrNull" => matches!(value, Value::UInt(_) | Value::Null),
                "FloatOrNull" => matches!(value, Value::Float(_) | Value::Null),
                "Histogram" => matches!(value, Value::Map(histogram)
                if matches!(histogram.get("count"), Some(Value::UInt(_)))
                && matches!(histogram.get("sum"), Some(Value::UInt(_)))
                && matches!(histogram.get("bounds"), Some(Value::List(items)) if items.len() == 6)
                && matches!(histogram.get("buckets"), Some(Value::List(items)) if items.len() == 7)),
                _ => false,
            },
            "{id}: {value:?}"
        );
        case.pass();
    }
    for row in rows(&root, "values") {
        let case = Case::new("live-values", &row[0]);
        let key = Value::String(format!("rust:{}", row[0]));
        let value = decode_value(&bytes(&row[1]), limits).unwrap();
        client
            .execute("SET", vec![key.clone(), value.clone()])
            .await
            .unwrap();
        assert_eq!(client.execute("GET", vec![key]).await.unwrap(), value);
        case.pass();
    }
    let key = Value::String("rust:batch".into());
    let case = Case::new("scenario", "batch");
    let results = client
        .batch(vec![
            Request::new("SET", vec![key.clone(), Value::Null]),
            Request::new("GET", vec![key.clone()]),
            Request::new("GET", vec![Value::String("absent".into())]),
        ])
        .await
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[1], Ok(Value::Null));
    assert!(results[2].is_err());
    case.pass();

    let case = Case::new("scenario", "subscription");
    client.select(1).await.unwrap();
    let mut connection = Client::connect(address, None, 1, options.clone())
        .await
        .unwrap();
    let mut subscriber = connection
        .subscribe(vec!["rust:event".into()], Some("rust-workers"))
        .await
        .unwrap();
    client
        .execute("SET", vec![Value::String("rust:event".into()), Value::Null])
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), subscriber.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.id.db, 1);
    let id = event.id;
    subscriber.close().await.unwrap();
    case.pass();

    let case = Case::new("scenario", "redelivery");
    connection = Client::connect(address, None, 1, options.clone())
        .await
        .unwrap();
    connection.require_event_stream(id.stream_id).unwrap();
    subscriber = connection
        .subscribe(vec!["rust:event".into()], Some("rust-workers"))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), subscriber.next_event())
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );
    case.pass();
    let case = Case::new("scenario", "ack-cancel");
    subscriber.ack(id).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), subscriber.next_event())
            .await
            .is_err()
    );
    assert!(subscriber.is_closed());
    case.pass();

    let case = Case::new("scenario", "pool");
    let pool = Pool::new(address, None, 0, options, PoolOptions::default()).unwrap();
    assert_eq!(
        pool.execute("GET", vec![key.clone()]).await.unwrap(),
        Value::Null
    );
    assert!(
        !pool
            .execute("SELECT", vec![Value::UInt(1)])
            .await
            .unwrap_err()
            .outcome_unknown()
    );
    let mut lease = pool.acquire().await.unwrap();
    lease.execute("SCAN", vec![Value::UInt(0)]).await.unwrap();
    pool.close();
    assert!(
        !lease
            .execute("GET", vec![key])
            .await
            .unwrap_err()
            .outcome_unknown()
    );
    case.pass();
    client.close().await.unwrap();
}
