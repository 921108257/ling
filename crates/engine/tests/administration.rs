//! 重命名、跨库、清空和交换的公开契约。
//! Public contracts for renaming, cross-DB changes, flushing, and swapping.

use linger_engine::{Config, Engine};
use linger_types::Value;

/// 明确构造文本参数；Builds an explicit text argument.
fn s(text: &str) -> Value {
    Value::String(text.into())
}
/// 在常规响应预算下执行命令；Executes with a normal response budget.
fn run(e: &Engine, db: u16, name: &str, args: &[Value]) -> Value {
    e.execute(db, name, args, 4096).unwrap()
}

/// 条件检查、缺失源和参数失败不能迁移部分条目，覆盖目的地也分配新身份。
/// Conditions, missing sources, and invalid arguments never partially relocate; overwritten destinations get fresh identities.
#[test]
fn relocations_keep_sources_and_assign_fresh_target_identities() {
    let e = Engine::new(
        Config {
            shard_count: 4,
            ..Config::default()
        },
        8,
    )
    .unwrap();
    run(&e, 0, "SET", &[s("source"), Value::Null]);
    run(&e, 0, "SET", &[s("target"), Value::Int(1)]);
    let original = e.metadata(0, "source").unwrap();
    let destination = e.metadata(0, "target").unwrap();
    assert_eq!(
        run(&e, 0, "RENAMENX", &[s("source"), s("target")]),
        Value::Bool(false)
    );
    assert_eq!(
        run(&e, 0, "RENAME", &[s("source"), s("source")]),
        Value::Bool(true)
    );
    assert_eq!(e.metadata(0, "source").unwrap(), original);
    assert_eq!(
        run(&e, 0, "COPY", &[s("source"), s("target")]),
        Value::Bool(false)
    );
    run(&e, 0, "COPY", &[s("source"), s("target"), s("REPLACE")]);
    assert_ne!(
        e.metadata(0, "target").unwrap().generation,
        destination.generation
    );
    assert_ne!(
        e.metadata(0, "target").unwrap().generation,
        original.generation
    );
    run(
        &e,
        0,
        "COPY",
        &[s("source"), s("source"), s("DB"), Value::UInt(1)],
    );
    assert_eq!(run(&e, 1, "GET", &[s("source")]), Value::Null);
    assert_eq!(
        run(&e, 0, "MOVE", &[s("source"), Value::UInt(1)]),
        Value::Bool(false)
    );
    assert_eq!(
        run(&e, 0, "MOVE", &[s("source"), Value::UInt(0)]),
        Value::Bool(false)
    );
    run(&e, 1, "DEL", &[s("source")]);
    assert_eq!(
        run(&e, 0, "MOVE", &[s("source"), Value::UInt(1)]),
        Value::Bool(true)
    );
    assert_eq!(
        e.execute(0, "GET", &[s("source")], 4096).unwrap_err().code,
        "ERR_KEY_NOT_FOUND"
    );
    assert_ne!(
        e.metadata(1, "source").unwrap().generation,
        original.generation
    );
    for (name, args, code) in [
        (
            "COPY",
            vec![s("target"), s("target")],
            "ERR_INVALID_ARGUMENT",
        ),
        (
            "MOVE",
            vec![s("target"), Value::UInt(65536)],
            "ERR_DB_OUT_OF_RANGE",
        ),
        (
            "COPY",
            vec![s("target"), s("copy"), s("DB"), Value::UInt(16)],
            "ERR_DB_OUT_OF_RANGE",
        ),
        (
            "COPY",
            vec![s("target"), s("copy"), s("REPLACE"), s("REPLACE")],
            "ERR_INVALID_ARGUMENT",
        ),
        (
            "RENAME",
            vec![s("missing"), s("target")],
            "ERR_KEY_NOT_FOUND",
        ),
    ] {
        assert_eq!(e.execute(0, name, &args, 4096).unwrap_err().code, code);
    }
    assert_eq!(run(&e, 0, "GET", &[s("target")]), Value::Null);
}

/// SWAP 只交换数据，清库不重置身份计数；响应预算失败也不改变数据库。
/// SWAP exchanges only data; flush never resets identity counters, and response-budget failure preserves DBs.
#[test]
fn swap_and_flush_are_whole_database_operations() {
    let e = Engine::new(
        Config {
            shard_count: 3,
            ..Config::default()
        },
        9,
    )
    .unwrap();
    for db in 0..2 {
        for i in 0..30 {
            run(&e, db, "SET", &[s(&format!("k{i}")), Value::Int(db as i64)]);
        }
    }
    let before = e.metadata(0, "k0").unwrap();
    assert_eq!(
        e.execute(0, "SWAPDB", &[Value::UInt(0), Value::UInt(1)], 1)
            .unwrap_err()
            .code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(e.metadata(0, "k0").unwrap(), before);
    run(&e, 0, "SWAPDB", &[Value::UInt(0), Value::UInt(1)]);
    assert_eq!(run(&e, 0, "GET", &[s("k0")]), Value::Int(1));
    assert_eq!(run(&e, 1, "GET", &[s("k0")]), Value::Int(0));
    assert_ne!(e.metadata(0, "k0").unwrap().generation, before.generation);
    assert_eq!(e.metadata(0, "k0").unwrap().revision, 1);
    let same = e.metadata(0, "k0").unwrap();
    run(&e, 0, "SWAPDB", &[Value::UInt(0), Value::UInt(0)]);
    assert_eq!(e.metadata(0, "k0").unwrap(), same);
    let memory = e.managed_bytes();
    assert_eq!(
        e.execute(0, "FLUSHDB", &[s("INVALID")], 4096)
            .unwrap_err()
            .code,
        "ERR_INVALID_ARGUMENT"
    );
    assert_eq!(e.managed_bytes(), memory);
    run(&e, 0, "FLUSHDB", &[s("ASYNC")]);
    assert_eq!(run(&e, 0, "DBSIZE", &[]), Value::UInt(0));
    assert_eq!(run(&e, 1, "DBSIZE", &[]), Value::UInt(30));
    run(&e, 0, "FLUSHALL", &[]);
    assert_eq!(e.managed_bytes(), 0);
    run(&e, 0, "SET", &[s("k0"), Value::Null]);
    assert!(e.metadata(0, "k0").unwrap().generation.counter > same.generation.counter);
}

/// 目标库容量失败保留源、目标和受管预算。
/// Destination quota failure preserves source, destination, and managed budget.
#[test]
fn cross_database_quota_rejections_publish_nothing() {
    let e = Engine::new(
        Config {
            shard_count: 2,
            max_keys: 1,
            ..Config::default()
        },
        2,
    )
    .unwrap();
    run(&e, 0, "SET", &[s("a"), Value::Null]);
    run(&e, 1, "SET", &[s("occupied"), Value::Bool(true)]);
    let before = e.metadata(0, "a").unwrap();
    let memory = e.managed_bytes();
    assert_eq!(
        e.execute(0, "MOVE", &[s("a"), Value::UInt(1)], 4096)
            .unwrap_err()
            .code,
        "ERR_OUT_OF_MEMORY"
    );
    assert_eq!(e.metadata(0, "a").unwrap(), before);
    assert_eq!(e.managed_bytes(), memory);
    assert_eq!(run(&e, 1, "GET", &[s("occupied")]), Value::Bool(true));
}
