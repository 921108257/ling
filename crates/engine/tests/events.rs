//! 引擎与事件提交边界契约；同一套生命周期断言运行于 Fast 和 Reliable。
//! Engine/event commit-boundary contracts with lifecycle assertions in both Fast and Reliable modes.

use ling_engine::{Clock, Config, Engine, EvictionPolicy, TimeSnapshot};
use ling_events::{EventStore, Subscription};
use ling_types::{DbError, Event, EventKind, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// 可控配对时间，避免依赖到期睡眠；Controllable paired time avoids expiry sleeps.
struct TestClock(Mutex<TimeSnapshot>);
impl TestClock {
    /// 固定墙钟；Fixed wall clock.
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(TimeSnapshot {
            monotonic: Instant::now(),
            unix_ms: 1700000000000,
        })))
    }
    /// 同时推进两种时间；Advances both time sources.
    fn advance(&self, ms: u64) {
        let mut t = self.0.lock().unwrap();
        t.monotonic += Duration::from_millis(ms);
        t.unix_ms += ms as i64;
    }
}
impl Clock for TestClock {
    /// 返回同一个采样；Returns one coherent sample.
    fn now(&self) -> Result<TimeSnapshot, DbError> {
        Ok(*self.0.lock().unwrap())
    }
}
/// 测试拥有临时目录；Test-owned temporary directory.
struct Directory(PathBuf);
impl Directory {
    /// 随机隔离；Random isolation.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "ling-engine-events-{:032x}",
            rand::random::<u128>()
        )))
    }
}
impl Drop for Directory {
    /// 引擎先释放，再清理；Engines drop before cleanup.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// 无隐式类型转换的文本参数；Text arguments without type coercion.
fn s(s: &str) -> Value {
    Value::String(s.into())
}
/// 断言成功，保留合理的响应预算；Asserts success with a realistic response budget.
fn run(e: &Engine, name: &str, args: &[Value]) -> Value {
    e.execute(0, name, args, 4096).unwrap()
}
/// 读取并 ACK，消费组窗口不干扰生命周期断言；Reads and ACKs so windows do not affect lifecycle assertions.
fn take(store: &EventStore, sub: &Subscription) -> Event {
    let event = store.next(sub, Instant::now()).unwrap().unwrap();
    store.ack(sub, event.id).unwrap();
    event
}

/// 六类事件遵守版本、无变化与移动语义。
/// All six event kinds obey revision, no-op, and relocation semantics.
#[test]
fn six_kinds_revisions_noops_and_relocation_in_both_modes() {
    for reliable in [false, true] {
        let dir = Directory::new();
        let c = Config {
            databases: 2,
            shard_count: 2,
            max_keys: 2,
            eviction: EvictionPolicy::Lru,
            ..Config::default()
        };
        let events = Arc::new(
            EventStore::open(
                ling_events::Config {
                    reliable,
                    directory: if reliable {
                        dir.0.clone()
                    } else {
                        PathBuf::new()
                    },
                    ..Default::default()
                },
                2,
                2,
            )
            .unwrap(),
        );
        let clock = TestClock::new();
        let e = Engine::with_events(c, 7, clock.clone(), Some(events.clone())).unwrap();
        let sub = events
            .subscribe(0, vec!["*".into()], Some("contract"))
            .unwrap();
        run(
            &e,
            "SET",
            &[s("a"), Value::Int(1), s("PX"), Value::Int(100)],
        );
        let created = take(&events, &sub);
        assert_eq!(created.kind, EventKind::Created);
        assert_eq!(created.revision, 1);
        run(&e, "SET", &[s("a"), Value::Int(2)]);
        let updated = take(&events, &sub);
        let ttl = take(&events, &sub);
        assert_eq!(
            (updated.kind, ttl.kind),
            (EventKind::Updated, EventKind::TtlChanged)
        );
        assert_eq!((updated.revision, ttl.revision), (2, 2));
        assert_eq!(updated.generation, created.generation);
        run(&e, "SET", &[s("a"), Value::Int(2)]);
        run(&e, "SET", &[s("a"), Value::Int(5), s("NX")]);
        assert!(
            e.execute(0, "INCRBY", &[s("a"), s("invalid")], 4096)
                .is_err()
        );
        assert!(events.next(&sub, Instant::now()).unwrap().is_none());
        run(&e, "PEXPIRE", &[s("a"), Value::Int(10)]);
        assert_eq!(take(&events, &sub).kind, EventKind::TtlChanged);
        clock.advance(10);
        assert!(e.execute(0, "GET", &[s("a")], 4096).is_err());
        assert!(e.managed_bytes() > 0);
        assert_eq!(e.expire_due(1).unwrap(), 1);
        let expired = take(&events, &sub);
        assert_eq!(expired.kind, EventKind::Expired);
        assert_eq!(expired.revision, 3);
        run(&e, "SET", &[s("a"), Value::Int(1)]);
        let recreated = take(&events, &sub);
        assert_ne!(recreated.generation, created.generation);
        run(&e, "RENAME", &[s("a"), s("b")]);
        // 跨分片不承诺全局交付顺序，但两条事实必须完整存在。
        // Cross-shard delivery has no global ordering, but both facts must exist.
        let a = take(&events, &sub);
        let b = take(&events, &sub);
        assert!(matches!(
            (a.kind, b.kind),
            (EventKind::Deleted, EventKind::Created) | (EventKind::Created, EventKind::Deleted)
        ));
        run(&e, "DEL", &[s("b")]);
        assert_eq!(take(&events, &sub).kind, EventKind::Deleted);
        run(&e, "SET", &[s("a"), Value::Null]);
        take(&events, &sub);
        run(&e, "SET", &[s("b"), Value::Null]);
        take(&events, &sub);
        run(&e, "SET", &[s("c"), Value::Null]);
        let a = take(&events, &sub);
        let b = take(&events, &sub);
        assert!(matches!(
            (a.kind, b.kind),
            (EventKind::Evicted, EventKind::Created) | (EventKind::Created, EventKind::Evicted)
        ));
        assert_eq!(events.stats().unwrap().committed, 13);
    }
}

/// 容量拒绝保留原子数据与待记账到期信息。
/// Capacity rejection preserves atomic data and pending expiry accounting.
#[test]
fn journal_backpressure_preserves_atomic_writes_and_expiry_accounting() {
    let dir = Directory::new();
    let clock = TestClock::new();
    let events = Arc::new(
        EventStore::open(
            ling_events::Config {
                reliable: true,
                directory: dir.0.clone(),
                max_bytes: 1024 * 1024,
                ..Default::default()
            },
            2,
            2,
        )
        .unwrap(),
    );
    let e = Engine::with_events(
        Config {
            databases: 2,
            shard_count: 2,
            ..Default::default()
        },
        1,
        clock.clone(),
        Some(events.clone()),
    )
    .unwrap();
    run(&e, "SET", &[s("ttl"), Value::Null, s("PX"), Value::Int(10)]);
    run(&e, "SET", &[s("a"), Value::Int(1)]);
    run(&e, "SET", &[s("b"), Value::Int(1)]);
    let key = "k".repeat(1024);
    loop {
        let change = ling_events::Change {
            db: 1,
            shard: 0,
            kind: EventKind::Created,
            key: key.clone(),
            generation: ling_types::Generation {
                epoch: 1,
                counter: 1,
            },
            revision: 1,
            timestamp_ms: clock.now().unwrap().unix_ms as u64,
        };
        if events.commit(&[change], || ()).is_err() {
            break;
        }
    }
    // 用尽剩余尾部空间，确保较短写入也受背压；Exhaust the remainder so short writes are also rejected.
    let change = ling_events::Change {
        db: 1,
        shard: 0,
        kind: EventKind::Created,
        key: "x".into(),
        generation: ling_types::Generation {
            epoch: 1,
            counter: 1,
        },
        revision: 1,
        timestamp_ms: clock.now().unwrap().unix_ms as u64,
    };
    while events.commit(std::slice::from_ref(&change), || ()).is_ok() {}
    let before = e.managed_bytes();
    for (name, args) in [
        ("MSET", vec![s("a"), Value::Int(2), s("b"), Value::Int(2)]),
        ("RENAME", vec![s("a"), s("new")]),
        ("FLUSHDB", vec![]),
        ("SWAPDB", vec![Value::UInt(0), Value::UInt(1)]),
    ] {
        assert_eq!(
            e.execute(0, name, &args, 4096).unwrap_err().code,
            "ERR_JOURNAL_FULL"
        );
    }
    assert_eq!(run(&e, "GET", &[s("a")]), Value::Int(1));
    assert_eq!(run(&e, "GET", &[s("b")]), Value::Int(1));
    assert_eq!(before, e.managed_bytes());
    clock.advance(10);
    assert_eq!(e.expire_due(1).unwrap_err().code, "ERR_JOURNAL_FULL");
    assert!(e.execute(0, "GET", &[s("ttl")], 4096).is_err());
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    assert_eq!(before, e.managed_bytes());
    assert_eq!(
        e.execute(0, "SET", &[s("ttl"), Value::Null], 4096)
            .unwrap_err()
            .code,
        "ERR_JOURNAL_FULL"
    );
    // 保留到期后允许记账与回收，而不是先忘掉终结事实。
    // After retention permits cleanup, accounting and reclamation proceed without forgetting the terminal fact.
    events
        .maintain(clock.now().unwrap().unix_ms as u64 + 8 * 86400000)
        .unwrap();
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
}

/// 交换分配新身份，清理保留到期原因。
/// Swaps allocate new identities and flushes retain expiry reasons.
#[test]
fn swap_and_flush_assign_new_lifecycles_and_report_expired_reasons() {
    let events = Arc::new(EventStore::open(Default::default(), 2, 2).unwrap());
    let clock = TestClock::new();
    let e = Engine::with_events(
        Config {
            databases: 2,
            shard_count: 2,
            ..Default::default()
        },
        1,
        clock.clone(),
        Some(events.clone()),
    )
    .unwrap();
    run(&e, "SET", &[s("same"), Value::Null]);
    e.execute(
        1,
        "SET",
        &[s("same"), Value::Null, s("PX"), Value::Int(100)],
        4096,
    )
    .unwrap();
    let old = e.metadata(0, "same").unwrap();
    let sub = events.subscribe(0, vec!["*".into()], Some("a")).unwrap();
    let other = events.subscribe(1, vec!["*".into()], Some("b")).unwrap();
    run(&e, "SWAPDB", &[Value::UInt(0), Value::UInt(1)]);
    let a = take(&events, &sub);
    let t = take(&events, &sub);
    assert_eq!(
        (a.kind, t.kind),
        (EventKind::Updated, EventKind::TtlChanged)
    );
    assert_eq!(a.revision, 1);
    assert_ne!(a.generation, old.generation);
    assert_eq!(take(&events, &other).kind, EventKind::Updated);
    assert_eq!(take(&events, &other).kind, EventKind::TtlChanged);
    clock.advance(100);
    run(&e, "FLUSHDB", &[s("ASYNC")]);
    assert_eq!(take(&events, &sub).kind, EventKind::Expired);
    assert!(events.next(&sub, Instant::now()).unwrap().is_none());
}
