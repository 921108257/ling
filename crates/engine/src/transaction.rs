//! 已取得分片执行权后的查找和提交；预算锁总在分片锁之后。
//! Lookups and commits after shard ownership; budget locks always follow shard locks.

use crate::{Entry, Metadata, Transaction, entry_size, support::*, unavailable};
use linger_types::{DbError, ErrorCode, Generation, Value};
use std::{collections::BTreeMap, time::Instant};

impl Transaction<'_> {
    /// 仅访问协调器预先锁定的分片；缺少执行权是内部编程错误。
    /// Accesses only prelocked shards; missing ownership is an internal programming error.
    fn index(&self, db: u16, key: &str) -> &crate::Index {
        let id = self.engine.location(db, key).expect("validated database");
        &self.guards[self
            .guards
            .binary_search_by_key(&id, |(id, _)| *id)
            .expect("shard not locked")]
        .1
    }

    /// 枚举当前命令已经锁定的数据库条目，不获取额外锁。
    /// Iterates already locked entries for this database without taking extra locks.
    pub(crate) fn entries(&self, db: u16) -> impl Iterator<Item = (&String, &Entry)> {
        let range = self.engine.db_locations(db);
        self.guards
            .iter()
            .filter(move |(id, _)| range.contains(id))
            .flat_map(|(_, index)| index.iter())
    }

    /// 所有入口使用同一个观察时点过滤到期状态。
    /// Every entry point filters expiry at the same observation instant.
    pub(crate) fn entry(&self, db: u16, key: &str, now: Instant) -> Result<&Entry, DbError> {
        self.index(db, key)
            .get(key)
            .filter(|e| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
            .ok_or_else(|| error(ErrorCode::KeyNotFound, "Key not found"))
    }

    /// 在提交锁中预检完整变更集，之后不再执行可失败业务操作。
    /// Preflights the whole change set under the commit lock, then performs no fallible business operations.
    pub(crate) fn publish(
        &mut self,
        db: u16,
        effect: Effect,
        now: Instant,
    ) -> Result<Value, DbError> {
        if effect.changes.is_empty() {
            return Ok(effect.result);
        }
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let mut memory = budget.memory;
        let mut key_counts = BTreeMap::new();
        if self.config.max_keys != 0 {
            for change in &effect.changes {
                let target = change.db.unwrap_or(db);
                key_counts.entry(target).or_insert_with(|| {
                    self.entries(target)
                        .filter(|(_, e)| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
                        .count()
                });
            }
        }
        let mut creations = budget.creations;
        let mut prepared = Vec::with_capacity(effect.changes.len());
        for change in effect.changes {
            let target = change.db.unwrap_or(db);
            let old = self.index(target, &change.key).get(&change.key);
            let active = old.filter(|e| e.metadata.deadline.is_none_or(|d| now < d.monotonic));
            let recreate = matches!(change.policy, Mutation::Recreate(_));
            let deadline = match change.policy {
                Mutation::Preserve => active.and_then(|e| e.metadata.deadline),
                Mutation::Replace => None,
                Mutation::Recreate(deadline) => deadline,
            };
            if !recreate
                && active.is_some_and(|e| {
                    change.value.as_ref() == Some(&e.value) && e.metadata.deadline == deadline
                })
                || (change.value.is_none() && old.is_none())
            {
                continue;
            }
            if let Some(old) = old {
                memory -= old.bytes;
            }
            if self.config.max_keys != 0 && active.is_some() {
                *key_counts.get_mut(&target).unwrap() -= 1;
            }
            let next = if let Some(value) = change.value {
                value.validate(self.config.value)?;
                let bytes = entry_size(&change.key, &value)?;
                memory = memory.checked_add(bytes).ok_or_else(budget_error)?;
                if let Some(keys) = key_counts.get_mut(&target) {
                    *keys += 1;
                }
                let metadata = if let Some(entry) = active.filter(|_| !recreate) {
                    Metadata {
                        generation: entry.metadata.generation,
                        revision: entry
                            .metadata
                            .revision
                            .checked_add(1)
                            .ok_or_else(|| error(ErrorCode::Overflow, "Revision exhausted"))?,
                        deadline,
                    }
                } else {
                    creations = creations.checked_add(1).ok_or_else(|| {
                        error(ErrorCode::Overflow, "Generation counter exhausted")
                    })?;
                    Metadata {
                        generation: Generation {
                            epoch: self.engine.epoch,
                            counter: creations,
                        },
                        revision: 1,
                        deadline,
                    }
                };
                Some(Entry {
                    value,
                    metadata,
                    bytes,
                })
            } else {
                None
            };
            prepared.push((target, change.key, next));
        }
        if (self.config.max_memory_bytes != 0
            && memory > self.config.max_memory_bytes
            && memory > budget.memory)
            || (key_counts.values().any(|keys| *keys > self.config.max_keys))
        {
            return Err(budget_error());
        }
        for (target, key, entry) in prepared {
            let id = self
                .engine
                .location(target, &key)
                .expect("validated target");
            let pos = self
                .guards
                .binary_search_by_key(&id, |(id, _)| *id)
                .expect("shard not locked");
            let index = &mut self.guards[pos].1;
            match entry {
                Some(entry) => {
                    index.insert(key, entry);
                }
                None => {
                    index.remove(&key);
                }
            }
        }
        budget.creations = creations;
        budget.memory = memory;
        Ok(effect.result)
    }
}
