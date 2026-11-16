//! 全实例内存、单库 Key 配额和淘汰失败原子性。
//! Instance memory, per-database key quotas, and atomic rejection of eviction failures.

use ling_engine::{Config, Engine, EvictionPolicy};
use ling_types::Value;

/// 显式文本参数；Explicit text argument.
fn s(text: &str) -> Value {
    Value::String(text.into())
}
/// 按指定数据库读取；Reads from the specified database.
fn get(e: &Engine, db: u16, key: &str) -> Result<Value, ling_types::DbError> {
    e.execute(db, "GET", &[s(key)], 4096)
}

/// 样本大于候选集时仍随机选择，不能总淘汰有序索引的首个 Key。
/// Selection stays random when samples exceed candidates instead of always evicting the first sorted key.
#[test]
fn random_eviction_is_not_biased_to_the_first_sample() {
    let mut saw_a = false;
    let mut saw_b = false;
    // 两个候选连续 128 次只选同一个的概率为 2^-127。
    // With two candidates, choosing only one across 128 independent trials has probability 2^-127.
    for _ in 0..128 {
        let e = Engine::new(
            Config {
                shard_count: 1,
                max_keys: 2,
                eviction: EvictionPolicy::Random,
                sample_size: 5,
                ..Config::default()
            },
            1,
        )
        .unwrap();
        for key in ["a", "b", "c"] {
            e.execute(0, "SET", &[s(key), Value::Null], 4096).unwrap();
        }
        saw_a |= get(&e, 0, "a").is_err();
        saw_b |= get(&e, 0, "b").is_err();
        if saw_a && saw_b {
            return;
        }
    }
    panic!("random eviction always selected the same sorted candidate");
}

/// Key 配额只淘汰受限数据库；TTL-first 没有期限候选时拒绝写入。
/// Key quotas evict only in the constrained database; ttl-first rejects writes with no timed candidates.
#[test]
fn key_quota_scopes_eviction_to_its_database() {
    for policy in [
        EvictionPolicy::Lru,
        EvictionPolicy::Lfu,
        EvictionPolicy::Random,
        EvictionPolicy::TtlFirst,
    ] {
        let e = Engine::new(
            Config {
                shard_count: 3,
                max_keys: 1,
                eviction: policy,
                ..Config::default()
            },
            1,
        )
        .unwrap();
        for db in 0..2 {
            e.execute(db, "SET", &[s("old"), Value::Null], 4096)
                .unwrap();
        }
        let before = e.managed_bytes();
        let result = e.execute(0, "SET", &[s("new"), Value::Null], 4096);
        if policy == EvictionPolicy::TtlFirst {
            assert_eq!(result.unwrap_err().code, "ERR_OUT_OF_MEMORY");
            assert_eq!(get(&e, 0, "old").unwrap(), Value::Null);
            assert_eq!(e.managed_bytes(), before);
        } else {
            result.unwrap();
            assert_eq!(get(&e, 0, "old").unwrap_err().code, "ERR_KEY_NOT_FOUND");
            assert_eq!(get(&e, 0, "new").unwrap(), Value::Null);
        }
        assert_eq!(get(&e, 1, "old").unwrap(), Value::Null);
    }
}

/// 内存不足可跨库选择受害者，但最终仍放不下或响应失败时一项也不淘汰。
/// Memory pressure may choose victims across DBs, but impossible writes or failed responses evict nothing.
#[test]
fn global_memory_eviction_and_failed_writes_are_atomic() {
    let probe = Engine::new(
        Config {
            shard_count: 2,
            ..Config::default()
        },
        1,
    )
    .unwrap();
    probe
        .execute(0, "SET", &[s("old"), Value::Null], 4096)
        .unwrap();
    let cap = probe.managed_bytes();
    let e = Engine::new(
        Config {
            shard_count: 2,
            max_memory_bytes: cap,
            eviction: EvictionPolicy::Random,
            ..Config::default()
        },
        2,
    )
    .unwrap();
    e.execute(0, "SET", &[s("old"), Value::Null], 4096).unwrap();
    let original = e.metadata(0, "old").unwrap();
    assert_eq!(
        e.execute(1, "SET", &[s("big"), s(&"x".repeat(cap * 2))], 4096)
            .unwrap_err()
            .code,
        "ERR_OUT_OF_MEMORY"
    );
    assert_eq!(e.metadata(0, "old").unwrap(), original);
    assert_eq!(e.managed_bytes(), cap);
    assert_eq!(
        e.execute(1, "SET", &[s("new"), Value::Null], 1)
            .unwrap_err()
            .code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(get(&e, 0, "old").unwrap(), Value::Null);
    e.execute(1, "SET", &[s("new"), Value::Null], 4096).unwrap();
    assert_eq!(get(&e, 0, "old").unwrap_err().code, "ERR_KEY_NOT_FOUND");
    assert_eq!(get(&e, 1, "new").unwrap(), Value::Null);
    assert!(e.managed_bytes() <= cap);
}

/// COPY 不能为满足容量淘汰自己的源；多 Key 写入不能淘汰本次目标。
/// COPY cannot evict its own source to make room; multi-key writes cannot evict their own targets.
#[test]
fn command_sources_and_targets_are_protected() {
    let e = Engine::new(
        Config {
            shard_count: 2,
            max_keys: 1,
            eviction: EvictionPolicy::Random,
            ..Config::default()
        },
        1,
    )
    .unwrap();
    e.execute(0, "SET", &[s("a"), Value::Null], 4096).unwrap();
    let before = e.managed_bytes();
    assert_eq!(
        e.execute(0, "COPY", &[s("a"), s("b")], 4096)
            .unwrap_err()
            .code,
        "ERR_OUT_OF_MEMORY"
    );
    assert_eq!(
        e.execute(0, "MSET", &[s("b"), Value::Null, s("c"), Value::Null], 4096)
            .unwrap_err()
            .code,
        "ERR_OUT_OF_MEMORY"
    );
    assert_eq!(get(&e, 0, "a").unwrap(), Value::Null);
    assert_eq!(e.managed_bytes(), before);
}
