//! 分片所有权契约，验证独立推进与多 Key 原子可见性。
//! Shard ownership contracts proving independent progress and atomic multi-key visibility.

use crate::{Config, Engine};
use linger_types::Value;
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

/// 找到落在不同分片的 Key；不假设进程随机哈希的具体结果。
/// Finds keys on distinct shards without assuming a particular process-random hash result.
fn distinct(engine: &Engine) -> (String, String) {
    let first = "first".to_string();
    let second = (0..)
        .map(|i| format!("other-{i}"))
        .find(|key| engine.shard_for(key) != engine.shard_for(&first))
        .unwrap();
    (first, second)
}

/// 阻塞一个分片时，另一个分片读取无需等待全局锁。
/// A read on another shard progresses while one shard is blocked.
#[test]
fn independent_shards_make_progress() {
    let engine = Arc::new(
        Engine::new(
            Config {
                shard_count: 4,
                ..Config::default()
            },
            3,
        )
        .unwrap(),
    );
    let (first, second) = distinct(&engine);
    engine
        .execute(
            0,
            "SET",
            &[Value::String(second.clone()), Value::Null],
            1024,
        )
        .unwrap();
    let guard = engine.shards[engine.location(0, &first).unwrap()]
        .lock()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let cloned = engine.clone();
    let worker = std::thread::spawn(move || {
        tx.send(cloned.execute(0, "GET", &[Value::String(second)], 1024))
            .unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(guard);
    worker.join().unwrap();
    assert_eq!(result.unwrap().unwrap(), Value::Null);
}

/// 不同输入顺序仍取得相同锁序；并发观察看不到半条 MSET。
/// Different argument orders share lock order; concurrent observers never see half an MSET.
#[test]
fn cross_shard_writes_have_fixed_order_and_atomic_observers() {
    let engine = Arc::new(
        Engine::new(
            Config {
                shard_count: 4,
                ..Config::default()
            },
            5,
        )
        .unwrap(),
    );
    let (first, second) = distinct(&engine);
    let (first, second) = (Value::String(first), Value::String(second));
    engine
        .execute(
            0,
            "MSET",
            &[first.clone(), Value::Int(0), second.clone(), Value::Int(0)],
            4096,
        )
        .unwrap();
    let (done, received) = mpsc::channel();
    let mut workers = Vec::new();
    for worker in 0..4 {
        let (engine, first, second, done) =
            (engine.clone(), first.clone(), second.clone(), done.clone());
        workers.push(std::thread::spawn(move || {
            for i in 0..200 {
                let value = Value::Int(worker * 1000 + i);
                let keys = if worker % 2 == 0 {
                    [&first, &second]
                } else {
                    [&second, &first]
                };
                engine
                    .execute(
                        0,
                        "MSET",
                        &[keys[0].clone(), value.clone(), keys[1].clone(), value],
                        4096,
                    )
                    .unwrap();
                let Value::List(items) = engine
                    .execute(0, "MGET", &[first.clone(), second.clone()], 4096)
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(items[0], items[1]);
            }
            done.send(()).unwrap();
        }));
    }
    for _ in 0..4 {
        received
            .recv_timeout(Duration::from_secs(10))
            .expect("lock-order deadlock");
    }
    for worker in workers {
        worker.join().unwrap();
    }
}
