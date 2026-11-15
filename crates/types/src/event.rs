//! 生命周期事件的稳定字段与 Value 封套；不携带缓存值。
//! Stable lifecycle event fields and Value envelopes, without cached values.

use crate::{DbError, ErrorCode, EventId, Generation, Value};
use std::collections::BTreeMap;

/// 冻结的事件类型编号，与磁盘格式 v1 一致。
/// Frozen event kind numbers shared with disk format v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    /// 新生命周期；New lifecycle.
    Created = 1,
    /// 值实际变化；Value changed.
    Updated = 2,
    /// 期限终结；Deadline ended the lifecycle.
    Expired = 3,
    /// 主动移除；Explicit removal.
    Deleted = 4,
    /// 容量淘汰；Capacity eviction.
    Evicted = 5,
    /// 期限变化；Deadline changed.
    TtlChanged = 6,
}
impl EventKind {
    /// 返回协议名称；Returns the wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "KEY_CREATED",
            Self::Updated => "KEY_UPDATED",
            Self::Expired => "KEY_EXPIRED",
            Self::Deleted => "KEY_DELETED",
            Self::Evicted => "KEY_EVICTED",
            Self::TtlChanged => "KEY_TTL_CHANGED",
        }
    }
    /// 拒绝未知磁盘编号；Rejects unknown disk tags.
    pub fn from_tag(tag: u8) -> Result<Self, DbError> {
        match tag {
            1 => Ok(Self::Created),
            2 => Ok(Self::Updated),
            3 => Ok(Self::Expired),
            4 => Ok(Self::Deleted),
            5 => Ok(Self::Evicted),
            6 => Ok(Self::TtlChanged),
            _ => Err(invalid()),
        }
    }
}

/// 已提交的变更事实，重投不改变身份，不包含新旧 Value。
/// A committed change fact whose identity survives redelivery, without old or new Values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// 流内唯一身份；Unique stream identity.
    pub id: EventId,
    /// 变更原因；Change reason.
    pub kind: EventKind,
    /// 原始 UTF-8 Key；Original UTF-8 key.
    pub key: String,
    /// 生命周期身份；Lifecycle identity.
    pub generation: Generation,
    /// 状态最终修订；Final state revision.
    pub revision: u64,
    /// 记账 Unix 毫秒；Accounting Unix milliseconds.
    pub timestamp_ms: u64,
}
impl Event {
    /// 构造已冻结的 EVENT Map，身份使用 16 字节大端而非浮点数。
    /// Builds the frozen EVENT Map with 16-byte big-endian identities rather than floating point.
    pub fn to_value(&self) -> Value {
        Value::Map(BTreeMap::from([
            ("event_id".into(), self.id.to_value()),
            (
                "event_type".into(),
                Value::String(self.kind.as_str().into()),
            ),
            ("key".into(), Value::String(self.key.clone())),
            (
                "generation".into(),
                Value::Map(BTreeMap::from([
                    (
                        "epoch".into(),
                        Value::Bytes(self.generation.epoch.to_be_bytes().to_vec()),
                    ),
                    ("counter".into(), Value::UInt(self.generation.counter)),
                ])),
            ),
            ("revision".into(), Value::UInt(self.revision)),
            ("timestamp_ms".into(), Value::UInt(self.timestamp_ms)),
        ]))
    }
}
impl EventId {
    /// ACK 与 EVENT 共用的无损复合身份；Lossless compound identity shared by ACK and EVENT.
    pub fn to_value(self) -> Value {
        Value::Map(BTreeMap::from([
            (
                "stream_id".into(),
                Value::Bytes(self.stream_id.to_be_bytes().to_vec()),
            ),
            ("db".into(), Value::UInt(self.db.into())),
            ("shard".into(), Value::UInt(self.shard.into())),
            ("sequence".into(), Value::UInt(self.sequence)),
        ]))
    }
    /// 完整验证 ACK 字段，不将字符串或宽整数隐式转换。
    /// Fully validates ACK fields without coercing strings or oversized integers.
    pub fn from_value(value: &Value) -> Result<Self, DbError> {
        let Value::Map(map) = value else {
            return Err(invalid());
        };
        if map.len() != 4 {
            return Err(invalid());
        }
        let Some(Value::Bytes(stream)) = map.get("stream_id") else {
            return Err(invalid());
        };
        let number = |name| match map.get(name) {
            Some(Value::UInt(n)) => Ok(*n),
            _ => Err(invalid()),
        };
        let id = Self {
            stream_id: u128::from_be_bytes(stream.as_slice().try_into().map_err(|_| invalid())?),
            db: number("db")?.try_into().map_err(|_| invalid())?,
            shard: number("shard")?.try_into().map_err(|_| invalid())?,
            sequence: number("sequence")?,
        };
        if id.sequence == 0 {
            return Err(invalid());
        }
        Ok(id)
    }
}
/// 使用稳定错误，诊断不含输入；Stable error with no input in diagnostics.
fn invalid() -> DbError {
    DbError::new(ErrorCode::InvalidArgument, "Invalid event identity or kind")
}
