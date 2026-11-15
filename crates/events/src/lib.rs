//! 有界事件流、消费组和同步 Journal；不恢复缓存值。
//! Bounded event streams, consumer groups, and synchronous Journal; cached values are never restored.

mod codec;
mod journal;
mod store;
use linger_types::{DbError, ErrorCode, EventKind, Generation};
use std::path::PathBuf;
pub use store::{EventStore, GroupStats, Stats, Subscription};

/// 启动固定的事件策略；Reliable 不允许静默降级。
/// Fixed startup event policy; Reliable never silently falls back.
#[derive(Clone, Debug)]
pub struct Config {
    /// 是否生成事件；Whether to generate events.
    pub enabled: bool,
    /// 是否逐事务同步 Journal；Whether to synchronize a Journal per transaction.
    pub reliable: bool,
    /// Reliable 独占目录，Fast 必须为空；Exclusive Reliable directory, empty for Fast.
    pub directory: PathBuf,
    /// 物理目录总容量；Total physical directory budget.
    pub max_bytes: usize,
    /// 内存事件及组元数据预算；Memory budget for events and group metadata.
    pub buffer_bytes: usize,
    /// 每组总交付窗口；Total delivery window per group.
    pub max_pending: usize,
    /// 命名组及临时会话合计上限；Combined named-group and temporary-session limit.
    pub max_groups: usize,
    /// 租约毫秒；Lease milliseconds.
    pub consumer_timeout_ms: u64,
    /// 重投最小间隔毫秒；Minimum redelivery interval in milliseconds.
    pub retry_interval_ms: u64,
    /// 保留毫秒数；Retention milliseconds.
    pub retention_ms: u64,
}
impl Default for Config {
    /// 默认有界 Fast；Defaults to bounded Fast mode.
    fn default() -> Self {
        Self {
            enabled: true,
            reliable: false,
            directory: PathBuf::new(),
            max_bytes: 1024 * 1024 * 1024,
            buffer_bytes: 64 * 1024 * 1024,
            max_pending: 10000,
            max_groups: 1024,
            consumer_timeout_ms: 30000,
            retry_interval_ms: 5000,
            retention_ms: 7 * 86400 * 1000,
        }
    }
}
impl Config {
    /// 分配或打开文件前验证组合与预算；Validates combinations and budgets before allocation or opening files.
    pub fn validate(&self) -> Result<(), DbError> {
        if self.buffer_bytes < 4096
            || self.buffer_bytes > 1024 * 1024 * 1024
            || self.max_bytes > usize::MAX / 4
            || self.max_pending > self.buffer_bytes / 256
            || self.max_groups > self.buffer_bytes / 512
            || self.max_pending == 0
            || self.max_groups == 0
            || self.consumer_timeout_ms == 0
            || self.retry_interval_ms == 0
            || self.retention_ms == 0
            || (self.reliable && !self.enabled)
            || (self.reliable
                && (self.directory.as_os_str().is_empty() || self.max_bytes < 1024 * 1024))
            || (!self.reliable && !self.directory.as_os_str().is_empty())
        {
            return Err(err(ErrorCode::ConfigInvalid, "Invalid event configuration"));
        }
        Ok(())
    }
}
/// 发布前生成的事件，序号只在完整提交时分配。
/// Event prepared before publication; sequences are assigned only by a complete commit.
#[derive(Clone, Debug)]
pub struct Change {
    /// 数据库；Database.
    pub db: u16,
    /// 分片；Shard.
    pub shard: u32,
    /// 类型；Kind.
    pub kind: EventKind,
    /// Key；Key.
    pub key: String,
    /// 生命周期；Lifecycle.
    pub generation: Generation,
    /// 最终修订；Final revision.
    pub revision: u64,
    /// 记账时间；Accounting timestamp.
    pub timestamp_ms: u64,
}
/// 稳定且无业务内容的错误；Stable error without business contents.
fn err(code: ErrorCode, message: &'static str) -> DbError {
    DbError::new(code, message)
}

#[cfg(test)]
mod tests;
