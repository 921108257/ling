//! 毫秒期限、TTL 查询和原子期限修改；逻辑到期独立于后台回收。
//! Millisecond deadlines, TTL queries, and atomic deadline changes; logical expiry is independent of reclamation.

use crate::{Deadline, TimeSnapshot, Transaction, support::*, unavailable};
use linger_types::{DbError, ErrorCode, Value};
use std::time::Duration;

/// 完整检查秒换算、Unix 范围及平台单调期限，禁止溢出变为永久有效。
/// Checks seconds, Unix range, and platform monotonic limits so overflow never makes a key persistent.
pub(crate) fn expiration(
    option: &str,
    value: &Value,
    time: TimeSnapshot,
    strict: bool,
) -> Result<Deadline, DbError> {
    let value = integer(value)?;
    let millis = if matches!(option, "EX" | "EXAT" | "EXPIRE" | "EXPIREAT") {
        value.checked_mul(1000).ok_or_else(time_overflow)?
    } else {
        value
    };
    let absolute = matches!(option, "EXAT" | "PXAT" | "EXPIREAT" | "PEXPIREAT");
    let delta = if absolute {
        i64::try_from(i128::from(millis) - i128::from(time.unix_ms)).map_err(|_| time_overflow())?
    } else {
        millis
    };
    if strict && delta <= 0 {
        return Err(argument_error());
    }
    let unix_ms = if absolute {
        millis
    } else {
        time.unix_ms.checked_add(delta).ok_or_else(time_overflow)?
    };
    let monotonic = time
        .monotonic
        .checked_add(Duration::from_millis(delta.max(0) as u64))
        .ok_or_else(time_overflow)?;
    Ok(Deadline { monotonic, unix_ms })
}

/// 时间运算使用既有数值溢出错误，不伪装为缺失或无期限。
/// Time arithmetic uses the existing overflow code rather than pretending the key is missing or persistent.
fn time_overflow() -> DbError {
    error(ErrorCode::Overflow, "TTL exceeds representable time range")
}

/// SET 选项无论条件是否满足均先验证；每组最多一个，顺序任意。
/// SET validates all options before evaluating conditions; each group appears once, in either order.
pub(crate) fn set_options(
    args: &[Value],
    time: TimeSnapshot,
) -> Result<(Option<bool>, Option<Deadline>), DbError> {
    if args.len() < 2 {
        return Err(argument_error());
    }
    let (mut condition, mut deadline, mut pos) = (None, None, 2);
    while pos < args.len() {
        let option = text(&args[pos])?.to_ascii_uppercase();
        match option.as_str() {
            "NX" | "XX" if condition.is_none() => {
                condition = Some(option == "XX");
                pos += 1;
            }
            "EX" | "PX" | "EXAT" | "PXAT" if deadline.is_none() && pos + 1 < args.len() => {
                deadline = Some(expiration(&option, &args[pos + 1], time, true)?);
                pos += 2;
            }
            _ => return Err(argument_error()),
        }
    }
    Ok((condition, deadline))
}

impl Transaction<'_> {
    /// 修改仅涉及期限与版本，避免为 PERSIST / EXPIRE 复制整个 Value。
    /// Changes only deadlines and revisions without copying entire Values for PERSIST or EXPIRE.
    pub(crate) fn ttl(
        &mut self,
        db: u16,
        name: &str,
        args: &[Value],
        time: TimeSnapshot,
        cap: usize,
    ) -> Result<Value, DbError> {
        let query = matches!(name, "TTL" | "PTTL" | "EXPIRETIME" | "PEXPIRETIME");
        if args.len() != if query || name == "PERSIST" { 1 } else { 2 } {
            return Err(argument_error());
        }
        let key = self.key(&args[0])?;
        let deadline = if query || name == "PERSIST" {
            None
        } else {
            Some(expiration(name, &args[1], time, false)?)
        };
        let active = self.entry(db, key, time.monotonic).ok();
        if query {
            let value = match active.map(|e| e.metadata.deadline) {
                None => -2,
                Some(None) => -1,
                Some(Some(at)) => match name {
                    "PEXPIRETIME" => at.unix_ms,
                    "EXPIRETIME" => at.unix_ms.div_euclid(1000),
                    _ => {
                        let ms =
                            i64::try_from(at.monotonic.duration_since(time.monotonic).as_millis())
                                .map_err(|_| time_overflow())?;
                        if name == "TTL" { ms / 1000 } else { ms }
                    }
                },
            };
            let result = Value::Int(value);
            response_size(&result, cap, self.config.value)?;
            return Ok(result);
        }
        let result =
            Value::Bool(active.is_some_and(|e| name != "PERSIST" || e.metadata.deadline.is_some()));
        response_size(&result, cap, self.config.value)?;
        let Some(entry) = active else {
            return Ok(result);
        };
        if entry.metadata.deadline == deadline {
            return Ok(result);
        }
        let immediate = deadline.is_some_and(|d| d.monotonic <= time.monotonic);
        let mut metadata = entry.metadata;
        let old = metadata;
        if !immediate {
            metadata.revision = metadata
                .revision
                .checked_add(1)
                .ok_or_else(|| error(ErrorCode::Overflow, "Revision exhausted"))?;
            metadata.deadline = deadline;
        }
        let bytes = entry.bytes;
        let mut budget = self.engine.budget.lock().map_err(|_| unavailable())?;
        let id = self.engine.location(db, key)?;
        let pos = self
            .guards
            .binary_search_by_key(&id, |(id, _)| *id)
            .expect("TTL shard locked");
        let events = [self.event(
            db,
            key,
            if immediate { old } else { metadata },
            if immediate {
                linger_types::EventKind::Expired
            } else {
                linger_types::EventKind::TtlChanged
            },
            time,
        )];
        let store = self.engine.events.as_ref();
        let publish = || {
            budget.schedule(id, key, Some(old), (!immediate).then_some(metadata));
            if immediate {
                self.guards[pos].1.remove(key);
                budget.memory -= bytes;
            } else {
                self.guards[pos].1.get_mut(key).unwrap().metadata = metadata;
            }
            result
        };
        match store {
            Some(s) => s.commit(&events, publish),
            None => Ok(publish()),
        }
    }
}
