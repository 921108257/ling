//! 存储命令的可观察契约，不依赖网络或真实时间等待。
//! Observable storage contracts, independent of networking and wall-clock sleeps.

use ling_engine::{Config, Engine};
use ling_types::{Float, Scalar, Value, ValueLimits};
use std::collections::{BTreeMap, HashSet};

/// 文本参数帮助函数；Builds a textual argument.
fn s(value: &str) -> Value {
    Value::String(value.into())
}
/// 创建独立引擎，固定 epoch 方便核对身份；Creates an isolated engine with a fixed epoch.
fn engine() -> Engine {
    Engine::new(Config::default(), 7).unwrap()
}
/// 在足够的响应预算中运行命令；Runs a command with an ample response budget.
fn run(engine: &mut Engine, name: &str, args: Vec<Value>) -> Value {
    engine.execute(0, name, &args, 1024 * 1024).unwrap()
}
/// 明确创建测试值，不依赖命令隐式创建；Creates a value explicitly rather than relying on implicit creation.
fn put(engine: &mut Engine, key: &str, value: Value) {
    assert_eq!(run(engine, "SET", vec![s(key), value]), Value::Bool(true));
}
/// 核对稳定错误码；Checks the stable error code.
fn fails(engine: &mut Engine, name: &str, args: Vec<Value>, code: &str) {
    assert_eq!(
        engine
            .execute(0, name, &args, 1024 * 1024)
            .unwrap_err()
            .code,
        code
    );
}

/// 所有类型保持标签，Null 与不存在区分；All types retain their tags and Null differs from absence.
#[test]
fn all_value_types_null_missing_and_database_isolation() {
    let mut e = engine();
    let values = [
        Value::Null,
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::UInt(u64::MAX),
        Value::Float(Float::new(f64::INFINITY).unwrap()),
        s("中"),
        Value::Bytes(vec![0, 255]),
        Value::List(vec![Value::Null]),
        Value::Map(BTreeMap::from([("x".into(), Value::Null)])),
        Value::Set(HashSet::from([Scalar::Int(1)])),
    ];
    for value in values {
        put(&mut e, "k", value.clone());
        assert_eq!(run(&mut e, "GET", vec![s("k")]), value);
        assert_eq!(run(&mut e, "TYPE", vec![s("k")]), s(value.type_name()));
    }
    put(&mut e, "k", Value::Null);
    assert_eq!(
        e.execute(1, "GET", &[s("k")], 1024).unwrap_err().code,
        "ERR_KEY_NOT_FOUND"
    );
    e.execute(1, "SET", &[s("k"), Value::UInt(2)], 1024)
        .unwrap();
    assert_eq!(run(&mut e, "GET", vec![s("k")]), Value::Null);
    let Value::List(items) = run(&mut e, "MGET", vec![s("k"), s("missing"), s("k")]) else {
        panic!()
    };
    assert_eq!(items[0], items[2]);
    let Value::Map(ref first) = items[0] else {
        panic!()
    };
    assert_eq!(first["ok"], Value::Bool(true));
    assert_eq!(first["value"], Value::Null);
    let Value::Map(ref missing) = items[1] else {
        panic!()
    };
    assert_eq!(missing["code"], s("ERR_KEY_NOT_FOUND"));
}

/// 条件、重复参数和版本检查不得部分发布；Conditions, duplicates, and versions cannot partially publish.
#[test]
fn conditions_generations_and_multi_key_atomic_validation() {
    let mut e = engine();
    put(&mut e, "k", Value::Int(1));
    let identity = e.metadata(0, "k").unwrap();
    run(&mut e, "SET", vec![s("k"), Value::Int(1)]);
    assert_eq!(e.metadata(0, "k").unwrap(), identity);
    assert_eq!(
        run(&mut e, "SET", vec![s("k"), Value::Int(2), s("nx")]),
        Value::Bool(false)
    );
    assert_eq!(
        run(&mut e, "SET", vec![s("absent"), Value::Null, s("XX")]),
        Value::Bool(false)
    );
    fails(
        &mut e,
        "MSET",
        vec![s("new"), Value::Null, s("new"), Value::Bool(true)],
        "ERR_INVALID_ARGUMENT",
    );
    fails(&mut e, "GET", vec![s("new")], "ERR_KEY_NOT_FOUND");
    fails(
        &mut e,
        "MSET",
        vec![s("new"), Value::Null, s(""), Value::Bool(true)],
        "ERR_INVALID_ARGUMENT",
    );
    fails(&mut e, "GET", vec![s("new")], "ERR_KEY_NOT_FOUND");
    assert_eq!(
        run(&mut e, "GETSET", vec![s("k"), Value::UInt(1)]),
        Value::Int(1)
    );
    assert_eq!(e.metadata(0, "k").unwrap().revision, identity.revision + 1);
    assert_eq!(
        run(&mut e, "EXISTS", vec![s("k"), s("k"), s("absent")]),
        Value::UInt(1)
    );
    assert_eq!(run(&mut e, "DEL", vec![s("k"), s("k")]), Value::UInt(1));
    put(&mut e, "k", Value::Null);
    assert_ne!(e.metadata(0, "k").unwrap().generation, identity.generation);
    assert_eq!(e.metadata(0, "k").unwrap().revision, 1);
}

/// 宽整数处理最小负数，有限浮点溢出与 NaN 失败不修改旧值。
/// Wide integers handle the minimum signed value; floating overflow and NaN preserve the old value.
#[test]
fn numeric_boundaries_and_text_bytes_are_strict() {
    let mut e = engine();
    put(&mut e, "i", Value::Int(i64::MIN));
    assert_eq!(
        run(&mut e, "DECRBY", vec![s("i"), Value::Int(i64::MIN)]),
        Value::Int(0)
    );
    put(&mut e, "i", Value::Int(i64::MAX));
    fails(&mut e, "INCR", vec![s("i")], "ERR_OVERFLOW");
    assert_eq!(run(&mut e, "GET", vec![s("i")]), Value::Int(i64::MAX));
    put(&mut e, "u", Value::UInt(0));
    fails(&mut e, "DECR", vec![s("u")], "ERR_OVERFLOW");
    assert_eq!(
        run(&mut e, "DECRBY", vec![s("u"), Value::Int(i64::MIN)]),
        Value::UInt(1u64 << 63)
    );
    put(&mut e, "u", Value::UInt(u64::MAX));
    fails(&mut e, "INCR", vec![s("u")], "ERR_OVERFLOW");
    put(&mut e, "f", Value::Float(Float::new(f64::MAX).unwrap()));
    fails(
        &mut e,
        "INCRFLOAT",
        vec![s("f"), Value::Float(Float::new(f64::MAX).unwrap())],
        "ERR_OVERFLOW",
    );
    put(
        &mut e,
        "f",
        Value::Float(Float::new(f64::INFINITY).unwrap()),
    );
    fails(
        &mut e,
        "INCRFLOAT",
        vec![s("f"), Value::Float(Float::new(f64::NEG_INFINITY).unwrap())],
        "ERR_INVALID_VALUE",
    );
    put(&mut e, "s", s("中"));
    assert_eq!(run(&mut e, "APPEND", vec![s("s"), s("文")]), Value::UInt(6));
    assert_eq!(run(&mut e, "STRLEN", vec![s("s")]), Value::UInt(6));
    put(&mut e, "b", Value::Bytes(vec![0, 255]));
    assert_eq!(run(&mut e, "BLEN", vec![s("b")]), Value::UInt(2));
    fails(&mut e, "APPEND", vec![s("b"), s("x")], "ERR_TYPE_MISMATCH");
    fails(&mut e, "INCR", vec![s("none")], "ERR_KEY_NOT_FOUND");
}

/// List 端点、方向、类型敏感删除与空容器行为。
/// List endpoints, direction, type-sensitive removals, and empty-container behavior.
#[test]
fn list_commands_preserve_order_and_empty_keys() {
    let mut e = engine();
    put(&mut e, "l", Value::List(vec![]));
    assert_eq!(
        run(&mut e, "LPUSH", vec![s("l"), Value::Int(1), Value::UInt(1)]),
        Value::UInt(2)
    );
    run(&mut e, "RPUSH", vec![s("l"), Value::Int(2)]);
    assert_eq!(
        run(
            &mut e,
            "LRANGE",
            vec![s("l"), Value::Int(i64::MIN), Value::Int(i64::MAX)]
        ),
        Value::List(vec![Value::UInt(1), Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        run(&mut e, "RPOP", vec![s("l"), Value::UInt(2)]),
        Value::List(vec![Value::Int(2), Value::Int(1)])
    );
    assert_eq!(
        run(&mut e, "LINDEX", vec![s("l"), Value::Int(-1)]),
        Value::UInt(1)
    );
    run(&mut e, "LSET", vec![s("l"), Value::Int(0), Value::Int(1)]);
    run(&mut e, "RPUSH", vec![s("l"), Value::UInt(1), Value::Int(1)]);
    assert_eq!(
        run(&mut e, "LREM", vec![s("l"), Value::Int(-1), Value::Int(1)]),
        Value::UInt(1)
    );
    assert_eq!(
        run(&mut e, "LREM", vec![s("l"), Value::Int(0), Value::Int(1)]),
        Value::UInt(1)
    );
    run(
        &mut e,
        "LTRIM",
        vec![s("l"), Value::Int(10), Value::Int(20)],
    );
    assert_eq!(run(&mut e, "LLEN", vec![s("l")]), Value::UInt(0));
    assert_eq!(run(&mut e, "LPOP", vec![s("l")]), Value::List(vec![]));
    assert_eq!(run(&mut e, "TYPE", vec![s("l")]), s("list"));
    fails(
        &mut e,
        "LINDEX",
        vec![s("l"), Value::Int(-1)],
        "ERR_INDEX_OUT_OF_RANGE",
    );
}

/// Map 逐项错误、浅合并、重复字段和空容器。
/// Map item errors, shallow merges, duplicate fields, and empty containers.
#[test]
fn map_commands_reject_partial_field_updates() {
    let mut e = engine();
    put(&mut e, "m", Value::Map(BTreeMap::new()));
    assert_eq!(
        run(
            &mut e,
            "FSET",
            vec![s("m"), s("x"), Value::Null, s("y"), Value::Int(1)]
        ),
        Value::UInt(2)
    );
    let before = run(&mut e, "FGETALL", vec![s("m")]);
    fails(
        &mut e,
        "FSET",
        vec![s("m"), s("z"), Value::Int(1), s("z"), Value::Int(2)],
        "ERR_INVALID_ARGUMENT",
    );
    assert_eq!(run(&mut e, "FGETALL", vec![s("m")]), before);
    assert_eq!(
        run(&mut e, "FSETNX", vec![s("m"), s("x"), Value::Bool(true)]),
        Value::Bool(false)
    );
    assert_eq!(
        run(&mut e, "FEXISTS", vec![s("m"), s("x")]),
        Value::Bool(true)
    );
    let Value::List(items) = run(&mut e, "FGET", vec![s("m"), s("x"), s("missing"), s("x")]) else {
        panic!()
    };
    assert_eq!(items[0], items[2]);
    let Value::Map(ref item) = items[1] else {
        panic!()
    };
    assert_eq!(item["code"], s("ERR_FIELD_NOT_FOUND"));
    assert_eq!(
        run(&mut e, "FKEYS", vec![s("m")]),
        Value::List(vec![s("x"), s("y")])
    );
    assert_eq!(
        run(&mut e, "FVALS", vec![s("m")]),
        Value::List(vec![Value::Null, Value::Int(1)])
    );
    assert_eq!(
        run(
            &mut e,
            "FMERGE",
            vec![
                s("m"),
                Value::Map(BTreeMap::from([
                    ("x".into(), Value::Bool(true)),
                    ("z".into(), Value::Null)
                ]))
            ]
        ),
        Value::UInt(1)
    );
    assert_eq!(
        run(&mut e, "FDEL", vec![s("m"), s("x"), s("x"), s("y"), s("z")]),
        Value::UInt(3)
    );
    assert_eq!(run(&mut e, "FLEN", vec![s("m")]), Value::UInt(0));
    assert_eq!(run(&mut e, "TYPE", vec![s("m")]), s("map"));
}

/// Set 区分数值类型和字节，STORE 允许目标同时是源。
/// Sets distinguish numeric tags and bytes; STORE may use its destination as a source.
#[test]
fn set_members_algebra_random_counts_and_empty_keys() {
    let mut e = engine();
    put(&mut e, "a", Value::Set(HashSet::new()));
    assert_eq!(
        run(
            &mut e,
            "SADD",
            vec![
                s("a"),
                Value::Int(1),
                Value::UInt(1),
                Value::Int(1),
                Value::Bytes(vec![255])
            ]
        ),
        Value::UInt(3)
    );
    assert_eq!(
        run(&mut e, "SMISMEMBER", vec![s("a"), Value::Int(1), s("1")]),
        Value::List(vec![Value::Bool(true), Value::Bool(false)])
    );
    fails(
        &mut e,
        "SADD",
        vec![s("a"), Value::Int(2), Value::List(vec![])],
        "ERR_INVALID_VALUE",
    );
    assert_eq!(run(&mut e, "SCARD", vec![s("a")]), Value::UInt(3));
    put(&mut e, "b", Value::Set(HashSet::from([Scalar::Int(1)])));
    assert_eq!(
        run(&mut e, "SINTER", vec![s("a"), s("b")]),
        Value::Set(HashSet::from([Scalar::Int(1)]))
    );
    assert_eq!(
        run(&mut e, "SDIFF", vec![s("b"), s("a")]),
        Value::Set(HashSet::new())
    );
    assert_eq!(
        run(&mut e, "SUNIONSTORE", vec![s("b"), s("b"), s("a")]),
        Value::UInt(3)
    );
    let before = run(&mut e, "SMEMBERS", vec![s("b")]);
    fails(
        &mut e,
        "SUNIONSTORE",
        vec![s("b"), s("a"), s("missing")],
        "ERR_KEY_NOT_FOUND",
    );
    assert_eq!(run(&mut e, "SMEMBERS", vec![s("b")]), before);
    let Value::List(random) = run(&mut e, "SRANDMEMBER", vec![s("a"), Value::Int(-9)]) else {
        panic!()
    };
    assert_eq!(random.len(), 9);
    let Value::List(popped) = run(&mut e, "SPOP", vec![s("a"), Value::UInt(u64::MAX)]) else {
        panic!()
    };
    assert_eq!(popped.len(), 3);
    assert_eq!(
        run(&mut e, "SRANDMEMBER", vec![s("a"), Value::Int(i64::MIN)]),
        Value::List(vec![])
    );
    assert_eq!(
        run(&mut e, "SREM", vec![s("a"), Value::Null]),
        Value::UInt(0)
    );
    assert_eq!(
        run(&mut e, "SISMEMBER", vec![s("a"), Value::Null]),
        Value::Bool(false)
    );
    assert_eq!(run(&mut e, "TYPE", vec![s("a")]), s("set"));
}

/// 响应、值、深度和内存拒绝都发生在发布之前。
/// Response, value, depth, and memory rejections all precede publication.
#[test]
fn resource_failures_leave_values_versions_and_memory_unchanged() {
    let mut e = Engine::new(
        Config {
            max_keys: 1,
            value: ValueLimits {
                max_depth: 2,
                max_bytes: 64,
                ..Default::default()
            },
            ..Default::default()
        },
        1,
    )
    .unwrap();
    put(&mut e, "l", Value::List(vec![s("payload")]));
    let before = e.metadata(0, "l").unwrap();
    let memory = e.managed_bytes();
    assert_eq!(
        e.execute(0, "LPOP", &[s("l")], 6).unwrap_err().code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(e.metadata(0, "l").unwrap(), before);
    assert_eq!(e.managed_bytes(), memory);
    fails(
        &mut e,
        "LPUSH",
        vec![s("l"), Value::List(vec![Value::List(vec![])])],
        "ERR_LIMIT_EXCEEDED",
    );
    fails(
        &mut e,
        "RPUSH",
        vec![s("l"), s(&"x".repeat(60))],
        "ERR_LIMIT_EXCEEDED",
    );
    fails(
        &mut e,
        "SET",
        vec![s("new"), Value::Null],
        "ERR_OUT_OF_MEMORY",
    );
    assert_eq!(
        run(&mut e, "GET", vec![s("l")]),
        Value::List(vec![s("payload")])
    );
    let mut e = Engine::new(
        Config {
            max_memory_bytes: 1,
            ..Default::default()
        },
        1,
    )
    .unwrap();
    fails(
        &mut e,
        "SET",
        vec![s("a"), Value::Null],
        "ERR_OUT_OF_MEMORY",
    );
    assert_eq!(e.managed_bytes(), 0);
    fails(
        &mut e,
        "MSET",
        vec![s("a"), Value::Null, s("b"), Value::Null],
        "ERR_OUT_OF_MEMORY",
    );
    assert_eq!(run(&mut e, "DBSIZE", vec![]), Value::UInt(0));
}

/// 响应封套不占业务深度；集合结果超限不能覆盖目标。
/// Response wrappers do not consume business depth; oversized unions never replace destinations.
#[test]
fn nested_results_and_union_budgets_preserve_contracts() {
    let mut e = Engine::new(
        Config {
            value: ValueLimits {
                max_depth: 1,
                max_bytes: 32,
                ..Default::default()
            },
            ..Default::default()
        },
        9,
    )
    .unwrap();
    put(
        &mut e,
        "m",
        Value::Map(BTreeMap::from([("x".into(), Value::Null)])),
    );
    let Value::List(items) = run(&mut e, "MGET", vec![s("m"), s("m")]) else {
        panic!()
    };
    assert_eq!(items[0], items[1]);
    assert_eq!(
        e.execute(0, "MGET", &[s("m"), s("m")], 32)
            .unwrap_err()
            .code,
        "ERR_LIMIT_EXCEEDED"
    );
    put(
        &mut e,
        "a",
        Value::Set(HashSet::from([Scalar::String("a".repeat(20))])),
    );
    put(
        &mut e,
        "b",
        Value::Set(HashSet::from([Scalar::String("b".repeat(20))])),
    );
    let before = e.metadata(0, "a").unwrap();
    fails(
        &mut e,
        "SUNIONSTORE",
        vec![s("a"), s("a"), s("b")],
        "ERR_LIMIT_EXCEEDED",
    );
    assert_eq!(e.metadata(0, "a").unwrap(), before);
    assert_eq!(
        e.execute(0, "SPOP", &[s("a")], 10).unwrap_err().code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(e.metadata(0, "a").unwrap(), before);
    assert_eq!(run(&mut e, "SCARD", vec![s("a")]), Value::UInt(1));
}
