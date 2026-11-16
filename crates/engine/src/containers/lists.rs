//! List 负索引与闭区间在宽整数中换算。
//! List negative indices and inclusive ranges use wide arithmetic.
use crate::{Transaction, support::*};
use ling_types::{DbError, ErrorCode, Value};
use std::{collections::HashSet, ops::Range, time::Instant};

/// 转换已有位置；Converts an existing position.
fn index(index: i64, len: usize) -> Result<usize, DbError> {
    let pos = if index < 0 {
        len as i128 + i128::from(index)
    } else {
        i128::from(index)
    };
    if pos < 0 || pos >= len as i128 {
        return Err(error(ErrorCode::IndexOutOfRange, "List index out of range"));
    }
    Ok(pos as usize)
}

/// 裁剪为半开区间，空区间始终合法；Clips to a half-open range, allowing empty ranges.
fn range(start: i64, end: i64, len: usize) -> Range<usize> {
    let normalize = |n: i64| {
        if n < 0 {
            len as i128 + i128::from(n)
        } else {
            i128::from(n)
        }
    };
    let start = normalize(start).max(0);
    let end = normalize(end).min(len as i128 - 1);
    if start > end || start >= len as i128 || end < 0 {
        0..0
    } else {
        start as usize..end as usize + 1
    }
}

impl Transaction<'_> {
    /// 候选列表先校验父层深度；修改为空时仍保留顶级 Key。
    /// Checks parent depth before building a candidate; empty results keep the top-level key.
    pub(crate) fn list_command(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
    ) -> Result<Effect, DbError> {
        let valid = match name {
            "LPUSH" | "RPUSH" => args.len() >= 2,
            "LPOP" | "RPOP" => matches!(args.len(), 1 | 2),
            "LRANGE" | "LTRIM" | "LSET" | "LREM" => args.len() == 3,
            "LINDEX" => args.len() == 2,
            _ => args.len() == 1,
        };
        if !valid {
            return Err(argument_error());
        }
        let key = self.key(&args[0])?;
        let Value::List(items) = &self.entry(db, key, now)?.value else {
            return Err(type_error());
        };
        let limits = self.config.value;
        let (result, next) = match name {
            "LLEN" => return Ok(Effect::read(Value::UInt(items.len() as u64))),
            "LINDEX" => {
                return Ok(Effect::read(
                    items[index(integer(&args[1])?, items.len())?].clone(),
                ));
            }
            "LPUSH" | "RPUSH" => {
                let len = items
                    .len()
                    .checked_add(args.len() - 1)
                    .ok_or_else(limit_error)?;
                let next = if name == "LPUSH" {
                    list(args[1..].iter().rev().chain(items.iter()), limits)?
                } else {
                    list(items.iter().chain(args[1..].iter()), limits)?
                };
                (Value::UInt(len as u64), next)
            }
            "LPOP" | "RPOP" => {
                let requested = args.get(1).map(count).transpose()?.unwrap_or(1);
                let count = requested.min(items.len() as u64) as usize;
                if name == "LPOP" {
                    (
                        list(items[..count].iter(), limits)?,
                        list(items[count..].iter(), limits)?,
                    )
                } else {
                    (
                        list(items[items.len() - count..].iter().rev(), limits)?,
                        list(items[..items.len() - count].iter(), limits)?,
                    )
                }
            }
            "LRANGE" | "LTRIM" => {
                let selected = range(integer(&args[1])?, integer(&args[2])?, items.len());
                let next = list(items[selected].iter(), limits)?;
                if name == "LRANGE" {
                    return Ok(Effect::read(next));
                }
                (Value::Bool(true), next)
            }
            "LSET" => {
                let pos = index(integer(&args[1])?, items.len())?;
                (
                    Value::Bool(true),
                    list(
                        items
                            .iter()
                            .enumerate()
                            .map(|(i, item)| if i == pos { &args[2] } else { item }),
                        limits,
                    )?,
                )
            }
            "LREM" => {
                let amount = integer(&args[1])?;
                let max = if amount == 0 {
                    usize::MAX
                } else {
                    amount.unsigned_abs().min(items.len() as u64) as usize
                };
                let matching = items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| (item == &args[2]).then_some(i));
                let removed: HashSet<_> = if amount < 0 {
                    matching.rev().take(max).collect()
                } else {
                    matching.take(max).collect()
                };
                (
                    Value::UInt(removed.len() as u64),
                    list(
                        items
                            .iter()
                            .enumerate()
                            .filter_map(|(i, item)| (!removed.contains(&i)).then_some(item)),
                        limits,
                    )?,
                )
            }
            _ => unreachable!(),
        };
        Ok(Effect::put(result, key, next, true))
    }
}
