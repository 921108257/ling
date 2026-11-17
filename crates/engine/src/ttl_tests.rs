//! 可控双时钟的生命周期契约；不依赖真实睡眠或后台运行时。
//! Lifecycle contracts with a controllable dual clock, independent of real sleeps or background runtimes.

use crate::{Clock, Config, Deadline, Engine, TimeSnapshot};
use ling_types::{DbError, ErrorCode, EventKind, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// 同一个互斥单元保证配对观测一致，允许独立校正墙钟。
/// One mutex keeps paired observations coherent while allowing independent wall-clock corrections.
struct TestClock(Mutex<TimeSnapshot>);
impl TestClock {
    /// 从固定 Unix 时间及真实单调起点建立测试时钟。
    /// Starts at a fixed Unix time and a real monotonic origin.
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(TimeSnapshot {
            monotonic: Instant::now(),
            unix_ms: 1_700_000_000_000,
        })))
    }
    /// 只推进单调时间，模拟系统墙钟独立校正。
    /// Advances monotonic time only to model independent wall-clock correction.
    fn advance(&self, ms: u64) {
        self.0.lock().unwrap().monotonic += Duration::from_millis(ms);
    }
    /// 显式设置墙钟，不改变已经接受的单调期限。
    /// Sets wall time without changing an accepted monotonic deadline.
    fn wall(&self, ms: i64) {
        self.0.lock().unwrap().unix_ms = ms;
    }
}
impl Clock for TestClock {
    /// 返回同一次观测副本；Returns one coherent observation copy.
    fn now(&self) -> Result<TimeSnapshot, DbError> {
        Ok(*self.0.lock().unwrap())
    }
}
/// 构造文本参数；Builds a text argument.
fn s(text: &str) -> Value {
    Value::String(text.into())
}
/// 建立两个分片的可控引擎；Builds a controllable two-shard engine.
fn engine() -> (Engine, Arc<TestClock>) {
    let clock = TestClock::new();
    let engine = Engine::with_clock(
        Config {
            shard_count: 2,
            ..Config::default()
        },
        1,
        clock.clone(),
    )
    .unwrap();
    (engine, clock)
}
/// 使用标准预算执行命令；Executes with the standard test response budget.
fn run(e: &Engine, name: &str, args: &[Value]) -> Value {
    e.execute(0, name, args, 4096).unwrap()
}

/// 使用 Fast 事件统计验证期限提交，两个入口均不依赖磁盘平台。
/// Uses Fast event counters to verify deadline commits on both entry paths without disk dependencies.
fn event_engine() -> (Engine, Arc<TestClock>, Arc<ling_events::EventStore>) {
    let clock = TestClock::new();
    let config = Config {
        databases: 1,
        shard_count: 2,
        ..Config::default()
    };
    let events = Arc::new(ling_events::EventStore::open(Default::default(), 1, 2).unwrap());
    let engine = Engine::with_events(config, 1, clock.clone(), Some(events.clone())).unwrap();
    (engine, clock, events)
}

/// 对秒与毫秒两种单位复用 SET 和独立绝对期限命令。
/// Exercises SET and standalone absolute deadline commands in seconds and milliseconds.
fn set_absolute(e: &Engine, option: &str, target_ms: i64, value: Value) {
    let target = if matches!(option, "EXAT" | "EXPIREAT") {
        target_ms / 1000
    } else {
        target_ms
    };
    if matches!(option, "PXAT" | "EXAT") {
        run(e, "SET", &[s("k"), value, s(option), Value::Int(target)]);
    } else {
        run(e, option, &[s("k"), Value::Int(target)]);
    }
}

/// 毫秒取整的正负亚毫秒抖动不得移动期限、增加修订或多发事件。
/// Positive and negative submillisecond rounding jitter must not move deadlines, revisions, or events.
#[test]
fn absolute_deadline_rounding_preserves_metadata_and_events() {
    for option in ["PEXPIREAT", "EXPIREAT", "PXAT", "EXAT"] {
        let (e, clock, events) = event_engine();
        let target = clock.now().unwrap().unix_ms + 5000;
        clock.0.lock().unwrap().monotonic += Duration::from_micros(750);
        run(&e, "SET", &[s("k"), Value::Int(7)]);
        set_absolute(&e, option, target, Value::Int(7));
        let before = e.metadata(0, "k").unwrap();
        let committed = events.stats().unwrap().committed;
        let ttl_events = events.stats().unwrap().by_kind[EventKind::TtlChanged as usize - 1];
        let start = clock.now().unwrap();
        // 混合相位下墙钟加 1 ms、单调仅加 250 µs，期限差 -750 µs 仍在容差内。
        // A 1 ms wall advance with only 250 µs monotonic advance gives a tolerated -750 µs deadline difference.
        for (micros, wall_ms) in [(250, 0), (250, 1), (750, 1)] {
            {
                let mut time = clock.0.lock().unwrap();
                time.monotonic = start.monotonic + Duration::from_micros(micros);
                time.unix_ms = start.unix_ms + wall_ms;
            }
            set_absolute(&e, option, target, Value::Int(7));
            assert_eq!(e.metadata(0, "k").unwrap(), before, "{option}");
        }
        assert_eq!(events.stats().unwrap().committed, committed, "{option}");
        assert_eq!(
            events.stats().unwrap().by_kind[EventKind::TtlChanged as usize - 1],
            ttl_events
        );
        assert_eq!(e.scheduled_expirations().unwrap(), 1);
    }
}

/// 真实期限或墙钟变化仍提交一次；同期限改值仅生成值事件，相对续期不被归并。
/// Real deadline or wall-clock changes still commit once; same-deadline writes emit only value events and relative renewals stay distinct.
#[test]
fn absolute_deadline_changes_wall_corrections_and_relative_renewals_commit() {
    for (option, correction) in ["PEXPIREAT", "EXPIREAT", "PXAT", "EXAT"]
        .into_iter()
        .flat_map(|option| [1, -1].map(|correction| (option, correction)))
    {
        let (e, clock, events) = event_engine();
        let wall = clock.now().unwrap().unix_ms;
        let target = wall + 5000;
        run(&e, "SET", &[s("k"), Value::Int(7)]);
        set_absolute(&e, option, target, Value::Int(7));
        let before = e.metadata(0, "k").unwrap();
        let committed = events.stats().unwrap().committed;
        clock.wall(wall + correction);
        set_absolute(&e, option, target, Value::Int(7));
        let corrected = e.metadata(0, "k").unwrap();
        assert_eq!(corrected.revision, before.revision + 1, "{option}");
        let expected = if correction > 0 {
            before.deadline.unwrap().monotonic - Duration::from_millis(1)
        } else {
            before.deadline.unwrap().monotonic + Duration::from_millis(1)
        };
        assert_eq!(corrected.deadline.unwrap().monotonic, expected, "{option}");
        assert_eq!(events.stats().unwrap().committed, committed + 1);
        set_absolute(&e, option, target + 1000, Value::Int(7));
        let changed = e.metadata(0, "k").unwrap();
        assert_eq!(changed.revision, corrected.revision + 1);
        assert_eq!(events.stats().unwrap().committed, committed + 2);
        let ttl_events = events.stats().unwrap().by_kind[EventKind::TtlChanged as usize - 1];
        if matches!(option, "PXAT" | "EXAT") {
            clock.0.lock().unwrap().monotonic += Duration::from_micros(250);
            set_absolute(&e, option, target + 1000, Value::Int(8));
            let value_changed = e.metadata(0, "k").unwrap();
            assert_eq!(value_changed.deadline, changed.deadline);
            assert_eq!(value_changed.revision, changed.revision + 1);
            assert_eq!(events.stats().unwrap().committed, committed + 3);
            assert_eq!(
                events.stats().unwrap().by_kind[EventKind::TtlChanged as usize - 1],
                ttl_events
            );
            assert_eq!(
                events.stats().unwrap().by_kind[EventKind::Updated as usize - 1],
                1
            );
        }
        run(&e, "PEXPIRE", &[s("k"), Value::Int(100)]);
        let relative = e.metadata(0, "k").unwrap();
        clock.0.lock().unwrap().monotonic += Duration::from_micros(250);
        let relative_committed = events.stats().unwrap().committed;
        run(&e, "PEXPIRE", &[s("k"), Value::Int(100)]);
        let renewed = e.metadata(0, "k").unwrap();
        assert_eq!(renewed.revision, relative.revision + 1);
        assert_eq!(
            renewed.deadline.unwrap().monotonic,
            relative.deadline.unwrap().monotonic + Duration::from_micros(250)
        );
        assert_eq!(events.stats().unwrap().committed, relative_committed + 1);
        // 相同 Unix 值也必须先判断立即到期，不能复用原来的未来单调期限。
        // Even equal Unix values must expire immediately instead of retaining a future monotonic deadline.
        clock.wall(renewed.deadline.unwrap().unix_ms);
        run(
            &e,
            "PEXPIREAT",
            &[s("k"), Value::Int(clock.now().unwrap().unix_ms)],
        );
        assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-2));
        assert_eq!(events.stats().unwrap().committed, relative_committed + 2);
        assert_eq!(e.scheduled_expirations().unwrap(), 0);
    }
}

/// 到期换算即使与原期限相差不足一毫秒也不得去重，已过期条目不能复活。
/// Immediate expiry must not deduplicate even within one millisecond, and expired entries cannot revive.
#[test]
fn absolute_deadline_rounding_never_hides_immediate_expiry_or_revives_keys() {
    let (e, clock, events) = event_engine();
    let target = clock.now().unwrap().unix_ms + 1;
    set_absolute(&e, "PXAT", target, Value::Null);
    clock.0.lock().unwrap().monotonic += Duration::from_micros(750);
    clock.wall(target);
    let before = e.metadata(0, "k").unwrap();
    assert_eq!(
        e.execute(
            0,
            "SET",
            &[s("k"), Value::Int(9), s("PXAT"), Value::Int(target)],
            4096
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgument.as_str()
    );
    assert_eq!(e.metadata(0, "k").unwrap(), before);
    run(&e, "PEXPIREAT", &[s("k"), Value::Int(target)]);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-2));
    assert_eq!(
        events.stats().unwrap().by_kind[EventKind::Expired as usize - 1],
        1
    );
    assert_eq!(
        events.stats().unwrap().by_kind[EventKind::TtlChanged as usize - 1],
        0
    );
    run(&e, "SET", &[s("k"), Value::Null, s("PX"), Value::Int(1)]);
    clock.advance(1);
    assert_eq!(
        run(&e, "PEXPIREAT", &[s("k"), Value::Int(target + 10)]),
        Value::Bool(false)
    );
    assert_eq!(
        run(
            &e,
            "SET",
            &[
                s("k"),
                Value::Null,
                s("XX"),
                s("PXAT"),
                Value::Int(target + 10)
            ]
        ),
        Value::Bool(false)
    );
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-2));
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
}

/// 到期边界只由单调时钟决定，墙钟前跳或回拨不会使原值复活。
/// Only monotonic time decides expiry; wall-clock jumps never revive the original value.
#[test]
fn logical_expiry_uses_injected_monotonic_time_without_maintenance() {
    let (e, clock) = engine();
    run(&e, "SET", &[s("k"), Value::Null]);
    let at = clock.now().unwrap().monotonic + Duration::from_millis(100);
    e.shards[e.location(0, "k").unwrap()]
        .lock()
        .unwrap()
        .get_mut("k")
        .unwrap()
        .metadata
        .deadline = Some(Deadline {
        monotonic: at,
        unix_ms: 1_700_000_000_100,
    });
    clock.wall(i64::MAX);
    clock.advance(99);
    assert_eq!(run(&e, "GET", &[s("k")]), Value::Null);
    clock.wall(0);
    clock.advance(1);
    assert_eq!(
        e.execute(0, "GET", &[s("k")], 4096).unwrap_err().code,
        "ERR_KEY_NOT_FOUND"
    );
    assert_eq!(run(&e, "EXISTS", &[s("k")]), Value::UInt(0));
    assert_eq!(run(&e, "DBSIZE", &[]), Value::UInt(0));
}

/// 时钟读取失败在发布前拒绝，保留已有数据与版本。
/// Clock failures reject before publication, preserving stored values and revisions.
#[test]
fn clock_failure_never_publishes_a_write() {
    /// 第一次构造成功，后续命令观测失败。
    /// Construction succeeds once; later command observations fail.
    struct FailingClock(std::sync::atomic::AtomicBool);
    impl Clock for FailingClock {
        /// 明确返回稳定错误而不使用伪造时间。
        /// Returns a stable error explicitly rather than fabricating time.
        fn now(&self) -> Result<TimeSnapshot, DbError> {
            if self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Ok(TimeSnapshot {
                    monotonic: Instant::now(),
                    unix_ms: 0,
                })
            } else {
                Err(DbError::new(ErrorCode::Overflow, "Clock unavailable"))
            }
        }
    }
    let e = Engine::with_clock(
        Config::default(),
        1,
        Arc::new(FailingClock(std::sync::atomic::AtomicBool::new(true))),
    )
    .unwrap();
    assert_eq!(
        e.execute(0, "SET", &[s("k"), Value::Null], 4096)
            .unwrap_err()
            .code,
        "ERR_OVERFLOW"
    );
    assert_eq!(e.memory_usage().unwrap(), 0);
}

/// 期限与查询使用毫秒精度，边界到达后所有入口都过滤旧值。
/// Deadlines and queries use millisecond precision; every entry point filters old values at the boundary.
#[test]
fn ttl_queries_round_down_and_expired_entries_cannot_revive() {
    let (e, clock) = engine();
    for name in ["TTL", "PTTL", "EXPIRETIME", "PEXPIRETIME"] {
        assert_eq!(run(&e, name, &[s("k")]), Value::Int(-2));
    }
    run(&e, "SET", &[s("k"), Value::Int(3)]);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-1));
    assert_eq!(
        run(&e, "PEXPIRE", &[s("k"), Value::Int(1001)]),
        Value::Bool(true)
    );
    assert_eq!(run(&e, "TTL", &[s("k")]), Value::Int(1));
    assert_eq!(
        run(&e, "PEXPIRETIME", &[s("k")]),
        Value::Int(1_700_000_001_001)
    );
    clock.advance(2);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(999));
    assert_eq!(run(&e, "TTL", &[s("k")]), Value::Int(0));
    clock.advance(999);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-2));
    assert_eq!(
        run(&e, "PEXPIRE", &[s("k"), Value::Int(9000)]),
        Value::Bool(false)
    );
    assert_eq!(run(&e, "PERSIST", &[s("k")]), Value::Bool(false));
    assert_eq!(
        run(&e, "SET", &[s("k"), Value::Int(4), s("XX")]),
        Value::Bool(false)
    );
    assert_eq!(
        run(
            &e,
            "SET",
            &[s("k"), Value::Int(5), s("NX"), s("EX"), Value::Int(2)]
        ),
        Value::Bool(true)
    );
    assert_eq!(run(&e, "TTL", &[s("k")]), Value::Int(2));
}

/// 选项和数值先完整校验，条件不满足也不能隐藏非法期限。
/// Options and numbers are validated completely before unmet conditions can hide an invalid deadline.
#[test]
fn ttl_invalid_arguments_and_response_limits_preserve_state() {
    let (e, _) = engine();
    run(&e, "SET", &[s("k"), Value::Null]);
    let before = e.metadata(0, "k").unwrap();
    for options in [
        vec![s("PX"), Value::Int(0)],
        vec![s("EX"), Value::Int(-1)],
        vec![s("PXAT"), Value::Int(1)],
        vec![s("EX"), Value::UInt(2)],
        vec![s("NX"), s("XX")],
        vec![s("PX"), Value::Int(1), s("EX"), Value::Int(1)],
        vec![s("PX")],
        vec![s("mystery")],
    ] {
        let mut args = vec![s("k"), Value::Int(9), s("NX")];
        args.extend(options);
        assert_eq!(
            e.execute(0, "SET", &args, 4096).unwrap_err().code,
            "ERR_INVALID_ARGUMENT"
        );
        assert_eq!(e.metadata(0, "k").unwrap(), before);
    }
    for (name, args, code) in [
        (
            "SET",
            vec![s("k"), Value::Int(1), s("EX"), Value::Int(i64::MAX)],
            "ERR_OVERFLOW",
        ),
        (
            "PEXPIRE",
            vec![s("k"), Value::UInt(3)],
            "ERR_INVALID_ARGUMENT",
        ),
        (
            "EXPIRE",
            vec![s("missing"), Value::Int(i64::MAX)],
            "ERR_OVERFLOW",
        ),
        (
            "EXPIRETIME",
            vec![s("k"), Value::Int(0)],
            "ERR_INVALID_ARGUMENT",
        ),
    ] {
        assert_eq!(e.execute(0, name, &args, 4096).unwrap_err().code, code);
        assert_eq!(e.metadata(0, "k").unwrap(), before);
    }
    for name in ["EXPIRE", "PEXPIRE"] {
        assert_eq!(
            e.execute(0, name, &[s("k"), Value::Int(0)], 1)
                .unwrap_err()
                .code,
            "ERR_LIMIT_EXCEEDED"
        );
        assert_eq!(e.metadata(0, "k").unwrap(), before);
    }
    assert_eq!(run(&e, "GET", &[s("k")]), Value::Null);
}

/// 局部修改保留期限，整体替换清除；TTL 变化只增加一次修订。
/// Local mutations preserve deadlines, replacements clear them, and TTL changes increment revisions once.
#[test]
fn ttl_mutation_revisions_and_immediate_expiry_follow_lifecycle() {
    let (e, _) = engine();
    run(
        &e,
        "SET",
        &[s("k"), Value::Int(0), s("PX"), Value::Int(5000)],
    );
    let before = e.metadata(0, "k").unwrap();
    run(&e, "INCR", &[s("k")]);
    let changed = e.metadata(0, "k").unwrap();
    assert_eq!(changed.deadline, before.deadline);
    assert_eq!(changed.revision, before.revision + 1);
    assert_eq!(
        run(
            &e,
            "PEXPIREAT",
            &[s("k"), Value::Int(before.deadline.unwrap().unix_ms)]
        ),
        Value::Bool(true)
    );
    assert_eq!(e.metadata(0, "k").unwrap(), changed);
    assert_eq!(run(&e, "PERSIST", &[s("k")]), Value::Bool(true));
    let persistent = e.metadata(0, "k").unwrap();
    assert_eq!(persistent.generation, before.generation);
    assert_eq!(persistent.revision, changed.revision + 1);
    assert_eq!(run(&e, "PERSIST", &[s("k")]), Value::Bool(false));
    assert_eq!(e.metadata(0, "k").unwrap(), persistent);
    run(&e, "SET", &[s("k"), Value::Int(1), s("PX"), Value::Int(9)]);
    assert_eq!(
        e.metadata(0, "k").unwrap().revision,
        persistent.revision + 1
    );
    run(&e, "MSET", &[s("k"), Value::Int(1)]);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-1));
    for (name, value) in [
        ("EXPIRE", 0),
        ("PEXPIRE", -1),
        ("EXPIREAT", 0),
        ("PEXPIREAT", 1),
    ] {
        run(&e, "SET", &[s("k"), Value::Null]);
        let previous = e.metadata(0, "k").unwrap();
        assert_eq!(
            run(&e, name, &[s("k"), Value::Int(value)]),
            Value::Bool(true)
        );
        assert_eq!(run(&e, "TTL", &[s("k")]), Value::Int(-2));
        run(&e, "SET", &[s("k"), Value::Null]);
        assert_ne!(e.metadata(0, "k").unwrap().generation, previous.generation);
    }
}

/// 绝对期限只在接受时换算；墙钟跃迁后查询期限和剩余时间可以不同步。
/// Absolute deadlines are converted only when accepted; wall-clock jumps may separate expiry timestamps from remaining time.
#[test]
fn absolute_deadlines_ignore_later_wall_clock_jumps() {
    let (e, clock) = engine();
    let target = clock.now().unwrap().unix_ms + 2000;
    run(
        &e,
        "SET",
        &[s("k"), Value::Null, s("PXAT"), Value::Int(target)],
    );
    clock.wall(1);
    clock.advance(500);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(1500));
    assert_eq!(run(&e, "PEXPIRETIME", &[s("k")]), Value::Int(target));
    clock.wall(i64::MAX);
    clock.advance(1500);
    assert_eq!(run(&e, "PTTL", &[s("k")]), Value::Int(-2));
    assert_eq!(run(&e, "KEYS", &[s("*")]), Value::List(vec![]));
    assert_eq!(run(&e, "DEL", &[s("k")]), Value::UInt(0));
}

/// 高频续期和局部写入只保留当前定时项，旧任务不能误删新版本或新生命周期。
/// Frequent renewals and local writes retain only current timers; stale tasks cannot delete new revisions or lifecycles.
#[test]
fn renewal_is_bounded_and_stale_tasks_cannot_delete_current_state() {
    let (e, clock) = engine();
    run(&e, "SET", &[s("k"), Value::Int(0), s("PX"), Value::Int(10)]);
    let id = e.location(0, "k").unwrap();
    let old = crate::expiry::ExpiryTask::new(id, "k", e.metadata(0, "k").unwrap()).unwrap();
    let bytes = e.managed_bytes();
    for i in 11..1011 {
        run(&e, "PEXPIRE", &[s("k"), Value::Int(i)]);
        run(&e, "INCR", &[s("k")]);
        assert_eq!(e.scheduled_expirations().unwrap(), 1);
    }
    assert_eq!(e.managed_bytes(), bytes);
    clock.advance(10);
    assert!(
        !e.lock(vec![id])
            .unwrap()
            .expire_task(&old, clock.now().unwrap())
            .unwrap()
    );
    assert_eq!(run(&e, "GET", &[s("k")]), Value::Int(1000));
    let before_recreate =
        crate::expiry::ExpiryTask::new(id, "k", e.metadata(0, "k").unwrap()).unwrap();
    run(&e, "DEL", &[s("k")]);
    run(
        &e,
        "SET",
        &[s("k"), Value::Int(7), s("PX"), Value::Int(5000)],
    );
    clock.advance(1000);
    assert!(
        !e.lock(vec![id])
            .unwrap()
            .expire_task(&before_recreate, clock.now().unwrap())
            .unwrap()
    );
    assert_eq!(run(&e, "GET", &[s("k")]), Value::Int(7));
    run(&e, "PERSIST", &[s("k")]);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
    clock.advance(5000);
    assert_eq!(e.expire_due(100).unwrap(), 0);
    assert_eq!(run(&e, "GET", &[s("k")]), Value::Int(7));
}

/// 当前修订保留同一期限也要重调度，到期批次严格受工作量预算限制。
/// A new revision retaining its deadline is rescheduled, and due batches obey the work budget.
#[test]
fn expiry_reclaims_memory_in_bounded_batches_with_current_revisions() {
    let (e, clock) = engine();
    assert_eq!(e.maintenance_reclaimed().unwrap(), 0);
    for i in 0..25 {
        run(
            &e,
            "SET",
            &[s(&format!("k{i}")), Value::Int(0), s("PX"), Value::Int(10)],
        );
        run(&e, "INCR", &[s(&format!("k{i}"))]);
    }
    assert_eq!(e.expire_due(10).unwrap(), 0);
    clock.advance(10);
    assert_eq!(run(&e, "DBSIZE", &[]), Value::UInt(0));
    assert!(e.managed_bytes() > 0);
    assert_eq!(e.expire_due(0).unwrap(), 0);
    assert_eq!(e.maintenance_reclaimed().unwrap(), 0);
    assert_eq!(e.expire_due(7).unwrap(), 7);
    assert_eq!(e.maintenance_reclaimed().unwrap(), 7);
    assert_eq!(e.scheduled_expirations().unwrap(), 18);
    assert_eq!(e.expire_due(100).unwrap(), 18);
    assert_eq!(e.maintenance_reclaimed().unwrap(), 25);
    assert_eq!(e.managed_bytes(), 0);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
}

/// 同批首条回收后时钟失败，累计数保留已发布进度，未回收条目仍占预算但逻辑不可见。
/// A clock failure after the first reclamation preserves committed progress; unreclaimed entries retain their charge but remain logically invisible.
#[test]
fn maintenance_count_survives_a_later_batch_clock_error() {
    /// 允许指定次数的成功观测后失败，不向生产时钟添加故障开关。
    /// Fails after a chosen number of successful observations without adding production clock failpoints.
    struct BatchClock {
        time: Arc<TestClock>,
        reads_left: Mutex<Option<usize>>,
    }
    impl Clock for BatchClock {
        /// 未启用故障时复用可控时钟；Uses the controllable clock while no fault is armed.
        fn now(&self) -> Result<TimeSnapshot, DbError> {
            let mut reads_left = self.reads_left.lock().unwrap();
            if let Some(left) = reads_left.as_mut() {
                if *left == 0 {
                    return Err(DbError::new(ErrorCode::Overflow, "Clock unavailable"));
                }
                *left -= 1;
            }
            self.time.now()
        }
    }
    let clock = Arc::new(BatchClock {
        time: TestClock::new(),
        reads_left: Mutex::new(None),
    });
    let e = Engine::with_clock(
        Config {
            databases: 1,
            shard_count: 1,
            ..Config::default()
        },
        1,
        clock.clone(),
    )
    .unwrap();
    for key in ["a", "b"] {
        run(&e, "SET", &[s(key), Value::Int(0), s("PX"), Value::Int(10)]);
    }
    let charged = e.memory_usage().unwrap();
    clock.time.advance(10);
    // 每项先取候选时钟，再在取得分片后重验；第三次观测属于下一项。
    // Each item samples before selection and again after ownership; the third observation starts the next item.
    *clock.reads_left.lock().unwrap() = Some(2);
    let error = e.expire_due(2).unwrap_err();
    assert_eq!(error.code, ErrorCode::Overflow.as_str());
    assert_eq!(e.maintenance_reclaimed().unwrap(), 1);
    assert_eq!(e.memory_usage().unwrap(), charged / 2);
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    *clock.reads_left.lock().unwrap() = None;
    for key in ["a", "b"] {
        assert_eq!(
            e.metadata(0, key).unwrap_err().code,
            ErrorCode::KeyNotFound.as_str()
        );
    }
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    assert_eq!(e.expire_due(2).unwrap(), 1);
    assert_eq!(e.maintenance_reclaimed().unwrap(), 2);
    assert_eq!(e.memory_usage().unwrap(), 0);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
    assert_eq!(run(&e, "DBSIZE", &[]), Value::UInt(0));
    run(&e, "SET", &[s("c"), Value::Null, s("PX"), Value::Int(1)]);
    clock.time.advance(1);
    e.budget.lock().unwrap().reclaimed_by_maintenance = u64::MAX;
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.maintenance_reclaimed().unwrap(), u64::MAX);
}

/// 复制、移动、重命名与交换沿用原期限，为新存储位置发布新定时身份。
/// Copy, move, rename, and swap retain deadlines while publishing fresh timer identities at destination locations.
#[test]
fn relocation_and_flush_keep_timer_ownership_consistent() {
    let (e, clock) = engine();
    run(&e, "SET", &[s("a"), Value::Null, s("PX"), Value::Int(20)]);
    let original = e.metadata(0, "a").unwrap();
    run(&e, "COPY", &[s("a"), s("b"), s("DB"), Value::UInt(1)]);
    run(&e, "MOVE", &[s("a"), Value::UInt(2)]);
    e.execute(2, "RENAME", &[s("a"), s("renamed")], 4096)
        .unwrap();
    run(&e, "SWAPDB", &[Value::UInt(1), Value::UInt(2)]);
    assert_eq!(e.scheduled_expirations().unwrap(), 2);
    assert_eq!(
        e.metadata(1, "renamed").unwrap().deadline,
        original.deadline
    );
    assert_eq!(e.metadata(2, "b").unwrap().deadline, original.deadline);
    clock.advance(20);
    assert_eq!(e.expire_due(10).unwrap(), 2);
    assert_eq!(e.managed_bytes(), 0);
    run(&e, "SET", &[s("c"), Value::Null, s("PX"), Value::Int(20)]);
    run(&e, "FLUSHDB", &[s("ASYNC")]);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
    clock.advance(20);
    assert_eq!(e.expire_due(10).unwrap(), 0);
    run(&e, "SET", &[s("d"), Value::Null, s("PX"), Value::Int(20)]);
    run(&e, "FLUSHALL", &[]);
    assert_eq!(e.scheduled_expirations().unwrap(), 0);
    assert_eq!(e.maintenance_reclaimed().unwrap(), 2);
}

/// 失败写入不发布定时项，ttl-first 淘汰会同时移除原条目的调度信息。
/// Failed writes publish no timers; ttl-first eviction also removes the victim's scheduling metadata.
#[test]
fn ttl_first_and_rejected_commands_do_not_leave_timer_garbage() {
    let clock = TestClock::new();
    let e = Engine::with_clock(
        Config {
            shard_count: 2,
            max_keys: 1,
            eviction: crate::EvictionPolicy::TtlFirst,
            ..Config::default()
        },
        1,
        clock.clone(),
    )
    .unwrap();
    run(
        &e,
        "SET",
        &[s("old"), Value::Null, s("PX"), Value::Int(100)],
    );
    assert_eq!(
        e.execute(
            0,
            "SET",
            &[s("new"), Value::Null, s("PX"), Value::Int(10)],
            1
        )
        .unwrap_err()
        .code,
        "ERR_LIMIT_EXCEEDED"
    );
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    assert_eq!(run(&e, "PTTL", &[s("old")]), Value::Int(100));
    run(&e, "SET", &[s("new"), Value::Null, s("PX"), Value::Int(10)]);
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    clock.advance(10);
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.managed_bytes(), 0);
}

/// 耗尽的修订或创建计数不能发布半条变更，溢出的期限不能清除原定时项。
/// Exhausted revision or creation counters never publish partial changes; overflowing deadlines preserve timers.
#[test]
fn exhausted_versions_and_deadline_overflow_preserve_state() {
    let (e, clock) = engine();
    run(
        &e,
        "SET",
        &[s("k"), Value::Int(0), s("PX"), Value::Int(100)],
    );
    let id = e.location(0, "k").unwrap();
    {
        let mut index = e.shards[id].lock().unwrap();
        let entry = index.get_mut("k").unwrap();
        let old = entry.metadata;
        entry.metadata.revision = u64::MAX;
        e.budget
            .lock()
            .unwrap()
            .schedule(id, "k", Some(old), Some(entry.metadata));
    }
    let before = e.metadata(0, "k").unwrap();
    let bytes = e.managed_bytes();
    for (name, args) in [
        ("PERSIST", vec![s("k")]),
        ("PEXPIRE", vec![s("k"), Value::Int(200)]),
        ("INCR", vec![s("k")]),
        ("SET", vec![s("k"), Value::Int(1)]),
    ] {
        assert_eq!(
            e.execute(0, name, &args, 4096).unwrap_err().code,
            "ERR_OVERFLOW"
        );
        assert_eq!(e.metadata(0, "k").unwrap(), before);
        assert_eq!(e.scheduled_expirations().unwrap(), 1);
        assert_eq!(e.managed_bytes(), bytes);
    }
    // 不改变状态的命令无需新版本，仍然成功；终结生命周期也无需递增。
    // No-op commands need no new revision and still succeed; lifecycle termination does not increment it either.
    assert_eq!(
        run(&e, "PEXPIRE", &[s("k"), Value::Int(100)]),
        Value::Bool(true)
    );
    clock.wall(i64::MAX);
    assert_eq!(
        e.execute(0, "PEXPIRE", &[s("k"), Value::Int(1)], 4096)
            .unwrap_err()
            .code,
        "ERR_OVERFLOW"
    );
    assert_eq!(e.metadata(0, "k").unwrap(), before);
    clock.wall(i64::MIN);
    assert_eq!(
        e.execute(0, "PEXPIREAT", &[s("k"), Value::Int(i64::MAX)], 4096)
            .unwrap_err()
            .code,
        "ERR_OVERFLOW"
    );
    clock.wall(1_700_000_000_000);
    e.budget.lock().unwrap().creations = u64::MAX;
    assert_eq!(
        e.execute(
            0,
            "MSET",
            &[s("k"), Value::Int(0), s("new"), Value::Null],
            4096
        )
        .unwrap_err()
        .code,
        "ERR_OVERFLOW"
    );
    assert_eq!(e.metadata(0, "k").unwrap(), before);
    assert_eq!(run(&e, "EXISTS", &[s("new")]), Value::UInt(0));
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    clock.advance(100);
    assert_eq!(
        e.execute(0, "SET", &[s("k"), Value::Null, s("NX")], 4096)
            .unwrap_err()
            .code,
        "ERR_OVERFLOW"
    );
    assert_eq!(e.scheduled_expirations().unwrap(), 1);
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.managed_bytes(), 0);
}

/// 每个身份或期限字段均须匹配，丢弃过时任务不能移除当前定时项。
/// Every identity and deadline field must match; discarding stale work cannot remove the current timer.
#[test]
fn expiry_validates_every_identity_and_deadline_component() {
    let (e, clock) = engine();
    run(&e, "SET", &[s("k"), Value::Null, s("PX"), Value::Int(10)]);
    let id = e.location(0, "k").unwrap();
    let original = e.metadata(0, "k").unwrap();
    let bytes = e.managed_bytes();
    clock.advance(20);
    for field in 0..5 {
        let mut stale = original;
        match field {
            0 => stale.generation.epoch += 1,
            1 => stale.generation.counter += 1,
            2 => stale.revision += 1,
            3 => stale.deadline.as_mut().unwrap().unix_ms += 1,
            _ => stale.deadline.as_mut().unwrap().monotonic += Duration::from_millis(1),
        }
        let task = crate::expiry::ExpiryTask::new(id, "k", stale).unwrap();
        assert!(
            !e.lock(vec![id])
                .unwrap()
                .expire_task(&task, clock.now().unwrap())
                .unwrap()
        );
        assert_eq!(e.scheduled_expirations().unwrap(), 1);
        assert_eq!(e.managed_bytes(), bytes);
    }
    assert_eq!(e.expire_due(1).unwrap(), 1);
    assert_eq!(e.managed_bytes(), 0);
}

/// 到期边界上，后台回收与条件写入、删除、复制和扫描共享同一可见性规则。
/// At expiry, reclamation races with conditional writes, deletion, copy, and scan under one visibility rule.
#[test]
fn concurrent_expiry_never_revives_or_deletes_new_lifecycles() {
    let (e, clock) = engine();
    let e = Arc::new(e);
    for round in 0..32 {
        run(&e, "FLUSHALL", &[]);
        for key in ["nx", "xx", "gone-del", "gone-copy", "gone-scan"] {
            run(&e, "SET", &[s(key), Value::Int(-1), s("PX"), Value::Int(1)]);
        }
        clock.advance(1);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let reclaim_engine = e.clone();
        let reclaim_barrier = barrier.clone();
        let reclaim = std::thread::spawn(move || {
            reclaim_barrier.wait();
            for _ in 0..8 {
                reclaim_engine.expire_due(1).unwrap();
                std::thread::yield_now();
            }
        });
        let commands_engine = e.clone();
        let commands_barrier = barrier.clone();
        let commands = std::thread::spawn(move || {
            commands_barrier.wait();
            assert_eq!(
                run(
                    &commands_engine,
                    "SET",
                    &[s("nx"), Value::Int(round), s("NX")]
                ),
                Value::Bool(true)
            );
            assert_eq!(
                run(&commands_engine, "SET", &[s("xx"), Value::Null, s("XX")]),
                Value::Bool(false)
            );
            assert_eq!(
                run(&commands_engine, "DEL", &[s("gone-del")]),
                Value::UInt(0)
            );
            assert_eq!(
                commands_engine
                    .execute(0, "COPY", &[s("gone-copy"), s("copy")], 4096)
                    .unwrap_err()
                    .code,
                "ERR_KEY_NOT_FOUND"
            );
            let mut scan = crate::ScanState::default();
            let Value::Map(page) = commands_engine
                .scan(
                    0,
                    &[
                        Value::UInt(0),
                        s("MATCH"),
                        s("gone-*"),
                        s("COUNT"),
                        Value::UInt(100),
                    ],
                    &mut scan,
                    4096,
                )
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(page["keys"], Value::List(vec![]));
            assert_eq!(page["cursor"], Value::UInt(0));
        });
        barrier.wait();
        reclaim.join().unwrap();
        commands.join().unwrap();
        assert_eq!(run(&e, "GET", &[s("nx")]), Value::Int(round));
        assert_eq!(run(&e, "DBSIZE", &[]), Value::UInt(1));
        assert_eq!(e.scheduled_expirations().unwrap(), 0);
    }
}
