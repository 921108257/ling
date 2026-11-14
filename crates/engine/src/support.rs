//! 命令共享的参数检查、大小预检和待发布变更。
//! Shared argument checks, size preflights, and unpublished changes.

use linger_types::{DbError, ErrorCode, Scalar, Value, ValueLimits};
use std::{collections::BTreeMap, mem::size_of};

/// 一个尚未发布的 Key 变更；An unpublished change to one key.
pub(crate) struct Change {
    /// 变更目标，生成变更前已经校验。
    /// Target name, validated before constructing the change.
    pub key: String,
    /// Some 表示替换，None 表示删除。
    /// Some replaces the entry; None deletes it.
    pub value: Option<Value>,
    /// None 使用当前数据库；跨库操作显式指定目标。
    /// None uses the current database; cross-DB operations name their target.
    pub db: Option<u16>,
    /// 值替换或迁入时的身份与期限规则。
    /// Identity and deadline policy for replacement or relocation.
    pub policy: Mutation,
}

/// 迁入必须分配新身份，不能继承目标生命周期。
/// Relocations need a fresh identity rather than inheriting the destination lifecycle.
pub(crate) enum Mutation {
    /// 局部修改保留期限；Local mutation preserves deadline.
    Preserve,
    /// 整体替换清除期限；Whole replacement clears deadline.
    Replace,
    /// 设置期限但保留原生命周期；Sets a deadline while preserving the lifecycle.
    Deadline(Option<crate::Deadline>),
    /// 迁入保留源期限并新建生命周期；Relocation preserves source deadline with a fresh lifecycle.
    Recreate(Option<crate::Deadline>),
}

/// 响应和变更共享同一提交边界；Result and changes share one publication boundary.
pub(crate) struct Effect {
    /// 发布前必须能完整编码的成功结果。
    /// Successful result that must be fully encodable before publication.
    pub result: Value,
    /// 同一命令无重复 Key 的待提交变更。
    /// Pending changes with no duplicate key within one command.
    pub changes: Vec<Change>,
}

impl Effect {
    /// 构造只读结果；Constructs a read-only result.
    pub fn read(result: Value) -> Self {
        Self {
            result,
            changes: vec![],
        }
    }
    /// 暂存单 Key 写入，所有权仍在命令局部；Stages a single-key write owned by the command.
    pub fn put(result: Value, key: &str, value: Value, keep_deadline: bool) -> Self {
        Self {
            result,
            changes: vec![Change {
                key: key.into(),
                value: Some(value),
                db: None,
                policy: if keep_deadline {
                    Mutation::Preserve
                } else {
                    Mutation::Replace
                },
            }],
        }
    }
}

/// 保留稳定错误码；Preserves the stable error code.
pub(crate) fn error(code: ErrorCode, message: &'static str) -> DbError {
    DbError::new(code, message)
}
/// 参数数量、选项或签名不合法；Invalid argument count, option, or signature.
pub(crate) fn argument_error() -> DbError {
    error(ErrorCode::InvalidArgument, "Invalid command arguments")
}
/// 类型不匹配，不进行转换；Type mismatch without coercion.
pub(crate) fn type_error() -> DbError {
    error(ErrorCode::TypeMismatch, "Value type mismatch")
}
/// 容量不足在修改前明确拒绝；Capacity rejection occurs before mutation.
pub(crate) fn budget_error() -> DbError {
    error(ErrorCode::OutOfMemory, "Cache budget exceeded")
}
/// 编码大小或深度超限；Encoded size or depth limit exceeded.
pub(crate) fn limit_error() -> DbError {
    error(ErrorCode::LimitExceeded, "Value or response limit exceeded")
}

/// 读取文本参数；Reads a textual argument.
pub(crate) fn text(value: &Value) -> Result<&str, DbError> {
    if let Value::String(v) = value {
        Ok(v)
    } else {
        Err(argument_error())
    }
}
/// 读取有符号整数参数；Reads a signed integer argument.
pub(crate) fn integer(value: &Value) -> Result<i64, DbError> {
    if let Value::Int(v) = value {
        Ok(*v)
    } else {
        Err(argument_error())
    }
}
/// 读取无符号计数，禁止平台截断；Reads an unsigned count without platform truncation.
pub(crate) fn count(value: &Value) -> Result<u64, DbError> {
    if let Value::UInt(v) = value {
        Ok(*v)
    } else {
        Err(argument_error())
    }
}

/// 检查完整响应封套；响应容器不占业务嵌套层数。
/// Checks the complete response envelope; wrapper containers do not consume business depth.
pub(crate) fn response_size(
    value: &Value,
    cap: usize,
    limits: ValueLimits,
) -> Result<usize, DbError> {
    value
        .validate(ValueLimits {
            max_bytes: cap.checked_sub(1).ok_or_else(limit_error)?,
            max_depth: limits.max_depth + 4,
            max_key_bytes: usize::MAX,
        })?
        .checked_add(1)
        .ok_or_else(limit_error)
}

/// 构造有标签的逐项结果，Null 保持成功状态。
/// Builds a tagged item result, keeping Null successful.
pub(crate) fn item_result(result: Result<&Value, DbError>) -> Value {
    Value::Map(match result {
        Ok(value) => BTreeMap::from([
            ("ok".into(), Value::Bool(true)),
            ("value".into(), value.clone()),
        ]),
        Err(error) => BTreeMap::from([
            ("ok".into(), Value::Bool(false)),
            ("code".into(), Value::String(error.code)),
            ("message".into(), Value::String(error.message)),
        ]),
    })
}

/// 在克隆重复读取结果前检查整份响应与临时分配预算。
/// Checks the entire response and temporary allocation budget before cloning repeated reads.
pub(crate) fn item_results<'a>(
    items: impl Iterator<Item = Result<&'a Value, DbError>> + Clone,
    cap: usize,
    limits: ValueLimits,
) -> Result<Value, DbError> {
    let mut wire = 6usize;
    let mut memory = 0usize;
    for item in items.clone() {
        let (bytes, heap) = match item {
            Ok(value) => (
                24usize
                    .checked_add(value.validate(ValueLimits {
                        max_bytes: cap,
                        max_depth: limits.max_depth,
                        max_key_bytes: usize::MAX,
                    })?)
                    .ok_or_else(limit_error)?,
                heap_size(value)?,
            ),
            Err(error) => (
                45 + error.code.len() + error.message.len(),
                error.code.len() + error.message.len(),
            ),
        };
        wire = wire
            .checked_add(bytes)
            .filter(|n| *n <= cap)
            .ok_or_else(limit_error)?;
        memory = memory
            .checked_add(heap)
            .and_then(|n| n.checked_add(4096 + 2 * size_of::<Value>()))
            .filter(|n| *n <= 128 * 1024 * 1024)
            .ok_or_else(limit_error)?;
    }
    if wire > cap {
        return Err(limit_error());
    }
    Ok(Value::List(items.map(item_result).collect()))
}

/// 在复制子值前检查最终 List 的编码长度和完整深度。
/// Checks final List wire length and full depth before cloning children.
pub(crate) fn list<'a>(
    items: impl Iterator<Item = &'a Value> + Clone,
    limits: ValueLimits,
) -> Result<Value, DbError> {
    let mut bytes = 5usize;
    let mut count = 0u32;
    for item in items.clone() {
        count = count.checked_add(1).ok_or_else(limit_error)?;
        let len = item.validate(ValueLimits {
            max_depth: limits.max_depth.checked_sub(1).ok_or_else(limit_error)?,
            ..limits
        })?;
        bytes = bytes
            .checked_add(len)
            .filter(|n| *n <= limits.max_bytes)
            .ok_or_else(limit_error)?;
    }
    if bytes > limits.max_bytes {
        return Err(limit_error());
    }
    Ok(Value::List(items.cloned().collect()))
}

/// 用保守容器开销计入逻辑预算；不声称等于分配器或 RSS。
/// Includes conservative container overhead in the logical budget; not allocator or RSS accounting.
pub(crate) fn heap_size(value: &Value) -> Result<usize, DbError> {
    let add = |a: usize, b: usize| a.checked_add(b).ok_or_else(budget_error);
    match value {
        Value::String(v) => Ok(v.len()),
        Value::Bytes(v) => Ok(v.len()),
        Value::List(v) => v.iter().try_fold(
            v.len()
                .checked_mul(size_of::<Value>())
                .ok_or_else(budget_error)?,
            |n, item| add(n, heap_size(item)?),
        ),
        Value::Map(v) => v.iter().try_fold(
            v.len().checked_mul(1024).ok_or_else(budget_error)?,
            |n, (key, item)| add(add(n, key.len())?, heap_size(item)?),
        ),
        Value::Set(v) => v.iter().try_fold(
            v.len()
                .checked_mul(2 * size_of::<Scalar>() + 2)
                .and_then(|n| n.checked_add(if v.is_empty() { 0 } else { 256 }))
                .ok_or_else(budget_error)?,
            |n, item| {
                add(
                    n,
                    match item {
                        Scalar::String(v) => v.len(),
                        Scalar::Bytes(v) => v.len(),
                        _ => 0,
                    },
                )
            },
        ),
        _ => Ok(0),
    }
}
