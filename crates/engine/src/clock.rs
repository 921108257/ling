//! 一次命令共享的双时钟观测；测试可独立推进单调时间与墙钟。
//! Dual-clock observation shared by one command; tests can advance monotonic and wall time independently.

use crate::support::error;
use ling_types::{DbError, ErrorCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 取得全部相关分片执行权后的时间观测。
/// Time observation taken after acquiring all relevant shard ownership.
#[derive(Clone, Copy, Debug)]
pub struct TimeSnapshot {
    /// 逻辑过期与内部热度使用的单调时间。
    /// Monotonic time for logical expiry and internal access heat.
    pub monotonic: Instant,
    /// 本次观测的有符号 Unix 毫秒，用于接受绝对期限。
    /// Observed signed Unix milliseconds used to accept absolute deadlines.
    pub unix_ms: i64,
}

/// 引擎时钟边界；实现应保证单调时间不回退，失败时不得发布修改。
/// Engine clock boundary; implementations must not move monotonic time backward, and failures publish no changes.
pub trait Clock: Send + Sync {
    /// 一次取得配对观测；墙钟范围无法表示时返回稳定错误。
    /// Takes a paired observation, returning a stable error if wall time is unrepresentable.
    fn now(&self) -> Result<TimeSnapshot, DbError>;
}

/// 生产环境的平台时钟，不依赖客户端或 Tokio 调度进度。
/// Platform clock for production, independent of client clocks and Tokio scheduling progress.
pub struct SystemClock;

impl Clock for SystemClock {
    /// 保留纪元前时间的符号，转换前使用宽整数避免回绕。
    /// Preserves pre-epoch signs using wide integers before conversion to prevent wrapping.
    fn now(&self) -> Result<TimeSnapshot, DbError> {
        let monotonic = Instant::now();
        let unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis() as i128,
            Err(error) => -(error.duration().as_millis() as i128),
        };
        Ok(TimeSnapshot {
            monotonic,
            unix_ms: i64::try_from(unix_ms).map_err(|_| {
                error(
                    ErrorCode::Overflow,
                    "Wall clock exceeds Unix millisecond range",
                )
            })?,
        })
    }
}
