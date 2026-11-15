//! 事件消费与磁盘故障契约；使用真实临时目录和独立黄金字节。
//! Event consumption and disk-failure contracts using real temporary directories and independent golden bytes.

use super::*;
use linger_types::{ErrorCode, EventId};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// 测试目录自动清理，不依赖平台临时目录 crate；Self-cleaning test directory without an extra dependency.
struct Directory(PathBuf);
impl Directory {
    /// 隔离并行测试；Isolates parallel tests.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("linger-events-{:032x}", rand::random::<u128>())))
    }
    /// 小磁盘预算促成可重复背压；A small disk budget makes backpressure reproducible.
    fn config(&self) -> Config {
        Config {
            reliable: true,
            directory: self.0.clone(),
            max_bytes: 1024 * 1024,
            ..Config::default()
        }
    }
}
impl Drop for Directory {
    /// 仅删除当前测试创建的目录；Removes only this test's directory.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// 合法变更，不使用业务 Value；Valid change without a cached Value.
fn change(key: &str, shard: u32) -> Change {
    Change {
        db: 0,
        shard,
        kind: EventKind::Created,
        key: key.into(),
        generation: Generation {
            epoch: 1,
            counter: 1,
        },
        revision: 1,
        timestamp_ms: 1000,
    }
}
/// 对稳定错误码断言；Asserts stable error codes.
fn code<T: std::fmt::Debug>(result: Result<T, linger_types::DbError>, code: ErrorCode) {
    assert_eq!(result.unwrap_err().code, code.as_str());
}

#[test]
fn independent_disk_vector_and_truncated_lengths() {
    // 手工定义字段，不调用生产编码器构造期望值；Fields are manual, not produced by the encoder under test.
    let hex = include_str!("../../../tests/fixtures/journal/event-v1.hex").trim();
    let expected: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let e = linger_types::Event {
        id: EventId {
            stream_id: 2,
            db: 0,
            shard: 0,
            sequence: 1,
        },
        kind: EventKind::Created,
        key: "k".into(),
        generation: Generation {
            epoch: 1,
            counter: 1,
        },
        revision: 1,
        timestamp_ms: 1000,
    };
    assert_eq!(codec::events(std::slice::from_ref(&e)), expected);
    assert_eq!(codec::read_events(&expected, 2).unwrap(), vec![e]);
    for end in 0..expected.len() {
        assert!(codec::read_events(&expected[..end], 2).is_err());
    }
}

#[test]
fn ack_holes_window_sharing_and_group_isolation() {
    let store = EventStore::open(
        Config {
            max_pending: 2,
            ..Config::default()
        },
        1,
        2,
    )
    .unwrap();
    let a = store
        .subscribe(0, vec!["a*".into(), "*".into()], Some("g"))
        .unwrap();
    let b = store
        .subscribe(0, vec!["*".into(), "a*".into()], Some("g"))
        .unwrap();
    let independent = store.subscribe(0, vec!["*".into()], Some("other")).unwrap();
    store
        .commit(&[change("a", 0), change("b", 0), change("c", 0)], || ())
        .unwrap();
    let now = Instant::now();
    let first = store.next(&a, now).unwrap().unwrap();
    let second = store.next(&b, now).unwrap().unwrap();
    assert_eq!((first.id.sequence, second.id.sequence), (1, 2));
    store.ack(&b, second.id).unwrap();
    assert!(store.next(&a, now).unwrap().is_none());
    assert_eq!(store.next(&independent, now).unwrap().unwrap().id, first.id);
    store.ack(&a, first.id).unwrap();
    store.ack(&a, first.id).unwrap();
    assert_eq!(store.next(&b, now).unwrap().unwrap().id.sequence, 3);
    code(
        store.ack(
            &a,
            EventId {
                sequence: 9,
                ..first.id
            },
        ),
        ErrorCode::InvalidAck,
    );
}

#[test]
fn lease_redelivery_and_late_ack_use_original_id() {
    let store = EventStore::open(
        Config {
            consumer_timeout_ms: 20,
            retry_interval_ms: 10,
            ..Config::default()
        },
        1,
        1,
    )
    .unwrap();
    let a = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    let b = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let now = Instant::now();
    let first = store.next(&a, now).unwrap().unwrap();
    assert!(
        store
            .next(&b, now + Duration::from_millis(19))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .next(&b, now + Duration::from_millis(20))
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    store.ack(&a, first.id).unwrap();
    store.ack(&b, first.id).unwrap();
    assert_eq!(store.stats().unwrap().redelivered, 1);
}

#[test]
fn reliable_recovery_keeps_holes_stream_and_sequences() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 2).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store
        .commit(&[change("a", 0), change("b", 0), change("c", 1)], || ())
        .unwrap();
    let a = store.next(&sub, Instant::now()).unwrap().unwrap();
    let b = store.next(&sub, Instant::now()).unwrap().unwrap();
    store.ack(&sub, b.id).unwrap();
    let stream = store.stream_id().unwrap();
    drop(store);
    let store = EventStore::open(dir.config(), 1, 2).unwrap();
    assert_eq!(store.stream_id().unwrap(), stream);
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    assert_eq!(store.next(&sub, Instant::now()).unwrap().unwrap().id, a.id);
    store.ack(&sub, a.id).unwrap();
    assert_eq!(
        store.next(&sub, Instant::now()).unwrap().unwrap().id.shard,
        1
    );
    store.commit(&[change("d", 0)], || ()).unwrap();
    assert_eq!(
        store
            .next(&sub, Instant::now())
            .unwrap()
            .unwrap()
            .id
            .sequence,
        3
    );
}

#[test]
fn sync_failure_is_unknown_and_globally_faults_writes_and_ack() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let e = store.next(&sub, Instant::now()).unwrap().unwrap();
    store.fail_next_sync();
    let mut published = false;
    code(
        store.commit(&[change("b", 0)], || {
            published = true;
        }),
        ErrorCode::CommitUnknown,
    );
    assert!(!published);
    code(
        store.commit(&[change("c", 0)], || ()),
        ErrorCode::JournalUnavailable,
    );
    code(store.ack(&sub, e.id), ErrorCode::JournalUnavailable);
    drop(store);
    // 完整但同步结果未知的记录可能幸存；A complete record with an unknown sync outcome may survive.
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    assert_eq!(store.next(&sub, Instant::now()).unwrap().unwrap().id, e.id);
    store.ack(&sub, e.id).unwrap();
    assert_eq!(store.next(&sub, Instant::now()).unwrap().unwrap().key, "b");
}

#[test]
fn incomplete_tail_recovers_but_complete_corruption_and_wrong_topology_fail() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    assert!(EventStore::open(dir.config(), 1, 1).is_err());
    drop(store);
    let path = dir.0.join("journal.lgj");
    let valid = fs::read(&path).unwrap();
    for tail in [vec![0, 0], vec![0, 0, 0, 20, 0, 0, 0, 0, 1, 2]] {
        let mut bytes = valid.clone();
        bytes.extend(tail);
        fs::write(&path, bytes).unwrap();
        drop(EventStore::open(dir.config(), 1, 1).unwrap());
        assert_eq!(fs::read(&path).unwrap(), valid);
    }
    code(
        EventStore::open(dir.config(), 2, 1).map(|_| ()),
        ErrorCode::ConfigInvalid,
    );
    let mut corrupt = valid;
    corrupt[48] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(EventStore::open(dir.config(), 1, 1).is_err());
}

#[test]
fn retention_compaction_and_capacity_backpressure_preserve_history() {
    let dir = Directory::new();
    let config = Config {
        retention_ms: 100,
        ..dir.config()
    };
    let store = EventStore::open(config.clone(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("slow")).unwrap();
    let mut count = 0;
    loop {
        match store.commit(&[change(&"k".repeat(1024), 0)], || ()) {
            Ok(()) => count += 1,
            Err(e) => {
                assert_eq!(e.code, ErrorCode::JournalFull.as_str());
                break;
            }
        }
    }
    assert!(count > 100);
    assert!(store.stats().unwrap().journal_bytes <= config.max_bytes / 2);
    assert!(store.next(&sub, Instant::now()).unwrap().is_some());
    store.maintain(1100).unwrap();
    code(store.next(&sub, Instant::now()), ErrorCode::OffsetExpired);
    let stream = store.stream_id().unwrap();
    drop(store);
    let store = EventStore::open(config, 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("new")).unwrap();
    store.commit(&[change("new", 0)], || ()).unwrap();
    let event = store.next(&sub, Instant::now()).unwrap().unwrap();
    assert_eq!(event.id.stream_id, stream);
    assert_eq!(event.id.sequence, count + 1);
}

#[test]
fn fast_drops_are_counted_and_offsets_expire() {
    let store = EventStore::open(
        Config {
            buffer_bytes: 65536,
            max_pending: 8,
            max_groups: 8,
            ..Config::default()
        },
        1,
        1,
    )
    .unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    for _ in 0..300 {
        store.commit(&[change("a", 0)], || ()).unwrap();
    }
    assert!(store.stats().unwrap().dropped > 0);
    code(store.next(&sub, Instant::now()), ErrorCode::OffsetExpired);
    assert!(
        store
            .subscribe(0, vec!["different".into()], Some("g"))
            .is_err()
    );
}
