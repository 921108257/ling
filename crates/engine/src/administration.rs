//! 跨库迁入与整库变更；验证身份、容量和响应后原子发布。
//! Cross-database relocation and whole-database mutations publish after identity, capacity, and response checks.

use crate::{Metadata, Transaction, copy_options, support::*, unavailable};
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
            changes.push(Change {
                key: source.into(),
                db: Some(db),
                value: None,
                policy: Mutation::Replace,
            });
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
        now: Instant,
        response_bytes: usize,
    ) -> Result<Value, DbError> {
        response_size(&Value::Bool(true), response_bytes, self.config.value)?;
        if name == "SWAPDB" {
            return self.swap(args, now);
        }
        let asynchronous = match args {
            [] => false,
            [Value::String(option)] if option.eq_ignore_ascii_case("ASYNC") => true,
            _ => return Err(argument_error()),
        };
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let mut retired = Vec::with_capacity(self.guards.len());
        for (id, index) in &mut self.guards {
            if name == "FLUSHALL" || self.engine.db_locations(db).contains(id) {
                budget.memory -= index.values().map(|e| e.bytes).sum::<usize>();
                retired.push(std::mem::take(&mut **index));
            }
        }
        drop(budget);
        if asynchronous {
            self.engine.retire(retired);
        } else {
            drop(retired);
        }
        Ok(Value::Bool(true))
    }

    /// 新库中每个活跃条目都取得新身份；已到期值不被交换复活。
    /// Each active entry gets a fresh identity in its new database; expired values never revive.
    fn swap(&mut self, args: &[Value], now: Instant) -> Result<Value, DbError> {
        let [a, b] = args else {
            return Err(argument_error());
        };
        let (a, b) = (self.engine.database_arg(a)?, self.engine.database_arg(b)?);
        if a == b {
            return Ok(Value::Bool(true));
        }
        let active = |db| {
            self.entries(db)
                .filter(|(_, e)| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
                .count()
        };
        let (ac, bc) = (active(a), active(b));
        if self.config.max_keys != 0 && ac.max(bc) > self.config.max_keys {
            return Err(budget_error());
        }
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let end = budget
            .creations
            .checked_add((ac + bc) as u64)
            .ok_or_else(|| error(ErrorCode::Overflow, "Generation counter exhausted"))?;
        let mut counter = budget.creations;
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
            for index in [&mut left, &mut right] {
                index.retain(|_, entry| {
                    if entry.metadata.deadline.is_some_and(|d| now >= d.monotonic) {
                        budget.memory -= entry.bytes;
                        return false;
                    }
                    counter += 1;
                    entry.metadata = Metadata {
                        generation: Generation {
                            epoch: self.engine.epoch,
                            counter,
                        },
                        revision: 1,
                        deadline: entry.metadata.deadline,
                    };
                    true
                });
            }
            *self.guards[ap].1 = right;
            *self.guards[bp].1 = left;
        }
        budget.creations = end;
        Ok(Value::Bool(true))
    }
}
