//! Shared data contracts; independent of transport and storage.

mod error;
mod value;

pub use error::{DbError, ErrorCode};
pub use value::{Float, Scalar, Value, ValueLimits};

/// 标记命令签名中的业务值位置，名称和选项另有限制。
/// Identifies business-value arguments; names and options use separate limits.
pub fn is_value_argument(command: &str, index: usize) -> bool {
    match command {
        "SET" | "GETSET" | "FMERGE" | "INCRFLOAT" => index == 1,
        "MSET" => index % 2 == 1,
        "LPUSH" | "RPUSH" | "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" => index >= 1,
        "LSET" | "LREM" => index == 2,
        "FSET" | "FSETNX" => index >= 2 && index % 2 == 0,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation {
    pub epoch: u128,
    pub counter: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    pub stream_id: u128,
    pub db: u16,
    pub shard: u32,
    pub sequence: u64,
}
