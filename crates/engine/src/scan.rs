//! 字节通配符、全库查询与有界游标；游标进度不依赖可变数组下标。
//! Byte globs, database queries, and bounded cursors; progress never depends on mutable array offsets.

use crate::{Engine, Transaction, support::*};
use ling_types::{DbError, ErrorCode, Value, key_pattern_matches};
use rand::seq::IteratorRandom;
use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
    time::Instant,
};

/// 仅一个连接持有的游标进度；不能克隆到其他会话。
/// Cursor progress owned by exactly one connection; cannot be cloned into another session.
#[derive(Default)]
pub struct ScanState {
    /// 不透明当前票号；Current opaque ticket.
    ticket: u64,
    /// 游标绑定的数据库；Database bound to this cursor.
    db: u16,
    /// 规范化筛选条件；Canonical filtering options.
    options: ScanOptions,
    /// 当前分片与其排他性下界；Current shard and its exclusive lower bound.
    position: Position,
}

/// 固定筛选决定游标身份，COUNT 只决定单页工作量。
/// Fixed filters define cursor identity; COUNT only changes page work.
#[derive(Default, PartialEq, Eq)]
struct ScanOptions {
    /// 字节通配符，None 等价匹配全部；Byte glob, None matches everything.
    pattern: Option<String>,
    /// 严格小写类型名；Strict lowercase type name.
    kind: Option<String>,
}

/// 逐分片单调前进，不保留全库快照。
/// Advances monotonically through shards without retaining a database snapshot.
#[derive(Default, Clone)]
struct Position {
    /// 当前分片序号；Current shard index.
    shard: u32,
    /// 分片内最后处理的 Key；Last processed key within the shard.
    last: Option<String>,
}

/// 校验末尾转义，不引入正则表达式语义。
/// Rejects dangling escapes without introducing regex semantics.
pub(crate) fn validate_pattern(pattern: &str) -> Result<(), DbError> {
    let mut escaped = false;
    for b in pattern.bytes() {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        }
    }
    if escaped {
        Err(argument_error())
    } else {
        Ok(())
    }
}

/// 解析筛选参数；重复项、未知类型和零 COUNT 均明确拒绝。
/// Parses filters; duplicate options, unknown types, and zero COUNT are rejected.
fn options(args: &[Value]) -> Result<(u64, ScanOptions, usize), DbError> {
    if args.is_empty() || args.len() % 2 != 1 {
        return Err(argument_error());
    }
    let cursor = count(&args[0])?;
    let mut result = ScanOptions::default();
    let mut work = None;
    for pair in args[1..].chunks_exact(2) {
        match text(&pair[0])?.to_ascii_uppercase().as_str() {
            "MATCH" if result.pattern.is_none() => {
                let pattern = text(&pair[1])?;
                validate_pattern(pattern)?;
                result.pattern = Some(pattern.into());
            }
            "TYPE" if result.kind.is_none() => {
                let kind = text(&pair[1])?.to_ascii_lowercase();
                if !matches!(
                    kind.as_str(),
                    "null"
                        | "bool"
                        | "int"
                        | "uint"
                        | "float"
                        | "string"
                        | "bytes"
                        | "list"
                        | "map"
                        | "set"
                ) {
                    return Err(argument_error());
                }
                result.kind = Some(kind);
            }
            "COUNT" if work.is_none() => {
                let n = count(&pair[1])?;
                if n == 0 {
                    return Err(argument_error());
                }
                work = Some(n.min(10_000) as usize);
            }
            _ => return Err(argument_error()),
        }
    }
    Ok((cursor, result, work.unwrap_or(100)))
}

impl Engine {
    /// 查询一页游标；成功编码前不移动进度，失效游标不退回首尾。
    /// Reads one cursor page; progress moves only after a valid response, and stale cursors never reset silently.
    pub fn scan(
        &self,
        db: u16,
        args: &[Value],
        state: &mut ScanState,
        response_bytes: usize,
    ) -> Result<Value, DbError> {
        self.validate_db(db)?;
        let (cursor, filters, work) = options(args)?;
        if cursor != 0 && (cursor != state.ticket || db != state.db || filters != state.options) {
            return Err(error(
                ErrorCode::InvalidCursor,
                "Cursor does not match this connection, database or filter",
            ));
        }
        let mut position = if cursor == 0 {
            Position::default()
        } else {
            state.position.clone()
        };
        let tx = self.lock(self.db_locations(db).collect())?;
        let now = self.clock.now()?.monotonic;
        let mut keys = Vec::new();
        // 包含 status、Map、字段名、游标和 List 封套。 Includes status, Map, names, cursor, and List.
        let mut bytes = 40usize;
        let mut processed = 0;
        while position.shard < self.config.shard_count && processed < work {
            let id = db as usize * self.config.shard_count as usize + position.shard as usize;
            let index = &tx.guards[tx.guards.binary_search_by_key(&id, |(id, _)| *id).unwrap()].1;
            let next = match &position.last {
                Some(key) => index
                    .range::<str, _>((Excluded(key.as_str()), Unbounded))
                    .next(),
                None => index.first_key_value(),
            };
            let Some((key, entry)) = next else {
                position.shard += 1;
                position.last = None;
                processed += 1;
                continue;
            };
            if entry.metadata.deadline.is_none_or(|d| now < d.monotonic)
                && filters
                    .kind
                    .as_deref()
                    .is_none_or(|kind| entry.value.type_name() == kind)
                && filters
                    .pattern
                    .as_deref()
                    .is_none_or(|pattern| key_pattern_matches(pattern, key))
            {
                let next_bytes = bytes.checked_add(5 + key.len()).ok_or_else(limit_error)?;
                if next_bytes > response_bytes || next_bytes > 64 * 1024 * 1024 {
                    if keys.is_empty() {
                        return Err(limit_error());
                    }
                    break;
                }
                bytes = next_bytes;
                keys.push(Value::String(key.clone()));
            }
            position.last = Some(key.clone());
            processed += 1;
        }
        let ticket = if position.shard == self.config.shard_count {
            0
        } else {
            // 随机票号跨会话难以误用，仍以实际状态严格匹配。
            // Random tickets prevent accidental cross-session reuse and are strictly matched to state.
            let mut ticket = rand::random::<u64>();
            while ticket == 0 || ticket == state.ticket {
                ticket = rand::random();
            }
            ticket
        };
        let result = Value::Map(BTreeMap::from([
            ("cursor".into(), Value::UInt(ticket)),
            ("keys".into(), Value::List(keys)),
        ]));
        response_size(&result, response_bytes, self.config.value)?;
        *state = ScanState {
            ticket,
            db,
            options: filters,
            position,
        };
        Ok(result)
    }
}

impl Transaction<'_> {
    /// KEYS 一次观察全部分片，RANDOMKEY 用蓄水池采样而不复制全库。
    /// KEYS observes all shards once; RANDOMKEY uses reservoir sampling without copying the database.
    pub(crate) fn query(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
        cap: usize,
    ) -> Result<Effect, DbError> {
        if name == "RANDOMKEY" {
            if !args.is_empty() {
                return Err(argument_error());
            }
            let key = self
                .entries(db)
                .filter(|(_, e)| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
                .choose(&mut rand::rng())
                .map(|(key, _)| key)
                .ok_or_else(|| error(ErrorCode::KeyNotFound, "Database is empty"))?;
            // 在复制 Key 前校验响应，避免失败查询产生不必要的大分配。
            // Check the response before copying the key to avoid large allocations on rejected queries.
            if key.len().checked_add(6).is_none_or(|n| n > cap) {
                return Err(limit_error());
            }
            let value = Value::String(key.clone());
            response_size(&value, cap, self.config.value)?;
            return Ok(Effect::read(value));
        }
        let [pattern] = args else {
            return Err(argument_error());
        };
        let pattern = text(pattern)?;
        validate_pattern(pattern)?;
        let mut bytes = 6usize;
        let mut memory = 0usize;
        let mut keys = Vec::new();
        for (key, entry) in self.entries(db) {
            if entry.metadata.deadline.is_none_or(|d| now < d.monotonic)
                && key_pattern_matches(pattern, key)
            {
                bytes = bytes
                    .checked_add(5 + key.len())
                    .filter(|n| *n <= cap && *n <= 64 * 1024 * 1024)
                    .ok_or_else(limit_error)?;
                // 为 Vec 扩容预留最多两倍元素空间，与字符串分配一起限制。
                // Allow up to twice the element storage for Vec growth, alongside string allocations.
                memory = memory
                    .checked_add(key.len())
                    .and_then(|n| n.checked_add(2 * std::mem::size_of::<Value>()))
                    .filter(|n| *n <= 128 * 1024 * 1024)
                    .ok_or_else(limit_error)?;
                keys.push(Value::String(key.clone()));
            }
        }
        Ok(Effect::read(Value::List(keys)))
    }
}
