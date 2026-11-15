//! 有界到期索引：每条目最多一项，变更时同步替换，维护先取候选再按正常锁序重验。
//! Bounded expiry index: one item per entry, replaced on mutations and rechecked under normal lock order.

use crate::{Budget, Engine, Metadata, TimeSnapshot, Transaction, unavailable};
use linger_types::DbError;
use std::time::Instant;

/// 包含完整身份的定时记录；排序先按期限，再按存储位置和身份。
/// Timer record with complete identity, ordered by deadline, storage location, and identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExpiryTask {
    /// 单调到期点；Monotonic deadline.
    when: Instant,
    /// 全局 (db, shard) 序号；Global (db, shard) index.
    location: usize,
    /// 完整 Key，禁止哈希冲突误删；Complete key, preventing hash-collision deletions.
    key: String,
    /// 创建进程身份；Creating process identity.
    epoch: u128,
    /// 生命周期创建序号；Lifecycle creation sequence.
    generation: u64,
    /// 生成定时记录时的修订；Revision when this record was created.
    revision: u64,
    /// 查询期限也是需要校验的期限组成部分；Queryable deadline is part of validation too.
    unix_ms: i64,
}

impl ExpiryTask {
    /// 没有期限就没有定时项；不复制缓存 Value。
    /// No deadline means no timer item; cached Values are never copied.
    pub(crate) fn new(location: usize, key: &str, metadata: Metadata) -> Option<Self> {
        let deadline = metadata.deadline?;
        Some(Self {
            when: deadline.monotonic,
            location,
            key: key.into(),
            epoch: metadata.generation.epoch,
            generation: metadata.generation.counter,
            revision: metadata.revision,
            unix_ms: deadline.unix_ms,
        })
    }

    /// 精确匹配 generation、revision 和双期限；删除重建不复用旧身份。
    /// Matches generation, revision, and both deadlines exactly; recreated keys never reuse old identities.
    fn matches(&self, metadata: Metadata) -> bool {
        metadata.generation.epoch == self.epoch
            && metadata.generation.counter == self.generation
            && metadata.revision == self.revision
            && metadata
                .deadline
                .is_some_and(|d| d.monotonic == self.when && d.unix_ms == self.unix_ms)
    }
}

impl Budget {
    /// 与值发布共用提交锁，先移除旧记录再插入新记录，续期不积累历史。
    /// Shares the value publication lock, replacing old records without accumulating renewal history.
    pub(crate) fn schedule(
        &mut self,
        location: usize,
        key: &str,
        old: Option<Metadata>,
        new: Option<Metadata>,
    ) {
        if let Some(task) = old.and_then(|metadata| ExpiryTask::new(location, key, metadata)) {
            self.expirations.remove(&task);
        }
        if let Some(task) = new.and_then(|metadata| ExpiryTask::new(location, key, metadata)) {
            self.expirations.insert(task);
        }
    }
}

impl Engine {
    /// 当前定时项数，可用于检查维护积压；不包含已经移除的异步退役索引。
    /// Current timer count for inspecting maintenance backlog, excluding removed asynchronous retirements.
    pub fn scheduled_expirations(&self) -> Result<usize, DbError> {
        Ok(self
            .budget
            .lock()
            .map_err(|_| unavailable())?
            .expirations
            .len())
    }

    /// 最多检查 limit 个到期候选并返回回收数；零预算不执行任何工作。
    /// Checks at most limit due candidates and returns the reclaimed count; a zero budget does no work.
    /// 不持提交锁等待分片锁；事件禁用时直接回收，逻辑可见性始终由读路径保证。
    /// Never waits for shards while holding the commit lock; events-disabled reclamation is independent of read visibility.
    pub fn expire_due(&self, limit: usize) -> Result<usize, DbError> {
        let mut reclaimed = 0;
        for _ in 0..limit {
            let now = self.clock.now()?.monotonic;
            let task = self
                .budget
                .lock()
                .map_err(|_| unavailable())?
                .expirations
                .first()
                .filter(|task| task.when <= now)
                .cloned();
            let Some(task) = task else {
                break;
            };
            let mut tx = self.lock(vec![task.location])?;
            // 等待期间可能发生续期、重建或迁移，因此取得执行权后再次取时间和身份。
            // Renewal, recreation, or relocation may happen while waiting, so resample and recheck after ownership.
            reclaimed += usize::from(tx.expire_task(&task, self.clock.now()?)?);
        }
        Ok(reclaimed)
    }
}

impl Transaction<'_> {
    /// 过期不增加修订；过时任务只能移除自身，不能触碰新生命周期的记录。
    /// Expiry never increments revisions; a stale task can remove only itself, never a newer lifecycle's timer.
    pub(crate) fn expire_task(
        &mut self,
        task: &ExpiryTask,
        time: TimeSnapshot,
    ) -> Result<bool, DbError> {
        if time.monotonic < task.when {
            return Ok(false);
        }
        let position = self
            .guards
            .binary_search_by_key(&task.location, |(id, _)| *id)
            .unwrap();
        let index = &self.guards[position].1;
        let metadata = index
            .get(&task.key)
            .filter(|e| task.matches(e.metadata))
            .map(|e| e.metadata);
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let events = metadata
            .map(|m| {
                self.event(
                    (task.location / self.config.shard_count as usize) as u16,
                    &task.key,
                    m,
                    linger_types::EventKind::Expired,
                    time,
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        let store = self.engine.events.as_ref();
        let mut publish = || {
            budget.expirations.remove(task);
            if metadata.is_some() {
                let entry = self.guards[position].1.remove(&task.key).unwrap();
                budget.memory -= entry.bytes;
            }
            metadata.is_some()
        };
        match store {
            Some(s) => s.commit(&events, publish),
            None => Ok(publish()),
        }
    }
}
