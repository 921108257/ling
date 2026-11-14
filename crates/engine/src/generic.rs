//! 通用 Key 命令；多 Key 结果在单所有者范围内整体发布。
//! Generic key commands; multi-key changes publish atomically within one owner.

use crate::{Transaction, support::*};
use linger_types::{DbError, ErrorCode, Value};
use std::{collections::HashSet, time::Instant};

impl Transaction<'_> {
    /// 分派通用命令；先校验所有名称和条件，再生成变更。
    /// Dispatches generic commands after validating every name and condition.
    pub(crate) fn generic(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
        response_bytes: usize,
    ) -> Result<Effect, DbError> {
        if name == "DBSIZE" {
            if !args.is_empty() {
                return Err(argument_error());
            }
            return Ok(Effect::read(Value::UInt(
                self.entries(db)
                    .filter(|(_, e)| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
                    .count() as u64,
            )));
        }
        if args.is_empty() {
            return Err(argument_error());
        }
        match name {
            "SET" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(argument_error());
                }
                let key = self.key(&args[0])?;
                let option = args
                    .get(2)
                    .map(text)
                    .transpose()?
                    .map(str::to_ascii_uppercase);
                if option.as_deref().is_some_and(|o| !matches!(o, "NX" | "XX")) {
                    return Err(argument_error());
                }
                let exists = self.entry(db, key, now).is_ok();
                if matches!(option.as_deref(), Some("NX")) && exists
                    || matches!(option.as_deref(), Some("XX")) && !exists
                {
                    return Ok(Effect::read(Value::Bool(false)));
                }
                Ok(Effect::put(Value::Bool(true), key, args[1].clone(), false))
            }
            "GET" | "GETSET" | "TYPE" => {
                if args.len() != if name == "GETSET" { 2 } else { 1 } {
                    return Err(argument_error());
                }
                let key = self.key(&args[0])?;
                let entry = self.entry(db, key, now)?;
                if name != "TYPE" {
                    response_size(&entry.value, response_bytes, self.config.value)?;
                }
                if name == "TYPE" {
                    Ok(Effect::read(Value::String(entry.value.type_name().into())))
                } else if name == "GET" {
                    Ok(Effect::read(entry.value.clone()))
                } else {
                    Ok(Effect::put(
                        entry.value.clone(),
                        key,
                        args[1].clone(),
                        false,
                    ))
                }
            }
            "MGET" | "EXISTS" | "DEL" => {
                let keys = args
                    .iter()
                    .map(|v| self.key(v))
                    .collect::<Result<Vec<_>, _>>()?;
                if name == "MGET" {
                    return Ok(Effect::read(item_results(
                        keys.iter()
                            .map(|key| self.entry(db, key, now).map(|e| &e.value)),
                        response_bytes,
                        self.config.value,
                    )?));
                }
                let keys: HashSet<_> = keys.into_iter().collect();
                let active: Vec<_> = keys
                    .into_iter()
                    .filter(|key| self.entry(db, key, now).is_ok())
                    .collect();
                Ok(Effect {
                    result: Value::UInt(active.len() as u64),
                    changes: if name == "DEL" {
                        active
                            .into_iter()
                            .map(|key| Change {
                                key: key.into(),
                                value: None,
                                keep_deadline: false,
                            })
                            .collect()
                    } else {
                        vec![]
                    },
                })
            }
            "MSET" => {
                if args.len() % 2 != 0 {
                    return Err(argument_error());
                }
                let mut seen = HashSet::new();
                for pair in args.chunks_exact(2) {
                    if !seen.insert(self.key(&pair[0])?) {
                        return Err(argument_error());
                    }
                }
                Ok(Effect {
                    result: Value::Bool(true),
                    changes: args
                        .chunks_exact(2)
                        .map(|pair| Change {
                            key: text(&pair[0]).unwrap().into(),
                            value: Some(pair[1].clone()),
                            keep_deadline: false,
                        })
                        .collect(),
                })
            }
            _ => Err(error(
                ErrorCode::UnknownCommand,
                "Unknown or unimplemented command",
            )),
        }
    }

    /// 校验 Key 的文本类型、非空性和字节上限。
    /// Validates key text type, non-emptiness, and byte limit.
    pub(crate) fn key<'a>(&self, value: &'a Value) -> Result<&'a str, DbError> {
        let key = text(value)?;
        self.config.value.validate_key(key)?;
        Ok(key)
    }
}
