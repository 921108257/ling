//! 游标弱一致性、字节匹配与响应预算契约。
//! Cursor weak-consistency, byte matching, and response-budget contracts.

use linger_engine::{Config, Engine, ScanState};
use linger_types::Value;
use std::collections::HashSet;

/// 构造文本参数；Builds a text argument.
fn s(text: &str) -> Value {
    Value::String(text.into())
}
/// 提取一页游标和 Key；Extracts a cursor and keys from one page.
fn page(value: Value) -> (u64, Vec<String>) {
    let Value::Map(mut fields) = value else {
        panic!()
    };
    let Value::UInt(cursor) = fields.remove("cursor").unwrap() else {
        panic!()
    };
    let Value::List(keys) = fields.remove("keys").unwrap() else {
        panic!()
    };
    (
        cursor,
        keys.into_iter()
            .map(|v| {
                let Value::String(key) = v else { panic!() };
                key
            })
            .collect(),
    )
}
/// 插入和删除过程中，持续存在的 Key 必须全部出现。
/// Keys that remain continuously present must all appear despite concurrent insertions and deletions.
#[test]
fn scan_survives_mutation_without_skipping_persistent_keys() {
    let e = Engine::new(
        Config {
            shard_count: 4,
            ..Config::default()
        },
        1,
    )
    .unwrap();
    let persistent: HashSet<_> = (0..120).map(|i| format!("stable-{i:03}")).collect();
    for key in &persistent {
        e.execute(0, "SET", &[s(key), Value::Null], 4096).unwrap();
    }
    let mut state = ScanState::default();
    let mut cursor = 0;
    let mut seen = HashSet::new();
    let mut pages = 0;
    loop {
        let args = [
            Value::UInt(cursor),
            s("MATCH"),
            s("stable-*"),
            s("COUNT"),
            Value::UInt(3),
        ];
        let (next, keys) = page(e.scan(0, &args, &mut state, 1024).unwrap());
        seen.extend(keys);
        e.execute(
            0,
            "SET",
            &[s(&format!("temporary-{pages}")), Value::Bool(true)],
            4096,
        )
        .unwrap();
        if pages > 0 {
            e.execute(0, "DEL", &[s(&format!("temporary-{}", pages - 1))], 4096)
                .unwrap();
        }
        pages += 1;
        cursor = next;
        if cursor == 0 {
            break;
        }
        assert!(pages < 200);
    }
    assert_eq!(seen, persistent);
}

/// 游标不能跨连接、数据库、筛选或页重放；失败响应不消耗进度。
/// Cursors cannot cross connections, databases, filters, or replayed pages; failed responses preserve progress.
#[test]
fn cursor_is_bound_and_response_failure_keeps_progress() {
    let e = Engine::new(
        Config {
            shard_count: 1,
            ..Config::default()
        },
        1,
    )
    .unwrap();
    for key in ["a", "b", "c"] {
        e.execute(0, "SET", &[s(key), Value::Int(1)], 1024).unwrap();
    }
    let mut state = ScanState::default();
    let (cursor, _) = page(
        e.scan(
            0,
            &[Value::UInt(0), s("COUNT"), Value::UInt(1)],
            &mut state,
            1024,
        )
        .unwrap(),
    );
    assert_ne!(cursor, 0);
    for (db, args) in [
        (1, vec![Value::UInt(cursor)]),
        (0, vec![Value::UInt(cursor), s("TYPE"), s("null")]),
    ] {
        assert_eq!(
            e.scan(db, &args, &mut state, 1024).unwrap_err().code,
            "ERR_INVALID_CURSOR"
        );
    }
    assert_eq!(
        e.scan(0, &[Value::UInt(cursor)], &mut ScanState::default(), 1024)
            .unwrap_err()
            .code,
        "ERR_INVALID_CURSOR"
    );
    assert_eq!(
        e.scan(0, &[Value::UInt(cursor)], &mut state, 1)
            .unwrap_err()
            .code,
        "ERR_LIMIT_EXCEEDED"
    );
    let (next, keys) = page(
        e.scan(
            0,
            &[Value::UInt(cursor), s("COUNT"), Value::UInt(1)],
            &mut state,
            1024,
        )
        .unwrap(),
    );
    assert_eq!(keys, vec!["b"]);
    assert_ne!(next, cursor);
    assert_eq!(
        e.scan(0, &[Value::UInt(cursor)], &mut state, 1024)
            .unwrap_err()
            .code,
        "ERR_INVALID_CURSOR"
    );
}

/// 通配符按字节匹配，查询不截断超限 Key。
/// Wildcards match bytes and queries never truncate oversized keys.
#[test]
fn byte_patterns_types_and_query_limits_are_explicit() {
    let e = Engine::new(
        Config {
            shard_count: 2,
            ..Config::default()
        },
        1,
    )
    .unwrap();
    for key in ["中", "*", "abc", "a?c"] {
        e.execute(0, "SET", &[s(key), Value::Null], 1024).unwrap();
    }
    let Value::List(keys) = e.execute(0, "KEYS", &[s("???")], 4096).unwrap() else {
        panic!()
    };
    assert_eq!(
        keys.into_iter()
            .map(|v| format!("{v:?}"))
            .collect::<HashSet<_>>(),
        [s("中"), s("abc"), s("a?c")]
            .into_iter()
            .map(|v| format!("{v:?}"))
            .collect()
    );
    assert_eq!(
        e.execute(0, "KEYS", &[s(r"\*")], 4096).unwrap(),
        Value::List(vec![s("*")])
    );
    assert_eq!(
        e.execute(0, "KEYS", &[s(r"broken\")], 4096)
            .unwrap_err()
            .code,
        "ERR_INVALID_ARGUMENT"
    );
    assert_eq!(
        e.execute(0, "KEYS", &[s("*")], 6).unwrap_err().code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(
        e.execute(1, "RANDOMKEY", &[], 4096).unwrap_err().code,
        "ERR_KEY_NOT_FOUND"
    );
    let random = e.execute(0, "RANDOMKEY", &[], 4096).unwrap();
    assert_eq!(
        e.execute(0, "RANDOMKEY", &[], 1).unwrap_err().code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert!([s("中"), s("*"), s("abc"), s("a?c")].contains(&random));
    let mut state = ScanState::default();
    assert_eq!(
        e.scan(
            0,
            &[Value::UInt(0), s("COUNT"), Value::UInt(0)],
            &mut state,
            4096
        )
        .unwrap_err()
        .code,
        "ERR_INVALID_ARGUMENT"
    );
    assert_eq!(
        e.scan(
            0,
            &[Value::UInt(0), s("TYPE"), s("bogus")],
            &mut state,
            4096
        )
        .unwrap_err()
        .code,
        "ERR_INVALID_ARGUMENT"
    );
    let (cursor, keys) = page(
        e.scan(0, &[Value::UInt(0), s("TYPE"), s("int")], &mut state, 4096)
            .unwrap(),
    );
    assert_eq!(cursor, 0);
    assert!(keys.is_empty());
}
