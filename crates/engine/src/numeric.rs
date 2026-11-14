//! 数值操作保留类型与 TTL，并在宽整数中校验溢出。
//! Numeric operations preserve type and TTL and check overflow with wide integers.
use crate::{Transaction, support::*};
use linger_types::{DbError, ErrorCode, Float, Value};
use std::time::Instant;

impl Transaction<'_> {
    /// 失败仅丢弃候选值，不发布部分修改。
    /// Failures discard the candidate without publishing partial mutations.
    pub(crate) fn numeric(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
    ) -> Result<Effect, DbError> {
        let arity = if matches!(name, "INCRBY" | "DECRBY" | "INCRFLOAT" | "APPEND") {
            2
        } else {
            1
        };
        if args.len() != arity {
            return Err(argument_error());
        }
        let key = self.key(&args[0])?;
        let value = &self.entry(db, key, now)?.value;
        let overflow = || error(ErrorCode::Overflow, "Numeric result out of range");
        let next = match name {
            "INCR" | "DECR" | "INCRBY" | "DECRBY" => {
                let step = if arity == 2 {
                    i128::from(integer(&args[1])?)
                } else {
                    1
                };
                let delta = if matches!(name, "DECR" | "DECRBY") {
                    -step
                } else {
                    step
                };
                match value {
                    Value::Int(v) => {
                        Value::Int(i64::try_from(i128::from(*v) + delta).map_err(|_| overflow())?)
                    }
                    Value::UInt(v) => {
                        Value::UInt(u64::try_from(i128::from(*v) + delta).map_err(|_| overflow())?)
                    }
                    _ => return Err(type_error()),
                }
            }
            "INCRFLOAT" => {
                let Value::Float(delta) = &args[1] else {
                    return Err(argument_error());
                };
                let Value::Float(old) = value else {
                    return Err(type_error());
                };
                let result = old.get() + delta.get();
                let number = Float::new(result)?;
                if old.get().is_finite() && delta.get().is_finite() && result.is_infinite() {
                    return Err(overflow());
                }
                Value::Float(number)
            }
            "STRLEN" => {
                let Value::String(v) = value else {
                    return Err(type_error());
                };
                return Ok(Effect::read(Value::UInt(v.len() as u64)));
            }
            "BLEN" => {
                let Value::Bytes(v) = value else {
                    return Err(type_error());
                };
                return Ok(Effect::read(Value::UInt(v.len() as u64)));
            }
            "APPEND" => {
                let appended = text(&args[1])?;
                let Value::String(old) = value else {
                    return Err(type_error());
                };
                let len = old
                    .len()
                    .checked_add(appended.len())
                    .ok_or_else(limit_error)?;
                if len > u32::MAX as usize
                    || len
                        .checked_add(5)
                        .is_none_or(|n| n > self.config.value.max_bytes)
                {
                    return Err(limit_error());
                }
                let mut result = String::with_capacity(len);
                result.push_str(old);
                result.push_str(appended);
                return Ok(Effect::put(
                    Value::UInt(len as u64),
                    key,
                    Value::String(result),
                    true,
                ));
            }
            _ => {
                return Err(error(
                    ErrorCode::UnknownCommand,
                    "Unknown or unimplemented command",
                ));
            }
        };
        Ok(Effect::put(next.clone(), key, next, true))
    }
}
