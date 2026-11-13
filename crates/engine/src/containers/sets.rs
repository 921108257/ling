//! Set 按 Scalar 类型敏感相等；随机选择不承诺密码学强度。
//! Sets use type-sensitive Scalar equality; random selection is not cryptographic.
use crate::{Engine, support::*};
use linger_types::{DbError, Scalar, Value, ValueLimits};
use rand::seq::{IndexedRandom, SliceRandom};
use std::{collections::HashSet, time::Instant};

/// 已验证标量的编码长度，不复制 String 或 Bytes。
/// Returns the encoded length of a validated scalar without cloning String or Bytes.
fn scalar_size(item: &Scalar) -> usize {
    match item {
        Scalar::Null => 1,
        Scalar::Bool(_) => 2,
        Scalar::String(s) => 5 + s.len(),
        Scalar::Bytes(b) => 5 + b.len(),
        _ => 9,
    }
}

/// 复制成员前检查去重后集合的完整长度。
/// Checks the complete deduplicated set length before cloning members.
fn set(items: HashSet<&Scalar>, limits: ValueLimits) -> Result<Value, DbError> {
    let mut bytes = 5usize;
    u32::try_from(items.len()).map_err(|_| limit_error())?;
    for item in &items {
        let len = scalar_size(item);
        bytes = bytes
            .checked_add(len)
            .filter(|n| *n <= limits.max_bytes)
            .ok_or_else(limit_error)?;
    }
    if bytes > limits.max_bytes {
        return Err(limit_error());
    }
    Ok(Value::Set(items.into_iter().cloned().collect()))
}

impl Engine {
    /// 先验证全部成员，STORE 先读齐源再替换目标。
    /// Validates all members first; STORE reads all sources before replacing its destination.
    pub(crate) fn set_command(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
        response_bytes: usize,
    ) -> Result<Effect, DbError> {
        if matches!(
            name,
            "SUNION" | "SINTER" | "SDIFF" | "SUNIONSTORE" | "SINTERSTORE" | "SDIFFSTORE"
        ) {
            return self.set_algebra(db, name, args, now);
        }
        let valid = match name {
            "SADD" | "SREM" | "SMISMEMBER" => args.len() >= 2,
            "SISMEMBER" => args.len() == 2,
            "SRANDMEMBER" | "SPOP" => matches!(args.len(), 1 | 2),
            _ => args.len() == 1,
        };
        if !valid {
            return Err(argument_error());
        }
        let key = self.key(&args[0])?;
        let Value::Set(members) = &self.entry(db, key, now)?.value else {
            return Err(type_error());
        };
        match name {
            "SCARD" => Ok(Effect::read(Value::UInt(members.len() as u64))),
            "SMEMBERS" => Ok(Effect::read(Value::Set(members.clone()))),
            "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" => {
                let inputs = args[1..]
                    .iter()
                    .cloned()
                    .map(Scalar::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                if name == "SISMEMBER" {
                    return Ok(Effect::read(Value::Bool(members.contains(&inputs[0]))));
                }
                if name == "SMISMEMBER" {
                    return Ok(Effect::read(Value::List(
                        inputs
                            .iter()
                            .map(|v| Value::Bool(members.contains(v)))
                            .collect(),
                    )));
                }
                let mut candidate: HashSet<_> = members.iter().collect();
                let mut changed = 0;
                for input in &inputs {
                    if if name == "SADD" {
                        candidate.insert(input)
                    } else {
                        candidate.remove(input)
                    } {
                        changed += 1;
                    }
                }
                Ok(Effect::put(
                    Value::UInt(changed),
                    key,
                    set(candidate, self.config.value)?,
                    true,
                ))
            }
            "SRANDMEMBER" | "SPOP" => {
                let (amount, repeat) = if name == "SPOP" {
                    (args.get(1).map(count).transpose()?.unwrap_or(1), false)
                } else {
                    let amount = args.get(1).map(integer).transpose()?.unwrap_or(1);
                    (amount.unsigned_abs(), amount < 0)
                };
                let amount = if members.is_empty() {
                    0
                } else if repeat {
                    amount
                } else {
                    amount.min(members.len() as u64)
                };
                let amount = usize::try_from(amount).map_err(|_| limit_error())?;
                // 最小标签字节也必须先计入，避免有符号 count 导致巨量分配。
                // Charge even minimum tag bytes first to prevent huge signed-count allocations.
                if amount.checked_add(6).is_none_or(|n| n > response_bytes)
                    || amount
                        .checked_mul(2 * std::mem::size_of::<Value>())
                        .is_none_or(|n| n > 128 * 1024 * 1024)
                {
                    return Err(limit_error());
                }
                let mut rng = rand::rng();
                let mut pool: Vec<_> = members.iter().collect();
                if repeat {
                    let mut wire = 6usize;
                    let mut memory = amount * 2 * std::mem::size_of::<Value>();
                    let mut result = Vec::new();
                    for _ in 0..amount {
                        let member = *pool.choose(&mut rng).unwrap();
                        let (len, heap) = match member {
                            Scalar::Null => (1, 0),
                            Scalar::Bool(_) => (2, 0),
                            Scalar::String(v) => (5 + v.len(), v.len()),
                            Scalar::Bytes(v) => (5 + v.len(), v.len()),
                            _ => (9, 0),
                        };
                        wire = wire
                            .checked_add(len)
                            .filter(|n| *n <= response_bytes)
                            .ok_or_else(limit_error)?;
                        memory = memory
                            .checked_add(heap)
                            .filter(|n| *n <= 128 * 1024 * 1024)
                            .ok_or_else(limit_error)?;
                        result.push(Value::from(member.clone()));
                    }
                    return Ok(Effect::read(Value::List(result)));
                }
                pool.shuffle(&mut rng);
                let selected = &pool[..amount];
                let mut wire = 6usize;
                for member in selected {
                    wire = wire
                        .checked_add(scalar_size(member))
                        .filter(|n| *n <= response_bytes)
                        .ok_or_else(limit_error)?;
                }
                let values: Vec<_> = selected.iter().map(|v| Value::from((*v).clone())).collect();
                let result = list(values.iter(), self.config.value)?;
                if name == "SRANDMEMBER" {
                    return Ok(Effect::read(result));
                }
                let removed: HashSet<_> = selected.iter().copied().collect();
                Ok(Effect::put(
                    result,
                    key,
                    set(
                        members.iter().filter(|v| !removed.contains(v)).collect(),
                        self.config.value,
                    )?,
                    true,
                ))
            }
            _ => unreachable!(),
        }
    }

    /// 单所有者内以借用成员准备集合代数结果。
    /// Prepares set-algebra results from borrowed members within one owner.
    fn set_algebra(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
    ) -> Result<Effect, DbError> {
        let store = name.ends_with("STORE");
        if args.len() < if store { 2 } else { 1 } {
            return Err(argument_error());
        }
        let destination = if store {
            Some(self.key(&args[0])?)
        } else {
            None
        };
        let keys = args[usize::from(store)..]
            .iter()
            .map(|v| self.key(v))
            .collect::<Result<Vec<_>, _>>()?;
        let mut sources = Vec::with_capacity(keys.len());
        for key in keys {
            let Value::Set(members) = &self.entry(db, key, now)?.value else {
                return Err(type_error());
            };
            sources.push(members);
        }
        let mut result: HashSet<_> = sources[0].iter().collect();
        let mut bytes = 5 + sources[0].iter().map(scalar_size).sum::<usize>();
        for source in &sources[1..] {
            if name.starts_with("SUNION") {
                for member in source.iter() {
                    if !result.contains(member) {
                        // 并集只增不减，在扩展候选表前拒绝超限结果。
                        // A union only grows; reject an oversized result before extending the table.
                        bytes = bytes
                            .checked_add(scalar_size(member))
                            .filter(|n| *n <= self.config.value.max_bytes)
                            .ok_or_else(limit_error)?;
                        result.insert(member);
                    }
                }
            } else if name.starts_with("SINTER") {
                result.retain(|v| source.contains(*v));
            } else {
                result.retain(|v| !source.contains(*v));
            }
        }
        let value = set(result, self.config.value)?;
        if let Some(key) = destination {
            let Value::Set(ref members) = value else {
                unreachable!()
            };
            Ok(Effect::put(
                Value::UInt(members.len() as u64),
                key,
                value,
                false,
            ))
        } else {
            Ok(Effect::read(value))
        }
    }
}
