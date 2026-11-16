//! 显式大端磁盘编码；所有长度在读取或分配前受限。
//! Explicit big-endian disk encoding; lengths are bounded before reads and allocation.

use crate::{
    err,
    store::{Group, Pending, Progress},
};
use ling_types::{DbError, ErrorCode, Event, EventId, EventKind, Generation};
use std::collections::BTreeMap;

/// 一个磁盘记录的硬上限；Hard limit for one disk record.
pub(crate) const MAX_RECORD: usize = 128 * 1024 * 1024;
/// 事件事务标签；Event transaction tag.
pub(crate) const EVENTS: u8 = 1;
/// 组状态标签；Group state tag.
pub(crate) const GROUP: u8 = 2;
/// 检查点标签；Checkpoint tag.
pub(crate) const CHECKPOINT: u8 = 3;
/// 逻辑流尾部与保留下界；Logical stream tail and retained floor.
pub(crate) type Positions = BTreeMap<(u16, u32), u64>;

/// 只对已知字段提供构造器；Encoder exposing only explicit disk fields.
#[derive(Default)]
pub(crate) struct Encoder(pub Vec<u8>);
impl Encoder {
    /// 写固定无符号整数；Writes a fixed-width unsigned integer.
    pub fn n(&mut self, n: u128, width: usize) {
        self.0.extend_from_slice(&n.to_be_bytes()[16 - width..]);
    }
    /// 长度前缀 UTF-8；Length-prefixed UTF-8.
    pub fn s(&mut self, s: &str) {
        self.n(s.len() as u128, 4);
        self.0.extend_from_slice(s.as_bytes());
    }
    /// 编码流位置；Encodes stream positions.
    pub fn positions(&mut self, p: &Positions) {
        self.n(p.len() as u128, 4);
        for (&(db, shard), &seq) in p {
            self.n(db.into(), 2);
            self.n(shard.into(), 4);
            self.n(seq.into(), 8);
        }
    }
}
/// 不信任磁盘长度或 count；Never trusts disk lengths or counts.
pub(crate) struct Decoder<'a> {
    pub rest: &'a [u8],
}
impl<'a> Decoder<'a> {
    /// 先验证剩余长度；Checks remaining bytes first.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], DbError> {
        if n > self.rest.len() {
            return Err(corrupt());
        }
        let (a, b) = self.rest.split_at(n);
        self.rest = b;
        Ok(a)
    }
    /// 读取固定大端整数；Reads a fixed big-endian integer.
    pub fn n(&mut self, width: usize) -> Result<u128, DbError> {
        let mut a = [0; 16];
        a[16 - width..].copy_from_slice(self.bytes(width)?);
        Ok(u128::from_be_bytes(a))
    }
    /// count 不能超过剩余最小记录数；Counts cannot exceed the remaining minimum record count.
    pub fn count(&mut self, min: usize) -> Result<usize, DbError> {
        let n = self.n(4)? as usize;
        if n > self.rest.len() / min {
            return Err(corrupt());
        }
        Ok(n)
    }
    /// UTF-8 字符串；UTF-8 string.
    pub fn s(&mut self) -> Result<String, DbError> {
        let n = self.n(4)? as usize;
        let b = self.bytes(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| corrupt())
    }
    /// 恢复流位置并拒绝重复；Restores positions and rejects duplicates.
    pub fn positions(&mut self) -> Result<Positions, DbError> {
        let n = self.count(14)?;
        let mut p = Positions::new();
        for _ in 0..n {
            let id = (self.n(2)? as u16, self.n(4)? as u32);
            let seq = self.n(8)? as u64;
            if p.insert(id, seq).is_some() {
                return Err(corrupt());
            }
        }
        Ok(p)
    }
    /// 不允许未解释的尾部；Rejects unexplained trailing bytes.
    pub fn end(self) -> Result<(), DbError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(corrupt())
        }
    }
}
/// 单个原子事件事务；One atomic event transaction.
pub(crate) fn events(events: &[Event]) -> Vec<u8> {
    let mut e = Encoder(vec![EVENTS]);
    e.n(events.len() as u128, 4);
    for v in events {
        e.n(v.id.db.into(), 2);
        e.n(v.id.shard.into(), 4);
        e.n(v.id.sequence.into(), 8);
        e.n(v.kind as u128, 1);
        e.s(&v.key);
        e.n(v.generation.epoch, 16);
        e.n(v.generation.counter.into(), 8);
        e.n(v.revision.into(), 8);
        e.n(v.timestamp_ms.into(), 8);
    }
    e.0
}
/// 从完整记录恢复，不为任意 count 预分配；Restores complete records without allocating for arbitrary counts.
pub(crate) fn read_events(data: &[u8], stream: u128) -> Result<Vec<Event>, DbError> {
    let mut d = Decoder { rest: data };
    if d.n(1)? != EVENTS.into() {
        return Err(corrupt());
    }
    let n = d.count(60)?;
    if n == 0 {
        return Err(corrupt());
    }
    let mut out = Vec::new();
    for _ in 0..n {
        let id = EventId {
            stream_id: stream,
            db: d.n(2)? as u16,
            shard: d.n(4)? as u32,
            sequence: d.n(8)? as u64,
        };
        let kind = EventKind::from_tag(d.n(1)? as u8).map_err(|_| corrupt())?;
        let key = d.s()?;
        let generation = Generation {
            epoch: d.n(16)?,
            counter: d.n(8)? as u64,
        };
        let revision = d.n(8)? as u64;
        let timestamp_ms = d.n(8)? as u64;
        if id.sequence == 0 || key.is_empty() || generation.counter == 0 || revision == 0 {
            return Err(corrupt());
        }
        out.push(Event {
            id,
            kind,
            key,
            generation,
            revision,
            timestamp_ms,
        });
    }
    d.end()?;
    Ok(out)
}
/// 完整组快照；租约所属连接和单调时间不落盘。
// Complete group snapshot; connection leases and monotonic times are never persisted.
pub(crate) fn group(name: &str, g: &Group) -> Vec<u8> {
    let mut e = Encoder(vec![GROUP]);
    e.s(name);
    e.n(g.db.into(), 2);
    e.n(g.expired as u128, 1);
    e.n(g.patterns.len() as u128, 4);
    for p in &g.patterns {
        e.s(p)
    }
    e.n(g.progress.len() as u128, 4);
    for (&shard, p) in &g.progress {
        e.n(shard.into(), 4);
        e.n(p.origin.into(), 8);
        e.n(p.watermark.into(), 8);
        e.n(p.cursor.into(), 8);
    }
    e.n(g.pending.len() as u128, 4);
    for (&(shard, seq), p) in &g.pending {
        e.n(shard.into(), 4);
        e.n(seq.into(), 8);
        e.n(p.acked as u128, 1);
    }
    e.0
}
/// 校验水位、空洞及重复字段；Validates watermarks, holes, and duplicate fields.
pub(crate) fn read_group(data: &[u8]) -> Result<(String, Group), DbError> {
    let mut d = Decoder { rest: data };
    if d.n(1)? != GROUP.into() {
        return Err(corrupt());
    }
    let name = d.s()?;
    let db = d.n(2)? as u16;
    let expired = boolean(&mut d)?;
    let n = d.count(4)?;
    let mut patterns = Vec::new();
    for _ in 0..n {
        patterns.push(d.s()?)
    }
    if patterns.is_empty() {
        return Err(corrupt());
    }
    let n = d.count(28)?;
    let mut progress = BTreeMap::new();
    for _ in 0..n {
        let shard = d.n(4)? as u32;
        let p = Progress {
            origin: d.n(8)? as u64,
            watermark: d.n(8)? as u64,
            cursor: d.n(8)? as u64,
        };
        if p.origin > p.watermark || p.watermark > p.cursor || progress.insert(shard, p).is_some() {
            return Err(corrupt());
        }
    }
    let n = d.count(13)?;
    let mut pending = BTreeMap::new();
    for _ in 0..n {
        let shard = d.n(4)? as u32;
        let seq = d.n(8)? as u64;
        let acked = boolean(&mut d)?;
        let p = progress.get(&shard).ok_or_else(corrupt)?;
        if seq <= p.watermark
            || seq > p.cursor
            || pending
                .insert(
                    (shard, seq),
                    Pending {
                        acked,
                        owner: 0,
                        sent: None,
                    },
                )
                .is_some()
        {
            return Err(corrupt());
        }
    }
    d.end()?;
    Ok((
        name,
        Group {
            db,
            patterns,
            progress,
            pending,
            expired,
            named: true,
            scan_offset: 40,
        },
    ))
}
/// bool 仅允许 0/1；Boolean tags are exactly zero or one.
fn boolean(d: &mut Decoder<'_>) -> Result<bool, DbError> {
    match d.n(1)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(corrupt()),
    }
}
/// 可辨识的损坏错误，不隐藏成空日志；Identifiable corruption, never disguised as an empty journal.
pub(crate) fn corrupt() -> DbError {
    err(
        ErrorCode::JournalUnavailable,
        "Corrupt or incompatible event Journal",
    )
}
