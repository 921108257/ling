//! 普通 Batch 的整批准入与共享响应预算；子项独立提交。
//! Whole-batch admission and shared response budgeting; items commit independently.

use crate::Config;
use linger_protocol::{Body, Frame, Request, encode_frame};
use linger_types::{DbError, ErrorCode, Value};

/// 严格白名单，不把未知命令或控制面命令交给引擎。
/// Strict allowlist excludes unknown and control-plane commands.
pub(crate) fn allowed(command: &str) -> bool {
    matches!(
        command,
        "SET"
            | "GET"
            | "GETSET"
            | "MGET"
            | "MSET"
            | "DEL"
            | "EXISTS"
            | "TYPE"
            | "RENAME"
            | "RENAMENX"
            | "COPY"
            | "MOVE"
            | "EXPIRE"
            | "PEXPIRE"
            | "EXPIREAT"
            | "PEXPIREAT"
            | "PERSIST"
            | "TTL"
            | "PTTL"
            | "EXPIRETIME"
            | "PEXPIRETIME"
            | "INCR"
            | "DECR"
            | "INCRBY"
            | "DECRBY"
            | "INCRFLOAT"
            | "APPEND"
            | "STRLEN"
            | "BLEN"
            | "LLEN"
            | "LINDEX"
            | "LPUSH"
            | "RPUSH"
            | "LPOP"
            | "RPOP"
            | "LRANGE"
            | "LSET"
            | "LTRIM"
            | "LREM"
            | "FSET"
            | "FSETNX"
            | "FGET"
            | "FGETALL"
            | "FDEL"
            | "FEXISTS"
            | "FKEYS"
            | "FVALS"
            | "FLEN"
            | "FMERGE"
            | "SADD"
            | "SREM"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SMEMBERS"
            | "SCARD"
            | "SPOP"
            | "SRANDMEMBER"
            | "SUNION"
            | "SINTER"
            | "SDIFF"
            | "SUNIONSTORE"
            | "SINTERSTORE"
            | "SDIFFSTORE"
    )
}
/// 预留每项最坏错误大小；各命令发布前检查自身剩余响应预算。
/// Reserves the largest error per item; each command checks its remaining response budget before publication.
pub(crate) fn execute(
    engine: &linger_engine::Engine,
    config: &Config,
    db: u16,
    items: Vec<Request>,
) -> Result<Vec<Result<Value, DbError>>, DbError> {
    if items.is_empty()
        || items.len() > config.protocol.max_batch_size
        || items.iter().any(|r| !allowed(&r.command))
    {
        return Err(DbError::new(
            ErrorCode::InvalidArgument,
            "Batch contains a disallowed command",
        ));
    }
    let error_bytes = 11
        + ErrorCode::ALL
            .iter()
            .map(|e| e.as_str().len())
            .max()
            .unwrap_or(0);
    let mut remaining = config
        .server
        .max_response_bytes
        .checked_sub(4)
        .filter(|n| {
            items
                .len()
                .checked_mul(error_bytes)
                .is_some_and(|size| size <= *n)
        })
        .ok_or_else(|| DbError::new(ErrorCode::LimitExceeded, "Batch response reserve exceeded"))?;
    let count = items.len();
    let mut results = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        let cap = remaining - (count - index - 1) * error_bytes;
        let mut result = if let Some(e) = item.value_error {
            Err(e)
        } else {
            engine.execute(db, &item.command, &item.arguments, cap)
        };
        // 空诊断仍保留稳定错误码，避免错误文本侵占后项预留。
        // Empty diagnostics retain stable codes without consuming later items' reserves.
        if let Err(error) = &mut result {
            error.message.clear();
        }
        let frame = Frame {
            request_id: 1,
            body: Body::Response(result.clone()),
        };
        let used = encode_frame(&frame, config.limits())?.len() - 18;
        if used > cap {
            return Err(DbError::new(
                ErrorCode::CommitUnknown,
                "Batch result exceeded reserved budget",
            ));
        }
        remaining -= used;
        results.push(result);
    }
    Ok(results)
}
