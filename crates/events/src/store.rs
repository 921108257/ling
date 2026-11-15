//! 事件提交锁连接磁盘、发布回调和消费水位，确保消费者只看到完整提交。
//! One commit lock joins disk, publication callbacks, and consumer watermarks for atomic visibility.

use crate::{
    Change, Config,
    codec::{self, Positions},
    err,
    journal::Journal,
};
use linger_types::{DbError, ErrorCode, Event, EventId};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

/// 单流消费位置；origin 用于拒绝组创建之前的伪造 ACK。
/// Per-stream progress; origin rejects forged ACKs from before group creation.
#[derive(Clone)]
pub(crate) struct Progress {
    /// 首次尾部；Initial tail.
    pub origin: u64,
    /// 连续确认位置；Contiguous acknowledged position.
    pub watermark: u64,
    /// 已扫描位置；Scanned position.
    pub cursor: u64,
}
/// 水位后的匹配交付，乱序 ACK 仍占窗口。
// A matching delivery beyond the watermark; out-of-order ACKs still occupy the window.
#[derive(Clone)]
pub(crate) struct Pending {
    /// 是否确认；Whether acknowledged.
    pub acked: bool,
    /// 运行时租约持有者；Runtime lease owner.
    pub owner: u64,
    /// 最近交付的单调时间；Last delivery's monotonic time.
    pub sent: Option<Instant>,
}
/// 规范化组定义和有界交付窗口；Canonical group definition and bounded delivery window.
#[derive(Clone)]
pub(crate) struct Group {
    /// 绑定数据库；Bound database.
    pub db: u16,
    /// 已排序去重的筛选；Sorted, deduplicated filters.
    pub patterns: Vec<String>,
    /// 各分片消费位置；Per-shard positions.
    pub progress: BTreeMap<u32, Progress>,
    /// 交付窗口；Delivery window.
    pub pending: BTreeMap<(u32, u64), Pending>,
    /// 已丢失需要的数据；Required history was lost.
    pub expired: bool,
    /// 命名组才持久化；Only named groups persist.
    pub named: bool,
}
impl Group {
    /// 窗口首个未完成位置前移，已确认后继释放；Advance past completed positions and release acknowledged successors.
    fn advance(&mut self, shard: u32) {
        let p = self.progress.get_mut(&shard).unwrap();
        let first = self
            .pending
            .range((shard, 0)..=(shard, u64::MAX))
            .find(|(_, v)| !v.acked)
            .map(|((_, seq), _)| *seq);
        p.watermark = first.map_or(p.cursor, |seq| seq - 1);
        let watermark = p.watermark;
        self.pending
            .retain(|&(s, seq), _| s != shard || seq > watermark);
    }
    /// 保守计费包含克隆候选所需空间；Conservative charge includes space for a cloned candidate.
    fn charge(&self, name: &str) -> usize {
        512 + name.len() * 2
            + self
                .patterns
                .iter()
                .map(|p| 128 + p.len() * 2)
                .sum::<usize>()
            + self.progress.len() * 192
            + self.pending.len() * 256
    }
}

/// 连接拥有的消费令牌，不能克隆为另一连接。
/// Connection-owned consumer token, never cloned into another connection.
pub struct Subscription {
    /// 组内部标识；Internal group identifier.
    group: String,
    /// 唯一消费者编号；Unique consumer number.
    consumer: u64,
}
/// 当前事件运行指标，不使用 Key 作为标签。
/// Current event metrics without key-valued labels.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    /// 本进程成功提交事件；Events committed in this process.
    pub committed: u64,
    /// Fast 缓冲丢弃数；Fast buffer drops.
    pub dropped: u64,
    /// 再次交付次数；Redelivery count.
    pub redelivered: u64,
    /// 当前日志字节；Current journal bytes.
    pub journal_bytes: usize,
    /// 当前命名组与临时会话数；Current named groups and temporary sessions.
    pub groups: usize,
    /// 当前交付窗口项数；Current delivery-window entries.
    pub pending: usize,
}
/// 互斥锁内的提交与消费状态；Commit and consumer state under the mutex.
struct State {
    /// 持久或进程内流身份；Persistent or process-local stream identity.
    stream: u128,
    /// 各流尾部；Per-stream tails.
    tails: Positions,
    /// 已删除的最大序号；Largest removed sequences.
    floors: Positions,
    /// 有界 Fast 事件队列；Bounded Fast event queue.
    fast: VecDeque<Event>,
    /// Fast 逻辑内存；Fast logical memory.
    bytes: usize,
    /// 命名与临时组；Named and temporary groups.
    groups: BTreeMap<String, Group>,
    /// 全局消费者编号；Global consumer number.
    consumer: u64,
    /// Reliable 日志；Reliable journal.
    journal: Option<Journal>,
    /// 运行计数；Runtime counters.
    stats: Stats,
}
/// 共享事件存储；引擎在其回调中发布缓存状态。
/// Shared event store; the engine publishes cache state inside its callback.
pub struct EventStore {
    /// 不可变配置；Immutable settings.
    config: Config,
    /// 拓扑范围；Topology bounds.
    dbs: u32,
    /// 每库分片数；Shards per database.
    shards: u32,
    /// 唯一提交所有权；Exclusive commit ownership.
    state: Mutex<State>,
}
impl EventStore {
    /// 测试提交未知路径；Exercises unknown commit outcomes in tests.
    #[cfg(test)]
    pub(crate) fn fail_next_sync(&self) {
        self.state
            .lock()
            .unwrap()
            .journal
            .as_mut()
            .unwrap()
            .fail_sync = true;
    }

    /// 恢复事件及消费元数据，缓存由引擎独立从空建立。
    /// Recovers events and consumer metadata; the engine independently starts with an empty cache.
    pub fn open(config: Config, dbs: u32, shards: u32) -> Result<Self, DbError> {
        config.validate()?;
        if dbs == 0
            || dbs > 65536
            || shards == 0
            || u64::from(dbs) * u64::from(shards) * 64 > config.buffer_bytes as u64 / 4
        {
            return Err(err(ErrorCode::ConfigInvalid, "Invalid event topology"));
        }
        let (mut journal, stream) = if config.reliable {
            let (j, s) = Journal::open(&config, dbs, shards)?;
            (Some(j), s)
        } else {
            (None, rand::random())
        };
        let mut state = State {
            stream,
            tails: Positions::new(),
            floors: Positions::new(),
            fast: VecDeque::new(),
            bytes: 0,
            groups: BTreeMap::new(),
            consumer: 0,
            journal: None,
            stats: Stats::default(),
        };
        let mut replay = Positions::new();
        let mut baseline = Positions::new();
        let mut records = 0;
        if let Some(j) = journal.as_mut() {
            j.visit(|record| {
                match record.first() {
                    Some(&codec::EVENTS) => {
                        for e in codec::read_events(record, stream)? {
                            if u32::from(e.id.db) >= dbs || e.id.shard >= shards {
                                return Err(codec::corrupt());
                            }
                            let key = (e.id.db, e.id.shard);
                            let previous = replay.get(&key).copied().unwrap_or(0);
                            if e.id.sequence
                                != previous.checked_add(1).ok_or_else(codec::corrupt)?
                            {
                                return Err(codec::corrupt());
                            }
                            replay.insert(key, e.id.sequence);
                            state
                                .tails
                                .entry(key)
                                .and_modify(|n| *n = (*n).max(e.id.sequence))
                                .or_insert(e.id.sequence);
                        }
                    }
                    Some(&codec::GROUP) => {
                        let (name, g) = codec::read_group(record)?;
                        if u32::from(g.db) >= dbs
                            || g.progress.len() != shards as usize
                            || g.pending.len() > config.max_pending
                        {
                            return Err(codec::corrupt());
                        }
                        state.groups.insert(name, g);
                    }
                    Some(&codec::CHECKPOINT) => {
                        let mut d = codec::Decoder { rest: &record[1..] };
                        if records != 0 {
                            return Err(codec::corrupt());
                        }
                        state.tails = d.positions()?;
                        state.floors = d.positions()?;
                        d.end()?;
                        for (&key, &seq) in &state.tails {
                            if u32::from(key.0) >= dbs || key.1 >= shards || seq == 0 {
                                return Err(codec::corrupt());
                            }
                        }
                        for (&key, &seq) in &state.floors {
                            if seq > state.tails.get(&key).copied().unwrap_or(0) {
                                return Err(codec::corrupt());
                            }
                        }
                        baseline = state.tails.clone();
                        replay = state.floors.clone();
                    }
                    _ => return Err(codec::corrupt()),
                }
                records += 1;
                if group_bytes(&state.groups) > config.buffer_bytes / 2
                    || state.groups.len() > config.max_groups
                {
                    return Err(err(
                        ErrorCode::ConfigInvalid,
                        "Recovered groups exceed event memory budget",
                    ));
                }
                Ok(())
            })?;
        }
        if baseline
            .iter()
            .any(|(key, seq)| replay.get(key).copied().unwrap_or(0) < *seq)
        {
            return Err(codec::corrupt());
        }
        for (name, g) in &state.groups {
            if !name.starts_with("n:")
                || name.len() > 1026
                || g.patterns.windows(2).any(|p| p[0] >= p[1])
                || g.patterns.iter().any(|p| validate_pattern(p).is_err())
                || g.progress.iter().any(|(&shard, p)| {
                    shard >= shards
                        || p.cursor > state.tails.get(&(g.db, shard)).copied().unwrap_or(0)
                })
            {
                return Err(codec::corrupt());
            }
        }
        state.journal = journal;
        Ok(Self {
            config,
            dbs,
            shards,
            state: Mutex::new(state),
        })
    }
    /// 是否生成事件；Whether events are enabled.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }
    /// 固定可靠模式；Fixed reliable mode.
    pub fn reliable(&self) -> bool {
        self.config.reliable
    }
    /// HELLO 中使用的流身份；Stream identity exposed by HELLO.
    pub fn stream_id(&self) -> Result<u128, DbError> {
        Ok(self.lock()?.stream)
    }
    /// 判断拓扑匹配，避免错误流坐标；Checks topology compatibility to prevent invalid stream coordinates.
    pub fn topology(&self) -> (u32, u32) {
        (self.dbs, self.shards)
    }
    /// 提交完整事件集合，再在同一锁内发布状态；失败不调用发布回调。
    /// Commits a complete event set and publishes state under the same lock; errors never invoke the callback.
    pub fn commit<T>(&self, changes: &[Change], publish: impl FnOnce() -> T) -> Result<T, DbError> {
        if !self.enabled() || changes.is_empty() {
            return Ok(publish());
        }
        let mut s = self.lock()?;
        let mut tails = s.tails.clone();
        let mut events = Vec::new();
        let mut charge = 0usize;
        for c in changes {
            if u32::from(c.db) >= self.dbs
                || c.shard >= self.shards
                || c.key.is_empty()
                || c.generation.counter == 0
                || c.revision == 0
            {
                return Err(err(ErrorCode::InvalidArgument, "Invalid event coordinates"));
            }
            charge = charge
                .checked_add(256 + c.key.len() * 2)
                .ok_or_else(limit)?;
            if charge > self.config.buffer_bytes / 4 {
                return Err(limit());
            }
            let seq = tails.entry((c.db, c.shard)).or_default();
            *seq = seq
                .checked_add(1)
                .ok_or_else(|| err(ErrorCode::Overflow, "Event sequence exhausted"))?;
            events.push(Event {
                id: EventId {
                    stream_id: s.stream,
                    db: c.db,
                    shard: c.shard,
                    sequence: *seq,
                },
                kind: c.kind,
                key: c.key.clone(),
                generation: c.generation,
                revision: c.revision,
                timestamp_ms: c.timestamp_ms,
            });
        }
        if self.config.reliable {
            let payload = codec::events(&events);
            if !s.journal.as_ref().unwrap().fits(payload.len(), false) {
                compact(
                    &mut s,
                    &self.config,
                    changes.iter().map(|c| c.timestamp_ms).max().unwrap_or(0),
                )?;
            }
            s.journal.as_mut().unwrap().append(&payload, false)?;
        }
        let result = publish();
        s.tails = tails;
        s.stats.committed = s.stats.committed.saturating_add(events.len() as u64);
        if !self.config.reliable {
            for event in events {
                s.bytes += event_charge(&event);
                s.fast.push_back(event);
            }
            while s.bytes + group_bytes(&s.groups) > self.config.buffer_bytes {
                drop_front(&mut s, true);
            }
        }
        Ok(result)
    }
    /// 规范化筛选后建立或恢复组；首次命名组必须先持久化尾部位置。
    /// Creates or resumes a normalized group; first named subscriptions persist their starting tails.
    pub fn subscribe(
        &self,
        db: u16,
        mut patterns: Vec<String>,
        name: Option<&str>,
    ) -> Result<Subscription, DbError> {
        if !self.enabled() {
            return Err(err(ErrorCode::EventsDisabled, "Events disabled"));
        }
        if u32::from(db) >= self.dbs || patterns.is_empty() {
            return Err(err(ErrorCode::InvalidArgument, "Invalid subscription"));
        }
        let mut pattern_bytes = 0usize;
        for p in &patterns {
            pattern_bytes = pattern_bytes.checked_add(p.len() + 128).ok_or_else(limit)?;
            validate_pattern(p)?;
        }
        if pattern_bytes > self.config.buffer_bytes / 4 {
            return Err(limit());
        }
        if name.is_some_and(|n| n.is_empty() || n.len() > 1024) {
            return Err(err(ErrorCode::InvalidArgument, "Invalid group name"));
        }
        patterns.sort();
        patterns.dedup();
        let mut s = self.lock()?;
        let consumer = s
            .consumer
            .checked_add(1)
            .ok_or_else(|| err(ErrorCode::Overflow, "Consumer sequence exhausted"))?;
        let key = name
            .map(|n| format!("n:{n}"))
            .unwrap_or_else(|| format!("t:{consumer}"));
        if let Some(g) = s.groups.get(&key) {
            if g.db != db || g.patterns != patterns {
                return Err(err(
                    ErrorCode::GroupMismatch,
                    "Group database or patterns differ",
                ));
            }
        } else {
            if s.groups.len() >= self.config.max_groups {
                return Err(limit());
            }
            let progress = (0..self.shards)
                .map(|shard| {
                    let seq = s.tails.get(&(db, shard)).copied().unwrap_or(0);
                    (
                        shard,
                        Progress {
                            origin: seq,
                            watermark: seq,
                            cursor: seq,
                        },
                    )
                })
                .collect();
            let g = Group {
                db,
                patterns,
                progress,
                pending: BTreeMap::new(),
                expired: false,
                named: name.is_some(),
            };
            check_group_budget(&s, &key, &g, &self.config)?;
            save_group(&mut s, &key, &g, &self.config)?;
            s.groups.insert(key.clone(), g);
        }
        s.consumer = consumer;
        Ok(Subscription {
            group: key,
            consumer,
        })
    }
    /// 交付一条事件；先落盘交付窗口，再返回事件，断线后可重投。
    /// Delivers one event after persisting its delivery window so disconnects remain replayable.
    pub fn next(&self, sub: &Subscription, now: Instant) -> Result<Option<Event>, DbError> {
        let mut s = self.lock()?;
        if s.journal.as_ref().is_some_and(|j| j.failed) {
            return Err(err(ErrorCode::JournalUnavailable, "Event Journal faulted"));
        }
        let mut g = s
            .groups
            .get(&sub.group)
            .cloned()
            .ok_or_else(|| err(ErrorCode::ConnectionState, "Subscription closed"))?;
        if g.expired
            || g.progress
                .iter()
                .any(|(&shard, p)| s.floors.get(&(g.db, shard)).copied().unwrap_or(0) > p.watermark)
        {
            return Err(err(
                ErrorCode::OffsetExpired,
                "Required event history has expired",
            ));
        }
        // 租约丢失仍等待 retry_interval；没有初次发送时间表示重启后待重投。
        // Lost leases still honor retry_interval; no send time means recovered work awaiting redelivery.
        let retry = g
            .pending
            .iter()
            .find(|(_, p)| {
                !p.acked
                    && p.sent.is_none_or(|sent| {
                        let elapsed = now.saturating_duration_since(sent);
                        elapsed >= Duration::from_millis(self.config.retry_interval_ms)
                            && (p.owner == 0
                                || elapsed
                                    >= Duration::from_millis(self.config.consumer_timeout_ms))
                    })
            })
            .map(|(id, _)| *id);
        if let Some((shard, seq)) = retry {
            let event = find(&mut s, g.db, shard, seq)?
                .ok_or_else(|| err(ErrorCode::OffsetExpired, "Pending event no longer retained"))?;
            let p = g.pending.get_mut(&(shard, seq)).unwrap();
            p.owner = sub.consumer;
            p.sent = Some(now);
            s.stats.redelivered = s.stats.redelivered.saturating_add(1);
            s.groups.insert(sub.group.clone(), g);
            return Ok(Some(event));
        }
        if g.pending.len() >= self.config.max_pending {
            return Ok(None);
        }
        let cursors: gather::Cursors = g
            .progress
            .iter()
            .map(|(&shard, p)| (shard, p.cursor))
            .collect();
        let events = read_after(&mut s, g.db, &cursors, self.config.buffer_bytes / 4)?;
        let mut result = None;
        let mut changed = false;
        for event in events {
            let shard = event.id.shard;
            let seq = event.id.sequence;
            let p = g.progress.get_mut(&shard).ok_or_else(codec::corrupt)?;
            if seq <= p.cursor {
                continue;
            }
            if seq != p.cursor + 1 {
                return Err(err(
                    ErrorCode::OffsetExpired,
                    "Gap in retained event history",
                ));
            }
            p.cursor = seq;
            changed = true;
            if g.patterns.iter().any(|p| matches(p, &event.key)) {
                g.pending.insert(
                    (shard, seq),
                    Pending {
                        acked: false,
                        owner: sub.consumer,
                        sent: Some(now),
                    },
                );
                g.advance(shard);
                result = Some(event);
                break;
            }
            g.advance(shard);
        }
        if changed {
            check_group_budget(&s, &sub.group, &g, &self.config)?;
            save_group(&mut s, &sub.group, &g, &self.config)?;
            s.groups.insert(sub.group.clone(), g);
        }
        Ok(result)
    }
    /// ACK 仅属于已交付组，连续水位前进前保留乱序空洞。
    /// ACKs belong only to delivered group events; out-of-order holes survive until contiguous progress.
    pub fn ack(&self, sub: &Subscription, id: EventId) -> Result<(), DbError> {
        let mut s = self.lock()?;
        if self.reliable() && s.journal.as_ref().is_some_and(|j| j.failed) {
            return Err(err(ErrorCode::JournalUnavailable, "Event Journal faulted"));
        }
        if id.stream_id != s.stream {
            return Err(err(ErrorCode::StreamMismatch, "Event stream differs"));
        }
        let mut g = s
            .groups
            .get(&sub.group)
            .cloned()
            .ok_or_else(|| err(ErrorCode::ConnectionState, "Subscription closed"))?;
        if g.expired {
            return Err(err(
                ErrorCode::OffsetExpired,
                "Required event history expired",
            ));
        }
        if id.db != g.db {
            return Err(err(ErrorCode::InvalidAck, "ACK database differs"));
        }
        let p = g
            .progress
            .get(&id.shard)
            .ok_or_else(|| err(ErrorCode::InvalidAck, "Unknown ACK shard"))?;
        if id.sequence <= p.origin {
            return Err(err(ErrorCode::InvalidAck, "Event predates this group"));
        }
        if id.sequence <= p.watermark {
            let event = find(&mut s, g.db, id.shard, id.sequence)?
                .ok_or_else(|| err(ErrorCode::OffsetExpired, "ACK history expired"))?;
            if g.patterns.iter().any(|p| matches(p, &event.key)) {
                return Ok(());
            }
            return Err(err(ErrorCode::InvalidAck, "Event was filtered out"));
        }
        let pending = g
            .pending
            .get_mut(&(id.shard, id.sequence))
            .ok_or_else(|| err(ErrorCode::InvalidAck, "Event was never delivered"))?;
        pending.acked = true;
        g.advance(id.shard);
        save_group(&mut s, &sub.group, &g, &self.config)?;
        s.groups.insert(sub.group.clone(), g);
        Ok(())
    }
    /// 断线释放租约；临时组销毁，命名组等待其他连接接手。
    /// Disconnect releases leases; temporary groups vanish and named groups await another consumer.
    pub fn disconnect(&self, sub: Subscription) {
        if let Ok(mut s) = self.state.lock() {
            if sub.group.starts_with("t:") {
                s.groups.remove(&sub.group);
            } else if let Some(g) = s.groups.get_mut(&sub.group) {
                for p in g.pending.values_mut() {
                    if p.owner == sub.consumer {
                        p.owner = 0;
                    }
                }
            }
        }
    }
    /// 按统一期限清理，可使落后组显式失效；不会缩短保留期限腾出容量。
    /// Retention cleanup may explicitly expire lagging groups and never shortens retention for capacity.
    pub fn maintain(&self, now_ms: u64) -> Result<(), DbError> {
        let mut s = self.lock()?;
        if self.config.reliable {
            compact(&mut s, &self.config, now_ms)
        } else {
            let cutoff = now_ms.saturating_sub(self.config.retention_ms);
            while s.fast.front().is_some_and(|e| e.timestamp_ms <= cutoff) {
                drop_front(&mut s, false)
            }
            Ok(())
        }
    }
    /// 固定规模运行统计；Fixed-size runtime statistics.
    pub fn stats(&self) -> Result<Stats, DbError> {
        let s = self.lock()?;
        let mut stats = s.stats;
        stats.groups = s.groups.len();
        stats.pending = s.groups.values().map(|g| g.pending.len()).sum();
        stats.journal_bytes = s.journal.as_ref().map_or(0, |j| j.bytes);
        Ok(stats)
    }
    /// 锁损坏明确失败；Poisoned ownership fails explicitly.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, DbError> {
        self.state
            .lock()
            .map_err(|_| err(ErrorCode::JournalUnavailable, "Event state unavailable"))
    }
}
/// 类型别名保持扫描水位含义明确；Alias makes scan cursors explicit.
mod gather {
    pub type Cursors = std::collections::BTreeMap<u32, u64>;
}
/// 单事件保守占用；Conservative per-event memory.
fn event_charge(e: &Event) -> usize {
    256 + e.key.len() * 2
}
/// 所有组含候选克隆预算；All groups include candidate clone allowance.
fn group_bytes(groups: &BTreeMap<String, Group>) -> usize {
    groups.iter().map(|(n, g)| g.charge(n)).sum()
}
/// 组元数据不占满事件缓冲；Group metadata cannot exhaust the event buffer.
fn check_group_budget(s: &State, key: &str, g: &Group, c: &Config) -> Result<(), DbError> {
    let bytes =
        group_bytes(&s.groups) - s.groups.get(key).map_or(0, |old| old.charge(key)) + g.charge(key);
    if bytes > c.buffer_bytes / 2 {
        return Err(limit());
    }
    Ok(())
}
/// 先持久化命名组状态，再发布内存；Persists named-group state before memory publication.
fn save_group(s: &mut State, name: &str, g: &Group, c: &Config) -> Result<(), DbError> {
    if g.named && c.reliable {
        let record = codec::group(name, g);
        if !s.journal.as_ref().unwrap().fits(record.len(), true) {
            compact(s, c, 0)?;
        }
        s.journal.as_mut().unwrap().append(&record, true)?;
    }
    Ok(())
}
/// 容量丢弃或期限删除只前移对应流下界；Drop or expiry advances only that stream's floor.
fn drop_front(s: &mut State, dropped: bool) {
    if let Some(e) = s.fast.pop_front() {
        s.bytes -= event_charge(&e);
        s.floors.insert((e.id.db, e.id.shard), e.id.sequence);
        if dropped {
            s.stats.dropped = s.stats.dropped.saturating_add(1)
        }
    }
}
/// 逐事务读取有界数量，不载入完整 Journal；Reads a bounded amount without loading the full journal.
fn read_after(
    s: &mut State,
    db: u16,
    cursors: &gather::Cursors,
    limit_bytes: usize,
) -> Result<Vec<Event>, DbError> {
    let mut out = Vec::new();
    let mut bytes = 0;
    let mut full = false;
    let mut accept = |e: Event| {
        if e.id.db == db && e.id.sequence > cursors.get(&e.id.shard).copied().unwrap_or(0) && !full
        {
            if bytes + event_charge(&e) > limit_bytes {
                full = true;
                return;
            }
            bytes += event_charge(&e);
            out.push(e)
        }
    };
    if let Some(j) = s.journal.as_mut() {
        let stream = s.stream;
        j.visit(|r| {
            if r[0] == codec::EVENTS {
                for e in codec::read_events(r, stream)? {
                    accept(e)
                }
            }
            Ok(())
        })?;
    } else {
        for e in &s.fast {
            accept(e.clone())
        }
    }
    Ok(out)
}
/// 按稳定 ID 查找一条事件；Finds one event by its stable ID.
fn find(s: &mut State, db: u16, shard: u32, sequence: u64) -> Result<Option<Event>, DbError> {
    if let Some(j) = s.journal.as_mut() {
        let stream = s.stream;
        let mut out = None;
        j.visit(|r| {
            if r[0] == codec::EVENTS {
                for e in codec::read_events(r, stream)? {
                    if e.id.db == db && e.id.shard == shard && e.id.sequence == sequence {
                        out = Some(e)
                    }
                }
            }
            Ok(())
        })?;
        Ok(out)
    } else {
        Ok(s.fast
            .iter()
            .find(|e| e.id.db == db && e.id.shard == shard && e.id.sequence == sequence)
            .cloned())
    }
}
/// 原子重写检查点及仍在保留期内的完整事务，不拆开多分片事务。
/// Atomically rewrites checkpoints and retained complete transactions without splitting cross-shard transactions.
fn compact(s: &mut State, c: &Config, now: u64) -> Result<(), DbError> {
    let cutoff = now.saturating_sub(c.retention_ms);
    let stream = s.stream;
    let mut floors = s.floors.clone();
    let j = s.journal.as_mut().unwrap();
    let mut prefix = true;
    j.visit(|r| {
        if r[0] == codec::EVENTS {
            let es = codec::read_events(r, stream)?;
            if prefix && es.iter().all(|e| e.timestamp_ms <= cutoff) {
                for e in es {
                    floors
                        .entry((e.id.db, e.id.shard))
                        .and_modify(|n| *n = (*n).max(e.id.sequence))
                        .or_insert(e.id.sequence);
                }
            } else {
                prefix = false;
            }
        }
        Ok(())
    })?;
    let mut groups = s.groups.clone();
    for g in groups.values_mut() {
        if g.progress
            .iter()
            .any(|(&shard, p)| floors.get(&(g.db, shard)).copied().unwrap_or(0) > p.watermark)
        {
            g.expired = true;
        }
    }
    let mut checkpoint = codec::Encoder(vec![codec::CHECKPOINT]);
    checkpoint.positions(&s.tails);
    checkpoint.positions(&floors);
    let states: Vec<_> = groups
        .iter()
        .filter(|(_, g)| g.named)
        .map(|(n, g)| codec::group(n, g))
        .collect();
    let mut prefix = true;
    j.compact(&checkpoint.0, &states, |r| {
        if r[0] != codec::EVENTS {
            return Ok(false);
        }
        if prefix
            && codec::read_events(r, stream)?
                .iter()
                .all(|e| e.timestamp_ms <= cutoff)
        {
            Ok(false)
        } else {
            prefix = false;
            Ok(true)
        }
    })?;
    s.floors = floors;
    s.groups = groups;
    Ok(())
}
/// 相同 SCAN 字节通配契约；Same byte-glob contract as SCAN.
pub(crate) fn matches(pattern: &str, key: &str) -> bool {
    let (p, k) = (pattern.as_bytes(), key.as_bytes());
    let (mut pi, mut ki, mut star, mut retry) = (0, 0, None, 0);
    while ki < k.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi + 1);
            pi += 1;
        } else {
            let (literal, step) = if pi < p.len() && p[pi] == b'\\' && pi + 1 < p.len() {
                (Some(p[pi + 1]), 2)
            } else {
                (p.get(pi).copied(), 1)
            };
            if literal == Some(k[ki]) || (pi < p.len() && p[pi] == b'?' && step == 1) {
                pi += step;
                ki += 1
            } else if let Some(after) = star {
                retry += 1;
                ki = retry;
                pi = after
            } else {
                return false;
            }
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1
    }
    pi == p.len()
}
/// 拒绝末尾转义；Rejects dangling escapes.
fn validate_pattern(p: &str) -> Result<(), DbError> {
    let mut escape = false;
    for b in p.bytes() {
        if escape {
            escape = false
        } else if b == b'\\' {
            escape = true
        }
    }
    if escape {
        Err(err(ErrorCode::InvalidArgument, "Dangling pattern escape"))
    } else {
        Ok(())
    }
}
/// 内存预算失败不伪装为提交未知；Memory rejection is never disguised as an unknown commit.
fn limit() -> DbError {
    err(
        ErrorCode::LimitExceeded,
        "Event memory or transaction budget exceeded",
    )
}
