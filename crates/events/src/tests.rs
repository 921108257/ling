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

/// 独立字节固定磁盘字段宽度，所有截断均拒绝。
/// Independent bytes freeze disk field widths and reject every truncated payload.
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
    assert!(codec::read_events(&[codec::EVENTS, 0, 0, 0, 0], 2).is_err());
    for end in 0..expected.len() {
        assert!(codec::read_events(&expected[..end], 2).is_err());
    }
}

/// 乱序 ACK 保留空洞，窗口由组内消费者共享，不同组独立。
/// Out-of-order ACKs preserve holes; windows are shared within a group and isolated across groups.
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

/// 租约到期重投原身份，旧消费者晚 ACK 仍有效。
/// Expired leases redeliver original IDs and late ACKs remain valid.
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

/// 恢复确认空洞并延续流身份和序号。
/// Recovery preserves ACK holes and continues stream identity and sequences.
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

/// 模拟同步失败时不发布回调，禁止后续日志和 ACK。
/// Simulated sync failure prevents publication and faults subsequent Journal writes and ACKs.
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

/// 仅不完整尾部可修复，完整损坏、目录锁与拓扑冲突明确失败。
/// Only incomplete tails are repaired; corruption, locks, and topology conflicts fail explicitly.
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
    let error = EventStore::open(dir.config(), 1, 1).err().unwrap();
    assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
    assert!(error.message.contains("byte offset 40"));
}

/// 完整且校验正确的空组名仍不符合恢复契约。
/// A complete checksummed record with an empty group name still violates recovery contracts.
#[test]
fn empty_named_group_is_rejected_during_recovery() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    drop(store);
    let bytes = fs::read(dir.0.join("journal.lgj")).unwrap();
    let len = u32::from_be_bytes(bytes[40..44].try_into().unwrap()) as usize;
    let (_, group) = codec::read_group(&bytes[48..48 + len]).unwrap();
    let (mut journal, _) = journal::Journal::open(&dir.config(), 1, 1).unwrap();
    journal.append(&codec::group("n:", &group), true).unwrap();
    drop(journal);
    code(
        EventStore::open(dir.config(), 1, 1).map(|_| ()),
        ErrorCode::JournalUnavailable,
    );
}

/// 容量背压不缩短保留，检查点继续序号并标记过期组。
/// Capacity backpressure preserves retention; checkpoints continue sequences and expire lagging groups.
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

/// Fast 容量丢弃必须计数并报告旧消费位置失效。
/// Fast capacity drops are counted and old offsets fail explicitly.
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

/// 中断多分片记录不会恢复其中一半。
/// Interrupted multi-shard records never recover partially.
#[test]
fn interrupted_multi_shard_transaction_is_never_partly_recovered() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 2).unwrap();
    let sub = store
        .subscribe(0, vec!["*".into()], Some("atomic"))
        .unwrap();
    store
        .commit(&[change("left", 0), change("right", 1)], || ())
        .unwrap();
    assert_eq!(
        store.next(&sub, Instant::now()).unwrap().unwrap().key,
        "left"
    );
    drop(store);
    let path = dir.0.join("journal.lgj");
    let bytes = fs::read(&path).unwrap();
    let mut at = 40;
    let mut transaction = 0;
    while at < bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 8] == codec::EVENTS {
            transaction = at;
            break;
        }
        at += len + 16;
    }
    assert!(transaction > 40);
    // 中断在第二个事件之前，完整事务都应被截断；Interrupt before the second event and discard the complete transaction.
    fs::write(&path, &bytes[..transaction + 8 + 5 + 64]).unwrap();
    let store = EventStore::open(dir.config(), 1, 2).unwrap();
    let sub = store
        .subscribe(0, vec!["*".into()], Some("atomic"))
        .unwrap();
    assert!(store.next(&sub, Instant::now()).unwrap().is_none());
    store.commit(&[change("new", 0)], || ()).unwrap();
    assert_eq!(
        store
            .next(&sub, Instant::now())
            .unwrap()
            .unwrap()
            .id
            .sequence,
        1
    );
}

/// 正确 CRC 不能让重复流序号合法化。
/// A valid CRC cannot legitimize duplicate stream sequences.
#[test]
fn complete_duplicate_sequence_is_rejected_even_with_valid_checksum() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.commit(&[change("key", 0)], || ()).unwrap();
    drop(store);
    let path = dir.0.join("journal.lgj");
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_within(40..);
    fs::write(path, bytes).unwrap();
    assert!(EventStore::open(dir.config(), 1, 1).is_err());
}

/// 组创建和 ACK 的同步失败也必须暴露结果未知。
/// Sync failures during group creation and ACK must also expose unknown outcomes.
#[test]
fn ack_sync_failure_and_group_creation_failure_keep_unknown_outcomes() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.fail_next_sync();
    code(
        store.subscribe(0, vec!["*".into()], Some("g")).map(|_| ()),
        ErrorCode::CommitUnknown,
    );
    drop(store);
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let event = store.next(&sub, Instant::now()).unwrap().unwrap();
    store.fail_next_sync();
    code(store.ack(&sub, event.id), ErrorCode::CommitUnknown);
    drop(store);
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    assert!(store.next(&sub, Instant::now()).unwrap().is_none());
    store.ack(&sub, event.id).unwrap();
}

/// 断线重投遵循间隔，组增长不侵占事件队列。
/// Disconnected redelivery honors its interval and group growth does not consume the event queue.
#[test]
fn disconnected_lease_and_group_growth_do_not_silently_drop_fast_events() {
    let store = EventStore::open(
        Config {
            buffer_bytes: 65536,
            max_pending: 8,
            max_groups: 32,
            retry_interval_ms: 10,
            consumer_timeout_ms: 100,
            ..Default::default()
        },
        1,
        1,
    )
    .unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("k", 0)], || ()).unwrap();
    let now = Instant::now();
    let first = store.next(&sub, now).unwrap().unwrap();
    store.disconnect(&sub);
    let next = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    assert!(
        store
            .next(&next, now + Duration::from_millis(9))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .next(&next, now + Duration::from_millis(10))
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    for n in 0..16 {
        store
            .subscribe(0, vec!["*".into()], Some(&format!("g{n}")))
            .unwrap();
    }
    assert_eq!(store.stats().unwrap().dropped, 0);
    store.ack(&next, first.id).unwrap();
}

/// 保留维护不把零时间误判为已超期。
/// Retention maintenance never mistakes timestamp zero for expired history.
#[test]
fn maintenance_never_expires_timestamp_zero_before_retention() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    let mut c = change("zero", 0);
    c.timestamp_ms = 0;
    store.commit(&[c], || ()).unwrap();
    store.maintain(0).unwrap();
    assert_eq!(
        store.next(&sub, Instant::now()).unwrap().unwrap().key,
        "zero"
    );
}

/// 真实文件权限变化拒绝重新打开，不创建替代流；Real permission changes reject reopen without creating a substitute stream.
#[cfg(unix)]
#[test]
fn read_only_journal_refuses_startup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.commit(&[change("k", 0)], || ()).unwrap();
    drop(store);
    let path = dir.0.join("journal.lgj");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    // root 可以绕过普通权限；仅在权限实际被执行的用户环境断言拒绝。
    // Root may bypass ordinary permissions; assert rejection only when permissions are actually enforced.
    if fs::OpenOptions::new().write(true).open(&path).is_err() {
        assert!(EventStore::open(dir.config(), 1, 1).is_err());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// 可观测计数与消费水位不含业务内容；Observable counts and consumer watermarks contain no business contents.
#[test]
fn stats_keep_drop_sync_failures_and_group_progress_separate() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let event = store.next(&sub, Instant::now()).unwrap().unwrap();
    let groups = store.group_stats(Instant::now()).unwrap();
    assert_eq!(groups[0].pending, 1);
    assert_eq!(groups[0].watermarks, vec![(0, 0)]);
    store.fail_next_sync();
    assert!(store.ack(&sub, event.id).is_err());
    let stats = store.stats().unwrap();
    assert_eq!(stats.by_kind, [1, 0, 0, 0, 0, 0]);
    assert_eq!(stats.journal_errors, 1);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.oldest_retained_ms, Some(1000));
    code(store.maintain(0), ErrorCode::JournalUnavailable);
}

/// 事务提交回调结束前，独立消费者不能看到事件。
/// Consumers cannot observe an event until its transaction publication callback finishes.
#[test]
fn consumer_visibility_waits_for_complete_publication() {
    use std::sync::{Arc, mpsc};
    let dir = Directory::new();
    let store = Arc::new(EventStore::open(dir.config(), 1, 2).unwrap());
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let writing = store.clone();
    let writer = std::thread::spawn(move || {
        writing
            .commit(&[change("left", 0), change("right", 1)], || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            })
            .unwrap();
    });
    ready.recv().unwrap();
    let (got, done) = mpsc::channel();
    let consuming = store.clone();
    let reader = std::thread::spawn(move || {
        let event = consuming.next(&sub, Instant::now()).unwrap().unwrap();
        got.send(event).unwrap();
    });
    assert!(done.recv_timeout(Duration::from_millis(20)).is_err());
    release.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(
        done.recv_timeout(Duration::from_secs(2)).unwrap().key,
        "left"
    );
    reader.join().unwrap();
}

/// 物理读游标不能跳过同事务后继；压缩及重启仍由逻辑水位恢复。
/// Physical read cursors never skip transaction siblings; compaction and restart use logical watermarks.
#[test]
fn sequential_reader_preserves_filtered_siblings_and_compaction_recovery() {
    let dir = Directory::new();
    let config = dir.config();
    let store = EventStore::open(config.clone(), 1, 2).unwrap();
    let sub = store
        .subscribe(0, vec!["match:*".into()], Some("reader"))
        .unwrap();
    store
        .commit(
            &[
                change("skip", 0),
                change("match:a", 1),
                change("match:b", 0),
            ],
            || (),
        )
        .unwrap();
    let first = store.next(&sub, Instant::now()).unwrap().unwrap();
    assert_eq!(first.key, "match:a");
    store.ack(&sub, first.id).unwrap();
    store.maintain(1100).unwrap();
    let second = store.next(&sub, Instant::now()).unwrap().unwrap();
    assert_eq!(second.key, "match:b");
    store.ack(&sub, second.id).unwrap();
    assert!(store.next(&sub, Instant::now()).unwrap().is_none());
    // 已到 EOF 后追加仍可见；Appending after EOF remains visible.
    store.commit(&[change("match:c", 1)], || ()).unwrap();
    let third = store.next(&sub, Instant::now()).unwrap().unwrap();
    assert_eq!(third.key, "match:c");
    drop(store);
    let store = EventStore::open(config, 1, 2).unwrap();
    let sub = store
        .subscribe(0, vec!["match:*".into()], Some("reader"))
        .unwrap();
    let retry = store.next(&sub, Instant::now()).unwrap().unwrap();
    assert_eq!(retry.id, third.id);
    store.ack(&sub, retry.id).unwrap();
    assert!(store.next(&sub, Instant::now()).unwrap().is_none());
}

/// ACK 空间触发的压缩同样重置尚未发布候选的物理游标。
/// Compaction triggered by ACK space also resets unpublished candidates' physical cursors.
#[test]
fn control_capacity_compaction_does_not_republish_stale_read_offset() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store
        .subscribe(0, vec!["*".into()], Some("capacity"))
        .unwrap();
    for n in 0..2500 {
        store
            .commit(&[change(&format!("k{n:04}"), 0)], || ())
            .unwrap();
    }
    for n in 0..2500 {
        let event = store.next(&sub, Instant::now()).unwrap().unwrap();
        assert_eq!(event.key, format!("k{n:04}"));
        store.ack(&sub, event.id).unwrap();
    }
    assert!(store.next(&sub, Instant::now()).unwrap().is_none());
    assert_eq!(store.stats().unwrap().pending, 0);
    assert!(store.stats().unwrap().journal_bytes <= 1024 * 1024 / 2);
}

/// 未达到保留期限的维护不改写日志；到期后仍压缩并保留后继事件。
/// Maintenance leaves unexpired journals untouched, then compacts due prefixes while retaining successors.
#[test]
fn maintenance_only_rewrites_when_retention_can_advance() {
    let dir = Directory::new();
    let mut config = dir.config();
    config.retention_ms = 1000;
    let store = EventStore::open(config.clone(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    let mut first = change("first", 0);
    first.timestamp_ms = 0;
    store.commit(&[first], || ()).unwrap();
    let mut second = change("second", 0);
    second.timestamp_ms = 500;
    store.commit(&[second], || ()).unwrap();
    let path = dir.0.join("journal.lgj");
    let before = fs::read(&path).unwrap();
    store.maintain(999).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(store.stats().unwrap().oldest_retained_ms, Some(0));
    store.maintain(1000).unwrap();
    assert_eq!(store.stats().unwrap().oldest_retained_ms, Some(500));
    code(store.next(&sub, Instant::now()), ErrorCode::OffsetExpired);
    drop(store);
    let store = EventStore::open(config, 1, 1).unwrap();
    assert_eq!(store.stats().unwrap().oldest_retained_ms, Some(500));
    let retained = fs::read(&path).unwrap();
    store.maintain(1499).unwrap();
    assert_eq!(fs::read(&path).unwrap(), retained);
    store.maintain(1500).unwrap();
    assert_eq!(store.stats().unwrap().oldest_retained_ms, None);
}
