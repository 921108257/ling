//! 可控双时钟的生命周期契约；不依赖真实睡眠或后台运行时。
//! Lifecycle contracts with a controllable dual clock, independent of real sleeps or background runtimes.

use crate::{Clock, Config, Deadline, Engine, TimeSnapshot};
use linger_types::{DbError, ErrorCode, Value};
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
