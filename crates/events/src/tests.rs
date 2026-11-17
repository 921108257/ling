//! 事件消费与磁盘故障契约；使用真实临时目录和独立黄金字节。
//! Event consumption and disk-failure contracts using real temporary directories and independent golden bytes.

use super::*;
use ling_types::{ErrorCode, EventId};
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
        Self(std::env::temp_dir().join(format!("ling-events-{:032x}", rand::random::<u128>())))
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
fn code<T: std::fmt::Debug>(result: Result<T, ling_types::DbError>, code: ErrorCode) {
    assert_eq!(result.unwrap_err().code, code.as_str());
}

/// 在空检查之前登记的多个等待者不会漏掉提交，交付与无变化维护不引起广播循环。
/// Waiters registered before an empty check cannot miss a commit; delivery and unchanged maintenance do not broadcast loops.
#[tokio::test]
async fn notifications_cover_commit_ack_and_disconnect_without_empty_wakeups() {
    let store = EventStore::open(Config::default(), 1, 1).unwrap();
    let sub = store
        .subscribe(0, vec!["*".into()], Some("notify"))
        .unwrap();
    let notifications = store.notifications();
    let first = notifications.notified();
    let second = notifications.notified();
    let idle = store.poll(&sub, Instant::now()).unwrap();
    assert!(idle.event.is_none() && idle.retry_at.is_none() && !idle.more);
    store.commit(&[change("a", 0)], || ()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap();

    let no_change = notifications.notified();
    let event = store.poll(&sub, Instant::now()).unwrap().event.unwrap();
    store.maintain(1000).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), no_change)
            .await
            .is_err()
    );

    let acknowledged = notifications.notified();
    store.ack(&sub, event.id).unwrap();
    tokio::time::timeout(Duration::from_secs(1), acknowledged)
        .await
        .unwrap();
    assert!(store.poll(&sub, Instant::now()).unwrap().retry_at.is_none());

    store.commit(&[change("b", 0)], || ()).unwrap();
    store.poll(&sub, Instant::now()).unwrap().event.unwrap();
    let disconnected = notifications.notified();
    store.disconnect(&sub);
    tokio::time::timeout(Duration::from_secs(1), disconnected)
        .await
        .unwrap();
}

/// 租约以原发送时刻计算，断开只改变所有者，不把重投间隔缩短为立即重投。
/// Lease deadlines use the original send time; disconnect changes ownership without bypassing the retry interval.
#[test]
fn notification_poll_uses_exact_lease_deadlines() {
    let config = Config {
        consumer_timeout_ms: 300,
        retry_interval_ms: 100,
        ..Config::default()
    };
    let store = EventStore::open(config, 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("lease")).unwrap();
    let other = store.subscribe(0, vec!["*".into()], Some("lease")).unwrap();
    let sent = Instant::now();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let first = store.poll(&sub, sent).unwrap();
    let id = first.event.unwrap().id;
    assert_eq!(first.retry_at, Some(sent + Duration::from_millis(300)));
    let waiting = store
        .poll(&other, sent + Duration::from_millis(50))
        .unwrap();
    assert!(waiting.event.is_none() && !waiting.more);
    assert_eq!(waiting.retry_at, Some(sent + Duration::from_millis(300)));
    store.disconnect(&sub);
    let waiting = store
        .poll(&other, sent + Duration::from_millis(99))
        .unwrap();
    assert!(waiting.event.is_none());
    assert_eq!(waiting.retry_at, Some(sent + Duration::from_millis(100)));
    let retry = store
        .poll(&other, sent + Duration::from_millis(100))
        .unwrap();
    assert_eq!(retry.event.unwrap().id, id);
    assert_eq!(retry.retry_at, Some(sent + Duration::from_millis(400)));
}

/// 有界过滤一轮未追平时继续扫描；共享预算拒绝时等待 ACK，不反复空转。
/// A bounded filter scan continues when it has progressed; shared-budget rejection waits for an ACK without spinning.
#[test]
fn notification_poll_distinguishes_filter_progress_from_budget_rejection() {
    let config = Config {
        buffer_bytes: 4096,
        max_pending: 8,
        max_groups: 4,
        ..Config::default()
    };
    let store = EventStore::open(config.clone(), 1, 1).unwrap();
    let filtered = store.subscribe(0, vec!["match*".into()], None).unwrap();
    for key in ["skip1", "skip2", "skip3", "skip4", "skip5", "match"] {
        store.commit(&[change(key, 0)], || ()).unwrap();
    }
    let first = store.poll(&filtered, Instant::now()).unwrap();
    assert!(first.event.is_none() && first.retry_at.is_none() && first.more);
    let second = store.poll(&filtered, Instant::now()).unwrap();
    let event = second.event.unwrap();
    assert_eq!(event.key, "match");
    store.ack(&filtered, event.id).unwrap();
    let caught_up = store.poll(&filtered, Instant::now()).unwrap();
    assert!(caught_up.event.is_none() && caught_up.retry_at.is_none() && !caught_up.more);

    let store = EventStore::open(config, 1, 1).unwrap();
    let one = store.subscribe(0, vec!["*".into()], Some("one")).unwrap();
    let two = store.subscribe(0, vec!["*".into()], Some("two")).unwrap();
    store.commit(&[change("budget", 0)], || ()).unwrap();
    let event = store.poll(&one, Instant::now()).unwrap().event.unwrap();
    let full = store.poll(&two, Instant::now()).unwrap();
    assert!(full.event.is_none() && full.retry_at.is_none() && !full.more);
    store.ack(&one, event.id).unwrap();
    assert_eq!(
        store.poll(&two, Instant::now()).unwrap().event.unwrap().id,
        event.id
    );
}

/// 保留删除会唤醒沉睡组，使其明确收到过期错误。
/// Retention removal wakes a sleeping group so expired offsets are reported explicitly.
#[tokio::test]
async fn retention_notifies_offset_expiry() {
    let store = EventStore::open(
        Config {
            retention_ms: 100,
            ..Config::default()
        },
        1,
        1,
    )
    .unwrap();
    let sub = store
        .subscribe(0, vec!["*".into()], Some("retained"))
        .unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let notifications = store.notifications();
    let changed = notifications.notified();
    store.maintain(1100).unwrap();
    tokio::time::timeout(Duration::from_secs(1), changed)
        .await
        .unwrap();
    code(store.poll(&sub, Instant::now()), ErrorCode::OffsetExpired);
}

/// fsync 失败唤醒已登记的空闲订阅，并保留全局故障状态及恢复后的立即重投。
/// An fsync failure wakes a registered idle subscription and preserves global failure and immediate recovered redelivery.
#[cfg(unix)]
#[tokio::test]
async fn journal_failure_wakes_waiters_and_recovered_pending_is_immediate() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("fault")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let id = store.poll(&sub, Instant::now()).unwrap().event.unwrap().id;
    let notifications = store.notifications();
    let failed = notifications.notified();
    store.fail_next_sync();
    code(
        store.commit(&[change("b", 0)], || ()),
        ErrorCode::CommitUnknown,
    );
    tokio::time::timeout(Duration::from_secs(1), failed)
        .await
        .unwrap();
    code(
        store.poll(&sub, Instant::now()),
        ErrorCode::JournalUnavailable,
    );
    drop(store);

    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("fault")).unwrap();
    let now = Instant::now();
    let recovered = store.poll(&sub, now).unwrap();
    assert_eq!(recovered.event.unwrap().id, id);
    assert_eq!(
        recovered.retry_at,
        Some(now + Duration::from_millis(Config::default().consumer_timeout_ms))
    );
}

/// 消费、重投与重复 ACK 的读故障唤醒无待确认事件的组，并停止后续持久操作。
/// Read faults during consumption, redelivery, and duplicate ACK wake groups without pending work and stop later durable operations.
#[cfg(unix)]
#[tokio::test]
async fn journal_read_fault_wakes_idle_groups_and_stops_durable_operations() {
    for access in 0..3 {
        let dir = Directory::new();
        let store = EventStore::open(dir.config(), 1, 1).unwrap();
        let active = store
            .subscribe(0, vec!["*".into()], Some("active"))
            .unwrap();
        let idle = store.subscribe(0, vec!["*".into()], Some("idle")).unwrap();
        store.commit(&[change("a", 0)], || ()).unwrap();
        let now = Instant::now();
        let id = store.poll(&active, now).unwrap().event.unwrap().id;
        assert_eq!(store.poll(&idle, now).unwrap().event.unwrap().id, id);
        store.ack(&idle, id).unwrap();
        if access != 1 {
            store.ack(&active, id).unwrap();
            assert!(store.poll(&active, now).unwrap().event.is_none());
        }
        let ready = store.poll(&idle, now).unwrap();
        assert!(ready.event.is_none() && ready.retry_at.is_none() && !ready.more);
        let notifications = store.notifications();
        let failed = notifications.notified();
        let path = dir.0.join("journal.lgj");
        let mut damaged = fs::read(&path).unwrap();
        let offset = if access == 0 {
            let offset = damaged.len();
            damaged.extend([0, 0]);
            offset
        } else {
            let mut offset = 40;
            while damaged[offset + 8] != codec::EVENTS {
                let len =
                    u32::from_be_bytes(damaged[offset..offset + 4].try_into().unwrap()) as usize;
                offset += len + 16;
            }
            damaged[offset + 8] ^= 1;
            offset
        };
        fs::write(&path, &damaged).unwrap();
        let error = if access == 2 {
            store.ack(&active, id).unwrap_err()
        } else {
            store
                .poll(&active, now + Duration::from_secs(60))
                .unwrap_err()
        };
        assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
        assert!(error.message.contains(&format!("byte offset {offset}")));
        tokio::time::timeout(Duration::from_secs(1), failed)
            .await
            .unwrap();
        let no_repeat = notifications.notified();
        code(store.poll(&idle, now), ErrorCode::JournalUnavailable);
        code(store.ack(&active, id), ErrorCode::JournalUnavailable);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), no_repeat)
                .await
                .is_err()
        );
        code(
            store.commit(&[change("b", 0)], || panic!("faulted commit published")),
            ErrorCode::JournalUnavailable,
        );
        code(store.maintain(1000), ErrorCode::JournalUnavailable);
        assert_eq!(fs::read(path).unwrap(), damaged);
    }
}

/// Windows 保持 Fast 可用，Reliable 在创建任何目录前明确拒绝，不静默降级。
/// Windows keeps Fast available and rejects Reliable before creating any directory, without silent fallback.
#[cfg(windows)]
#[test]
fn windows_reliable_rejected_without_creating_directory() {
    let fast = EventStore::open(Config::default(), 1, 1).unwrap();
    assert!(!fast.reliable());
    fast.commit(&[change("fast", 0)], || ()).unwrap();
    assert_eq!(fast.stats().unwrap().committed, 1);

    let directory = Directory::new();
    let error = EventStore::open(directory.config(), 1, 1).err().unwrap();
    assert_eq!(error.code, ErrorCode::ConfigInvalid.as_str());
    assert!(error.message.contains("not supported on Windows"));
    assert!(!directory.0.exists());
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
    let e = ling_types::Event {
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
#[cfg(unix)]
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
#[cfg(unix)]
#[test]
fn sync_failure_is_unknown_and_globally_faults_writes_and_ack() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    let e = store.next(&sub, Instant::now()).unwrap().unwrap();
    store.set_metrics_enabled(true).unwrap();
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
    let stats = store.stats().unwrap();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.journal_errors, 1);
    assert_eq!(stats.journal_fsync_duration_us.count, 1);
    assert_eq!(stats.journal_transaction_bytes.count, 1);
    drop(store);
    // 完整但同步结果未知的记录可能幸存；A complete record with an unknown sync outcome may survive.
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
    assert_eq!(store.next(&sub, Instant::now()).unwrap().unwrap().id, e.id);
    store.ack(&sub, e.id).unwrap();
    assert_eq!(store.next(&sub, Instant::now()).unwrap().unwrap().key, "b");
}

/// 仅确认的短尾可修复；可疑长度、完整损坏、目录锁与拓扑冲突明确失败。
/// Only identifiable short tails are repaired; ambiguous lengths, corruption, locks, and topology conflicts fail.
#[cfg(unix)]
#[test]
fn incomplete_tail_recovers_but_complete_corruption_and_wrong_topology_fail() {
    let dir = Directory::new();
    let store = EventStore::open(dir.config(), 1, 1).unwrap();
    store.commit(&[change("a", 0)], || ()).unwrap();
    assert!(EventStore::open(dir.config(), 1, 1).is_err());
    drop(store);
    let path = dir.0.join("journal.lgj");
    let valid = fs::read(&path).unwrap();
    let payload = b"uncommitted";
    let mut short_commit = (payload.len() as u32).to_be_bytes().to_vec();
    short_commit.extend(crc32fast::hash(payload).to_be_bytes());
    short_commit.extend(payload);
    short_commit.extend(b"LGCMT");
    for tail in [vec![0, 0], short_commit] {
        let mut bytes = valid.clone();
        bytes.extend(tail);
        fs::write(&path, bytes).unwrap();
        drop(EventStore::open(dir.config(), 1, 1).unwrap());
        assert_eq!(fs::read(&path).unwrap(), valid);
    }
    // 长度头可能已损坏，不能仅凭载荷不足删除原件。
    // A corrupted length header can also overrun the file, so insufficient payload alone cannot justify truncation.
    let mut ambiguous = valid.clone();
    ambiguous.extend([0, 0, 0, 20, 0, 0, 0, 0, 1, 2]);
    fs::write(&path, &ambiguous).unwrap();
    let error = EventStore::open(dir.config(), 1, 1).err().unwrap();
    assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
    assert!(
        error
            .message
            .contains(&format!("byte offset {}", valid.len()))
    );
    assert_eq!(fs::read(&path).unwrap(), ambiguous);
    fs::write(&path, &valid).unwrap();
    code(
        EventStore::open(dir.config(), 2, 1).map(|_| ()),
        ErrorCode::ConfigInvalid,
    );
    let mut corrupt = valid;
    corrupt[48] ^= 1;
    fs::write(&path, &corrupt).unwrap();
    let error = EventStore::open(dir.config(), 1, 1).err().unwrap();
    assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
    assert!(error.message.contains("byte offset 40"));
    assert_eq!(fs::read(&path).unwrap(), corrupt);
}

/// 完整且校验正确的空组名仍不符合恢复契约。
/// A complete checksummed record with an empty group name still violates recovery contracts.
#[cfg(unix)]
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
    let path = dir.0.join("journal.lgj");
    let mut invalid = fs::read(&path).unwrap();
    invalid.extend([0, 0]);
    fs::write(&path, &invalid).unwrap();
    code(
        EventStore::open(dir.config(), 1, 1).map(|_| ()),
        ErrorCode::JournalUnavailable,
    );
    // 全局语义校验也须先完成，不能先修尾再发现组状态非法。
    // Global semantic validation must also finish before tail repair can discard evidence.
    assert_eq!(fs::read(path).unwrap(), invalid);
}

/// 消费续读短尾与重投按 ID 查找坏记录均只报错，保持原始磁盘证据。
/// Consumption at a short tail and redelivery lookup of a corrupt record report faults without altering disk evidence.
#[cfg(unix)]
#[test]
fn runtime_consumption_and_redelivery_preserve_corrupt_journal() {
    for redeliver in [false, true] {
        let dir = Directory::new();
        let store = EventStore::open(dir.config(), 1, 1).unwrap();
        let sub = store.subscribe(0, vec!["*".into()], Some("g")).unwrap();
        store.commit(&[change("key", 0)], || ()).unwrap();
        let now = Instant::now();
        let event = store.next(&sub, now).unwrap().unwrap();
        if !redeliver {
            store.ack(&sub, event.id).unwrap();
            assert!(store.next(&sub, now).unwrap().is_none());
        }
        let path = dir.0.join("journal.lgj");
        let mut damaged = fs::read(&path).unwrap();
        let offset = if redeliver {
            let mut at = 40;
            while damaged[at + 8] != codec::EVENTS {
                let n = u32::from_be_bytes(damaged[at..at + 4].try_into().unwrap()) as usize;
                at += n + 16;
            }
            damaged[at + 8] ^= 1;
            at
        } else {
            let at = damaged.len();
            damaged.extend([0, 0]);
            at
        };
        fs::write(&path, &damaged).unwrap();
        let error = store.next(&sub, now + Duration::from_secs(60)).unwrap_err();
        assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
        assert!(error.message.contains(&format!("byte offset {offset}")));
        assert_eq!(fs::read(path).unwrap(), damaged);
    }
}

/// 容量背压不缩短保留，检查点继续序号并标记过期组。
/// Capacity backpressure preserves retention; checkpoints continue sequences and expire lagging groups.
#[cfg(unix)]
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
#[cfg(unix)]
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
    // 中断在第二个事件前证据不足，拒绝启动且保留原件，不恢复第一项。
    // An interruption before the second event is ambiguous: reject startup, preserve evidence, and expose no partial transaction.
    let partial = &bytes[..transaction + 8 + 5 + 64];
    fs::write(&path, partial).unwrap();
    let error = EventStore::open(dir.config(), 1, 2).err().unwrap();
    assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
    assert!(
        error
            .message
            .contains(&format!("byte offset {transaction}"))
    );
    assert_eq!(fs::read(&path).unwrap(), partial);
    // 完整载荷与正确 CRC 加提交尾标前缀能识别未提交事务，全部丢弃而非恢复一半。
    // A complete payload, correct CRC, and commit-trailer prefix identify an uncommitted transaction to discard atomically.
    let len = u32::from_be_bytes(bytes[transaction..transaction + 4].try_into().unwrap()) as usize;
    fs::write(&path, &bytes[..transaction + 8 + len + 3]).unwrap();
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    consumer_visibility(Config::default());
}

/// Reliable 的同步事务也不能提前发布；Reliable synchronized transactions cannot publish early either.
#[cfg(unix)]
#[test]
fn reliable_consumer_visibility_waits_for_complete_publication() {
    let dir = Directory::new();
    consumer_visibility(dir.config());
}

/// 在提交回调处阻塞写入，同时观察独立消费线程。
/// Blocks the publication callback while observing an independent consumer thread.
fn consumer_visibility(config: Config) {
    use std::sync::{Arc, mpsc};
    let store = Arc::new(EventStore::open(config, 1, 2).unwrap());
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

/// 压缩全流程计数与时延包含失败，文件和目录同步分开计数，重写不冒充追加事务。
/// Full compaction counts and latency include failures; file and directory syncs count separately and rewrites are not append transactions.
#[cfg(unix)]
#[test]
fn journal_metrics_preserve_compaction_and_recovery_boundaries() {
    let dir = Directory::new();
    let mut config = dir.config();
    config.retention_ms = 100;
    let store = EventStore::open(config.clone(), 1, 1).unwrap();
    store.commit(&[change("first", 0)], || ()).unwrap();
    let initial = store.stats().unwrap();
    assert_eq!(initial.journal_fsync_duration_us.count, 0);
    assert_eq!(initial.journal_transaction_bytes.count, 0);
    store.set_metrics_enabled(true).unwrap();
    store.commit(&[change("second", 0)], || ()).unwrap();
    let appended = store.stats().unwrap();
    assert_eq!(appended.journal_fsync_duration_us.count, 1);
    assert_eq!(appended.journal_transaction_bytes.count, 1);
    store.maintain(1099).unwrap();
    assert_eq!(store.stats().unwrap().journal_compactions, 0);
    store.maintain(1100).unwrap();
    let compacted = store.stats().unwrap();
    assert_eq!(compacted.journal_compactions, 1);
    assert_eq!(compacted.journal_compaction_successes, 1);
    assert_eq!(compacted.journal_compaction_failures, 0);
    assert_eq!(compacted.journal_compaction_duration_us.count, 1);
    assert_eq!(compacted.journal_fsync_duration_us.count, 3);
    assert_eq!(
        compacted.journal_transaction_bytes,
        appended.journal_transaction_bytes
    );
    store.set_metrics_enabled(false).unwrap();
    store.commit(&[change("third", 0)], || ()).unwrap();
    store.maintain(1100).unwrap();
    let disabled = store.stats().unwrap();
    assert_eq!(disabled.journal_compactions, 2);
    assert_eq!(disabled.journal_compaction_successes, 2);
    assert_eq!(
        disabled.journal_compaction_duration_us,
        compacted.journal_compaction_duration_us
    );
    assert_eq!(
        disabled.journal_fsync_duration_us,
        compacted.journal_fsync_duration_us
    );
    assert_eq!(
        disabled.journal_transaction_bytes,
        appended.journal_transaction_bytes
    );
    drop(store);
    let store = EventStore::open(config, 1, 1).unwrap();
    assert_eq!(store.stats().unwrap().journal_compactions, 0);
    assert_eq!(store.stats().unwrap().journal_fsync_duration_us.count, 0);
    store.set_metrics_enabled(true).unwrap();
    store.commit(&[change("fourth", 0)], || ()).unwrap();
    let before = fs::read(dir.0.join("journal.lgj")).unwrap();
    fs::create_dir(dir.0.join("journal.next")).unwrap();
    code(store.maintain(1100), ErrorCode::JournalUnavailable);
    let failed = store.stats().unwrap();
    assert_eq!(failed.journal_compactions, 1);
    assert_eq!(failed.journal_compaction_successes, 0);
    assert_eq!(failed.journal_compaction_failures, 1);
    assert_eq!(failed.journal_compaction_duration_us.count, 1);
    assert_eq!(failed.journal_fsync_duration_us.count, 1);
    assert_eq!(failed.oldest_retained_ms, Some(1000));
    assert_eq!(fs::read(dir.0.join("journal.lgj")).unwrap(), before);
}
