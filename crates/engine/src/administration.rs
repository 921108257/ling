//! 跨库迁入与整库变更；验证身份、容量和响应后原子发布。
//! Cross-database relocation and whole-database mutations publish after identity, capacity, and response checks.

use crate::{Metadata, TimeSnapshot, Transaction, copy_options, support::*, unavailable};
use linger_types::{DbError, ErrorCode, Generation, Value};
use std::time::Instant;

impl Transaction<'_> {
    /// 移动和复制保留源期限，目标创建新身份；源不存在始终报错。
    /// Moves and copies preserve source deadlines with fresh target identities; missing sources always fail.
    pub(crate) fn relocate(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
    ) -> Result<Effect, DbError> {
        if name != "COPY" && args.len() != 2 {
            return Err(argument_error());
        }
        let (target_db, replace) = if name == "COPY" {
            copy_options(self.engine, db, args)?
        } else if name == "MOVE" {
            (self.engine.database_arg(&args[1])?, false)
        } else {
            (db, name == "RENAME")
        };
        let source = self.key(&args[0])?;
        let destination = if name == "MOVE" {
            source
        } else {
            self.key(&args[1])?
        };
        let entry = self.entry(db, source, now)?;
        if db == target_db && source == destination {
            return if name == "COPY" {
                Err(argument_error())
            } else {
                Ok(Effect::read(Value::Bool(name == "RENAME")))
            };
        }
        if !replace && self.entry(target_db, destination, now).is_ok() {
            return Ok(Effect::read(Value::Bool(false)));
        }
        let mut changes = vec![Change {
            key: destination.into(),
            db: Some(target_db),
            value: Some(entry.value.clone()),
            policy: Mutation::Recreate(entry.metadata.deadline),
        }];
        if name != "COPY" {
            changes.insert(
                0,
                Change {
                    key: source.into(),
                    db: Some(db),
                    value: None,
                    policy: Mutation::Replace,
                },
            );
        }
        Ok(Effect {
            result: Value::Bool(true),
            changes,
        })
    }

    /// 整库操作移动索引所有权，避免复制整库 Value；全部错误在移动前返回。
    /// Whole-DB operations move index ownership rather than cloning Values; all errors precede the move.
    pub(crate) fn administration(
        &mut self,
        db: u16,
        name: &str,
        args: &[Value],
        time: TimeSnapshot,
        response_bytes: usize,
    ) -> Result<Value, DbError> {
        response_size(&Value::Bool(true), response_bytes, self.config.value)?;
        if name == "SWAPDB" {
            return self.swap(args, time);
        }
        let asynchronous = match args {
            [] => false,
            [Value::String(option)] if option.eq_ignore_ascii_case("ASYNC") => true,
            _ => return Err(argument_error()),
        };
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let mut events = Vec::new();
        if self.engine.events.as_ref().is_some_and(|s| s.enabled()) {
            for (id, index) in &self.guards {
                if name == "FLUSHALL" || self.engine.db_locations(db).contains(id) {
                    let target = (id / self.config.shard_count as usize) as u16;
                    for (key, entry) in index.iter() {
                        self.describe(target, key, Some(entry), None, time, &mut events);
                    }
                }
            }
        }
        let mut retired = Vec::with_capacity(self.guards.len());
        let mut bytes = 0;
        let store = self.engine.events.as_ref();
        let mut publish = || {
            for (id, index) in &mut self.guards {
                if name == "FLUSHALL" || self.engine.db_locations(db).contains(id) {
                    bytes += index.values().map(|e| e.bytes).sum::<usize>();
                    for (key, entry) in index.iter() {
                        budget.schedule(*id, key, Some(entry.metadata), None);
                    }
                    retired.push(std::mem::take(&mut **index));
                }
            }
        };
        match store {
            Some(s) => s.commit(&events, publish)?,
            None => publish(),
        }
        if asynchronous {
            drop(budget);
            self.engine.retire(retired, bytes);
        } else {
            drop(retired);
            budget.memory -= bytes;
        }
        Ok(Value::Bool(true))
    }

    /// 先准备全部迁入身份和事件，再移动索引；Journal 失败保留两个库。
    /// Prepares all incoming identities and events before moving indexes; Journal failures preserve both databases.
    fn swap(&mut self, args: &[Value], time: TimeSnapshot) -> Result<Value, DbError> {
        let [a, b] = args else {
            return Err(argument_error());
        };
        let (a, b) = (self.engine.database_arg(a)?, self.engine.database_arg(b)?);
        if a == b {
            return Ok(Value::Bool(true));
        }
        let now = time.monotonic;
        let active = |e: &&crate::Entry| e.metadata.deadline.is_none_or(|d| now < d.monotonic);
        let (ac, bc) = (
            self.entries(a).map(|(_, e)| e).filter(active).count(),
            self.entries(b).map(|(_, e)| e).filter(active).count(),
        );
        if self.config.max_keys != 0 && ac.max(bc) > self.config.max_keys {
            return Err(budget_error());
        }
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let end = budget
            .creations
            .checked_add((ac + bc) as u64)
            .ok_or_else(|| error(ErrorCode::Overflow, "Generation counter exhausted"))?;
        let mut counter = budget.creations;
        let mut incoming = std::collections::BTreeMap::new();
        for (origin, dest) in [(a, b), (b, a)] {
            for (key, entry) in self.entries(origin) {
                if !active(&entry) {
                    continue;
                }
                counter += 1;
                incoming.insert(
                    (dest, key.clone()),
                    Metadata {
                        generation: Generation {
                            epoch: self.engine.epoch,
                            counter,
                        },
                        revision: 1,
                        deadline: entry.metadata.deadline,
                    },
                );
            }
        }
        let mut events = Vec::new();
        if self.engine.events.as_ref().is_some_and(|s| s.enabled()) {
            for (target, origin) in [(a, b), (b, a)] {
                let keys: std::collections::BTreeSet<_> = self
                    .entries(target)
                    .chain(self.entries(origin))
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in keys {
                    let old = self
                        .entries(target)
                        .find(|(k, _)| **k == key)
                        .map(|(_, e)| e);
                    let next = incoming.get(&(target, key.clone())).copied();
                    let old_active = old.filter(|e| active(e));
                    if let Some(e) = old.filter(|_| old_active.is_none()) {
                        events.push(self.event(
                            target,
                            &key,
                            e.metadata,
                            linger_types::EventKind::Expired,
                            time,
                        ));
                    }
                    match (old_active, next) {
                        (None, Some(m)) => events.push(self.event(
                            target,
                            &key,
                            m,
                            linger_types::EventKind::Created,
                            time,
                        )),
                        (Some(e), None) => events.push(self.event(
                            target,
                            &key,
                            e.metadata,
                            linger_types::EventKind::Deleted,
                            time,
                        )),
                        (Some(e), Some(m)) => {
                            events.push(self.event(
                                target,
                                &key,
                                m,
                                linger_types::EventKind::Updated,
                                time,
                            ));
                            if e.metadata.deadline != m.deadline {
                                events.push(self.event(
                                    target,
                                    &key,
                                    m,
                                    linger_types::EventKind::TtlChanged,
                                    time,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let store = self.engine.events.as_ref();
        let mut publish = || {
            for shard in 0..self.config.shard_count as usize {
                let ai = a as usize * self.config.shard_count as usize + shard;
                let bi = b as usize * self.config.shard_count as usize + shard;
                let ap = self
                    .guards
                    .binary_search_by_key(&ai, |(id, _)| *id)
                    .unwrap();
                let bp = self
                    .guards
                    .binary_search_by_key(&bi, |(id, _)| *id)
                    .unwrap();
                let mut left = std::mem::take(&mut *self.guards[ap].1);
                let mut right = std::mem::take(&mut *self.guards[bp].1);
                for (index, origin, destination, db) in
                    [(&mut left, ai, bi, b), (&mut right, bi, ai, a)]
                {
                    index.retain(|key, entry| {
                        budget.schedule(origin, key, Some(entry.metadata), None);
                        let Some(metadata) = incoming.get(&(db, key.clone())).copied() else {
                            budget.memory -= entry.bytes;
                            return false;
                        };
                        entry.metadata = metadata;
                        budget.schedule(destination, key, None, Some(metadata));
                        true
                    });
                }
                *self.guards[ap].1 = right;
                *self.guards[bp].1 = left;
            }
            budget.creations = end;
            Value::Bool(true)
        };
        match store {
            Some(s) => s.commit(&events, publish),
            None => Ok(publish()),
        }
    }
}
