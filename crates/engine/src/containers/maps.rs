//! Map 修改浅合并，请求中的重复字段在写入前拒绝。
//! Map mutations use shallow merging and reject duplicate request fields before writing.
use crate::{Transaction, support::*};
use ling_types::{DbError, ErrorCode, Value};
use std::{
    collections::{BTreeMap, HashSet},
    time::Instant,
};

/// 先测量借用字段的完整编码，再复制为持有所有权的 Map。
/// Measures borrowed fields before cloning into an owned Map.
fn map(fields: BTreeMap<&str, &Value>, limits: ling_types::ValueLimits) -> Result<Value, DbError> {
    let mut bytes = 5usize;
    u32::try_from(fields.len()).map_err(|_| limit_error())?;
    for (key, value) in &fields {
        let len = value.validate(ling_types::ValueLimits {
            max_depth: limits.max_depth - 1,
            ..limits
        })?;
        bytes = bytes
            .checked_add(5)
            .and_then(|n| n.checked_add(key.len()))
            .and_then(|n| n.checked_add(len))
            .filter(|n| *n <= limits.max_bytes)
            .ok_or_else(limit_error)?;
    }
    if bytes > limits.max_bytes {
        return Err(limit_error());
    }
    Ok(Value::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value.clone()))
            .collect(),
    ))
}

impl Transaction<'_> {
    /// 字段 Null 保持成功；缺失字段以逐项错误表达。
    /// Null fields remain successful; missing fields use per-item errors.
    pub(crate) fn map_command(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
        response_bytes: usize,
    ) -> Result<Effect, DbError> {
        let valid = match name {
            "FSET" => args.len() >= 3 && args.len() % 2 == 1,
            "FSETNX" => args.len() == 3,
            "FGET" | "FDEL" => args.len() >= 2,
            "FEXISTS" | "FMERGE" => args.len() == 2,
            _ => args.len() == 1,
        };
        if !valid {
            return Err(argument_error());
        }
        let key = self.key(&args[0])?;
        let Value::Map(fields) = &self.entry(db, key, now)?.value else {
            return Err(type_error());
        };
        let mut candidate: BTreeMap<&str, &Value> =
            fields.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let result = match name {
            "FLEN" => return Ok(Effect::read(Value::UInt(fields.len() as u64))),
            "FGETALL" => return Ok(Effect::read(Value::Map(fields.clone()))),
            "FKEYS" => {
                return Ok(Effect::read(Value::List(
                    fields.keys().cloned().map(Value::String).collect(),
                )));
            }
            "FVALS" => {
                return Ok(Effect::read(Value::List(
                    fields.values().cloned().collect(),
                )));
            }
            "FEXISTS" => {
                return Ok(Effect::read(Value::Bool(
                    fields.contains_key(self.key(&args[1])?),
                )));
            }
            "FGET" => {
                let names = args[1..]
                    .iter()
                    .map(|v| self.key(v))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Effect::read(item_results(
                    names.iter().map(|field| {
                        fields
                            .get(*field)
                            .ok_or_else(|| error(ErrorCode::FieldNotFound, "Map field not found"))
                    }),
                    response_bytes,
                    self.config.value,
                )?));
            }
            "FSET" | "FSETNX" => {
                let mut seen = HashSet::new();
                for pair in args[1..].chunks_exact(2) {
                    if !seen.insert(self.key(&pair[0])?) {
                        return Err(argument_error());
                    }
                }
                if name == "FSETNX" && fields.contains_key(text(&args[1])?) {
                    return Ok(Effect::read(Value::Bool(false)));
                }
                let mut added = 0;
                for pair in args[1..].chunks_exact(2) {
                    if candidate.insert(text(&pair[0])?, &pair[1]).is_none() {
                        added += 1;
                    }
                }
                if name == "FSETNX" {
                    Value::Bool(true)
                } else {
                    Value::UInt(added)
                }
            }
            "FDEL" => {
                let names = args[1..]
                    .iter()
                    .map(|v| self.key(v))
                    .collect::<Result<HashSet<_>, _>>()?;
                let mut removed = 0;
                for field in names {
                    if candidate.remove(field).is_some() {
                        removed += 1;
                    }
                }
                Value::UInt(removed)
            }
            "FMERGE" => {
                let Value::Map(merged) = &args[1] else {
                    return Err(argument_error());
                };
                let mut added = 0;
                for (field, value) in merged {
                    if candidate.insert(field, value).is_none() {
                        added += 1;
                    }
                }
                Value::UInt(added)
            }
            // 调度器和本函数入口已验证 Map 命令名，其他只读分支已提前返回。
            // The dispatcher and this function's entry validate Map command names; other read-only branches returned earlier.
            _ => unreachable!(),
        };
        Ok(Effect::put(
            result,
            key,
            map(candidate, self.config.value)?,
            true,
        ))
    }
}
