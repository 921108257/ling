//! 共享数据契约；不依赖网络、客户端或存储。
//! Shared data contracts, independent of networking, clients, and storage.

mod error;
mod event;
pub use event::{Event, EventKind};
mod metrics;
mod pattern;
mod value;

pub use error::{DbError, ErrorCode};
pub use metrics::{
    HistogramSnapshot, JOURNAL_TRANSACTION_BUCKET_BOUNDS_BYTES, LATENCY_BUCKET_BOUNDS_US,
};
pub use pattern::key_pattern_matches;
pub use value::{Float, Scalar, Value, ValueLimits};

/// 标记命令签名中的业务值位置，名称和选项另有限制。
/// Identifies business-value arguments; names and options use separate limits.
pub fn is_value_argument(command: &str, index: usize) -> bool {
    match command {
        "SET" | "GETSET" | "FMERGE" | "INCRFLOAT" => index == 1,
        "MSET" => index % 2 == 1,
        "LPUSH" | "RPUSH" | "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" => index >= 1,
        "LSET" | "LREM" => index == 2,
        "FSET" | "FSETNX" => index >= 2 && index.is_multiple_of(2),
        _ => false,
    }
}

/// 条目生命周期身份；删除重建后不得复用。
/// Entry lifecycle identity; never reused after deletion and recreation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation {
    /// 进程启动时生成的随机代号。
    /// Random identifier generated at process startup.
    pub epoch: u128,
    /// 同一进程内单调递增的创建序号。
    /// Monotonically increasing creation number within one process.
    pub counter: u64,
}

/// 可靠流事件身份；保留各字段精度，不经过浮点或 JSON。
/// Event identity in a reliable stream; preserves field precision without float or JSON conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    /// 持久事件流身份。
    /// Persistent event-stream identity.
    pub stream_id: u128,
    /// 所属数据库编号。
    /// Owning database number.
    pub db: u16,
    /// 所属分片编号。
    /// Owning shard number.
    pub shard: u32,
    /// 流内递增序号，重投不改变。
    /// Increasing stream sequence, unchanged on redelivery.
    pub sequence: u64,
}
