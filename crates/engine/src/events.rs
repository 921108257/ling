//! 从最终候选推导生命周期事件；普通状态仅在事件提交回调中发布。
//! Derives lifecycle events from final candidates; ordinary state publishes only inside event commits.

use crate::{Entry, Metadata, TimeSnapshot, Transaction, unavailable};
use linger_events::Change;
use linger_types::{DbError, EventKind};

impl Transaction<'_> {
    /// 不复制 Value，身份与时间均来自取得执行权后的观察。
    /// Never copies Values; identities and time come from the observation after ownership.
    pub(crate) fn event(
        &self,
        db: u16,
        key: &str,
        metadata: Metadata,
        kind: EventKind,
        time: TimeSnapshot,
    ) -> Change {
        Change {
            db,
            shard: self.engine.shard_for(key),
            kind,
            key: key.into(),
            generation: metadata.generation,
            revision: metadata.revision,
            timestamp_ms: time.unix_ms.max(0) as u64,
        }
    }

    /// 过期保留原原因；值和 TTL 同变使用同一最终版本并保持值事件在前。
    /// Expiry preserves its reason; simultaneous value and TTL changes share a revision with value first.
    pub(crate) fn describe(
        &self,
        db: u16,
        key: &str,
        old: Option<&Entry>,
        new: Option<&Entry>,
        time: TimeSnapshot,
        out: &mut Vec<Change>,
    ) {
        let active = old.filter(|e| {
            e.metadata
                .deadline
                .is_none_or(|d| time.monotonic < d.monotonic)
        });
        if let Some(old) = old.filter(|_| active.is_none()) {
            out.push(self.event(db, key, old.metadata, EventKind::Expired, time));
        }
        match (active, new) {
            (None, Some(new)) => {
                out.push(self.event(db, key, new.metadata, EventKind::Created, time))
            }
            (Some(old), None) => {
                out.push(self.event(db, key, old.metadata, EventKind::Deleted, time))
            }
            (Some(old), Some(new)) => {
                if old.value != new.value || old.metadata.generation != new.metadata.generation {
                    out.push(self.event(db, key, new.metadata, EventKind::Updated, time));
                }
                if old.metadata.deadline != new.metadata.deadline {
                    out.push(self.event(db, key, new.metadata, EventKind::TtlChanged, time));
                }
            }
            _ => {}
        }
    }

    /// Reliable 下先记账本次锁定分片的自然过期，失败保留条目并阻止该分片写入。
    /// Accounts due expirations in locked Reliable shards first; failures preserve entries and block shard writes.
    pub(crate) fn account_expired(&mut self, time: TimeSnapshot) -> Result<(), DbError> {
        if !self
            .engine
            .events
            .as_ref()
            .is_some_and(|s| s.enabled() && s.reliable())
        {
            return Ok(());
        }
        // 每次仅准备一个终结事实，避免大积压超出单事务预算后永远无法推进。
        // Prepare one terminal fact at a time so a large backlog cannot permanently exceed transaction budgets.
        for pos in 0..self.guards.len() {
            while let Some((key, metadata, bytes)) = self.guards[pos]
                .1
                .iter()
                .find(|(_, e)| {
                    e.metadata
                        .deadline
                        .is_some_and(|d| time.monotonic >= d.monotonic)
                })
                .map(|(k, e)| (k.clone(), e.metadata, e.bytes))
            {
                let id = self.guards[pos].0;
                let event = self.event(
                    (id / self.config.shard_count as usize) as u16,
                    &key,
                    metadata,
                    EventKind::Expired,
                    time,
                );
                let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
                self.engine.events.as_ref().unwrap().commit(&[event], || {
                    budget.schedule(id, &key, Some(metadata), None);
                    self.guards[pos].1.remove(&key);
                    budget.memory -= bytes;
                })?;
            }
        }
        Ok(())
    }
}
