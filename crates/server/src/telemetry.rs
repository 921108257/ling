//! 有界执行与订阅的生命周期计数；关闭指标时不采集高频时间戳。
//! Lifecycle counters for bounded execution and subscriptions; disabled metrics collect no hot-path timestamps.

use ling_types::{HistogramSnapshot, LATENCY_BUCKET_BOUNDS_US};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// 固定桶的并发累积器，快照不保证跨字段原子性。
/// Concurrent fixed buckets whose snapshots are not atomic across fields.
pub(crate) struct Histogram {
    buckets: [AtomicU64; 7],
    sum: AtomicU64,
}

impl Histogram {
    /// 创建空直方图；Creates an empty histogram.
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum: AtomicU64::new(0),
        }
    }

    /// 微秒分桶与共享 Journal 口径一致；Microsecond buckets match the shared Journal convention.
    fn observe(&self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let index = LATENCY_BUCKET_BOUNDS_US
            .iter()
            .position(|bound| micros <= *bound)
            .unwrap_or(6);
        add(&self.buckets[index], 1);
        add(&self.sum, micros);
    }

    /// 读取累计值，不重置；Reads cumulative values without resetting them.
    pub(crate) fn snapshot(&self) -> HistogramSnapshot {
        let buckets = std::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed));
        HistogramSnapshot {
            count: buckets
                .iter()
                .fold(0u64, |sum, count| sum.saturating_add(*count)),
            buckets,
            sum: self.sum.load(Ordering::Relaxed),
        }
    }
}

/// 每实例低基数计数；只有直方图依赖 metrics.enabled。
/// Per-instance low-cardinality counters; only histograms depend on metrics.enabled.
pub(crate) struct Telemetry {
    /// 启动时固定的计时开关；Fixed startup timing switch.
    pub(crate) enabled: bool,
    /// 已完成请求时延；Completed request durations.
    pub(crate) request_duration: Histogram,
    /// 许可取得尝试时延；Permit acquisition attempt durations.
    pub(crate) execution_wait: Histogram,
    /// 实际仍持有的执行许可；Execution permits still held.
    pub(crate) execution_occupied: AtomicU64,
    /// 正在等待许可的任务；Tasks awaiting a permit.
    pub(crate) execution_queued: AtomicU64,
    /// 执行前等待超时；Wait timeouts before execution.
    pub(crate) execution_wait_timeouts: AtomicU64,
    /// 调用方取消的等待；Waits cancelled by the caller.
    pub(crate) execution_wait_cancelled: AtomicU64,
    /// 已开始的维护批次；Started maintenance batches.
    pub(crate) maintenance_cycles: AtomicU64,
    /// 因许可忙而等待的维护轮次；Maintenance rounds deferred for capacity.
    pub(crate) maintenance_deferred: AtomicU64,
    /// 首帧完整读取超时；First-frame completion timeouts.
    pub(crate) first_frame_timeouts: AtomicU64,
    /// 错误口令累计；Cumulative wrong passwords.
    pub(crate) auth_failures: AtomicU64,
    /// 仍持有消费身份的订阅；Subscriptions retaining a consumer identity.
    pub(crate) subscriptions: AtomicU64,
    /// 实际调用事件读取次数；Actual event polling calls.
    pub(crate) subscription_polls: AtomicU64,
    /// 正常返回无事件的读取次数；Polls returning no event successfully.
    pub(crate) subscription_empty_polls: AtomicU64,
}

impl Telemetry {
    /// 开关在启动时固定，禁用时连 Instant::now 也不调用。
    /// The startup switch is fixed; disabled timing never calls Instant::now.
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            request_duration: Histogram::new(),
            execution_wait: Histogram::new(),
            execution_occupied: AtomicU64::new(0),
            execution_queued: AtomicU64::new(0),
            execution_wait_timeouts: AtomicU64::new(0),
            execution_wait_cancelled: AtomicU64::new(0),
            maintenance_cycles: AtomicU64::new(0),
            maintenance_deferred: AtomicU64::new(0),
            first_frame_timeouts: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            subscriptions: AtomicU64::new(0),
            subscription_polls: AtomicU64::new(0),
            subscription_empty_polls: AtomicU64::new(0),
        }
    }

    /// 可选计时起点；Optional timing origin.
    pub(crate) fn started(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// 已完成请求的计时；Timing for a completed request.
    pub(crate) fn record_request(&self, started: Option<Instant>) {
        if let Some(started) = started {
            self.request_duration.observe(started.elapsed());
        }
    }
}

/// 累计计数饱和而不回绕；Cumulative counters saturate instead of wrapping.
pub(crate) fn add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

/// 队列等待的取消安全清理；Cancellation-safe cleanup of a queue wait.
struct Waiting {
    telemetry: Option<Arc<Telemetry>>,
    started: Option<Instant>,
    queued: bool,
    cancelled: bool,
}

impl Drop for Waiting {
    /// 取消和超时都释放排队数；Cancellation and timeout both release the queued gauge.
    fn drop(&mut self) {
        if let Some(metrics) = &self.telemetry {
            if self.queued {
                metrics.execution_queued.fetch_sub(1, Ordering::Relaxed);
            }
            if self.cancelled {
                add(&metrics.execution_wait_cancelled, 1);
            }
            if let Some(started) = self.started {
                metrics.execution_wait.observe(started.elapsed());
            }
        }
    }
}

/// 许可随阻塞任务持有，异步调用方取消不会提前释放占用数。
/// The blocking job owns this permit, so cancellation cannot release occupied capacity early.
pub(crate) struct ExecutionPermit {
    _permit: OwnedSemaphorePermit,
    telemetry: Option<Arc<Telemetry>>,
}

impl Drop for ExecutionPermit {
    /// 与实际许可一起归还占用数；Releases the occupied gauge with the real permit.
    fn drop(&mut self) {
        if let Some(metrics) = &self.telemetry {
            metrics.execution_occupied.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// 所有执行入口共用的等待口径；零超时表示无限等待。
/// Shared wait semantics for all execution paths; zero timeout means unlimited waiting.
pub(crate) async fn acquire(
    slots: Arc<Semaphore>,
    timeout_ms: u64,
    telemetry: Option<Arc<Telemetry>>,
    maintenance: bool,
) -> Result<ExecutionPermit, ()> {
    let mut waiting = Waiting {
        started: telemetry.as_ref().and_then(|metrics| metrics.started()),
        telemetry: telemetry.clone(),
        queued: false,
        cancelled: true,
    };
    let permit = match slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(TryAcquireError::Closed) => {
            waiting.cancelled = false;
            return Err(());
        }
        Err(TryAcquireError::NoPermits) => {
            if let Some(metrics) = &telemetry {
                add(&metrics.execution_queued, 1);
                if maintenance {
                    add(&metrics.maintenance_deferred, 1);
                }
            }
            waiting.queued = true;
            let result = if timeout_ms == 0 {
                slots.acquire_owned().await.map_err(|_| ())
            } else {
                match tokio::time::timeout(Duration::from_millis(timeout_ms), slots.acquire_owned())
                    .await
                {
                    Ok(result) => result.map_err(|_| ()),
                    Err(_) => {
                        waiting.cancelled = false;
                        if let Some(metrics) = &telemetry {
                            add(&metrics.execution_wait_timeouts, 1);
                        }
                        return Err(());
                    }
                }
            };
            match result {
                Ok(permit) => permit,
                Err(()) => {
                    waiting.cancelled = false;
                    return Err(());
                }
            }
        }
    };
    waiting.cancelled = false;
    if let Some(metrics) = &telemetry {
        add(&metrics.execution_occupied, 1);
    }
    Ok(ExecutionPermit {
        _permit: permit,
        telemetry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上界包含等值，溢出桶独立且计数与总和饱和。
    /// Upper bounds are inclusive, the overflow bucket is separate, and counts and sums saturate.
    #[test]
    fn fixed_histogram_boundaries_and_saturation() {
        let histogram = Histogram::new();
        for micros in [
            0, 10, 11, 100, 101, 1000, 1001, 10000, 10001, 100000, 100001, 1000000, 1000001,
        ] {
            histogram.observe(Duration::from_micros(micros));
        }
        assert_eq!(histogram.snapshot().buckets, [2, 2, 2, 2, 2, 2, 1]);
        assert_eq!(histogram.snapshot().count, 13);
        histogram.buckets[0].store(u64::MAX, Ordering::Relaxed);
        histogram.sum.store(u64::MAX, Ordering::Relaxed);
        histogram.observe(Duration::from_micros(1));
        assert_eq!(histogram.snapshot().buckets[0], u64::MAX);
        assert_eq!(histogram.snapshot().count, u64::MAX);
        assert_eq!(histogram.snapshot().sum, u64::MAX);
    }

    /// 等待取消、超时与关闭各自释放排队数且不伪称任务已执行。
    /// Cancellation, timeout, and closure release queued counts without pretending work ran.
    #[tokio::test(start_paused = true)]
    async fn queue_outcomes_release_counts() {
        let metrics = Arc::new(Telemetry::new(true));
        let slots = Arc::new(Semaphore::new(0));
        let waiter = tokio::spawn(acquire(slots.clone(), 0, Some(metrics.clone()), true));
        tokio::task::yield_now().await;
        assert_eq!(metrics.execution_queued.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.maintenance_deferred.load(Ordering::Relaxed), 1);
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert_eq!(metrics.execution_queued.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.execution_wait_cancelled.load(Ordering::Relaxed), 1);
        assert!(
            acquire(slots.clone(), 20, Some(metrics.clone()), false)
                .await
                .is_err()
        );
        assert_eq!(metrics.execution_queued.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.execution_wait_timeouts.load(Ordering::Relaxed), 1);
        slots.close();
        assert!(
            acquire(slots, 0, Some(metrics.clone()), false)
                .await
                .is_err()
        );
        assert_eq!(metrics.execution_wait_cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.execution_wait.snapshot().count, 3);
    }

    /// 已开始任务在调用方取消后保留许可，直至真实完成才归还。
    /// A started job retains capacity after caller cancellation until real completion.
    #[tokio::test]
    async fn cancelled_blocking_job_retains_occupied_capacity() {
        let metrics = Arc::new(Telemetry::new(false));
        let slots = Arc::new(Semaphore::new(1));
        let permit = acquire(slots.clone(), 0, Some(metrics.clone()), false)
            .await
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(blocking(permit, move || {
            let _ = started_tx.send(());
            finish_rx.recv().unwrap();
        }));
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(metrics.execution_occupied.load(Ordering::Relaxed), 1);
        finish_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(metrics.execution_occupied.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.execution_wait.snapshot().count, 0);
        drop(permit);
    }

    /// 禁用时无计时起点或样本，计数及订阅 RAII 仍有效。
    /// Disabled timing has no origin or samples; counters and subscription RAII still work.
    #[tokio::test]
    async fn disabled_timing_and_subscription_cleanup() {
        let metrics = Arc::new(Telemetry::new(false));
        assert!(metrics.started().is_none());
        metrics.record_request(metrics.started());
        let permit = acquire(Arc::new(Semaphore::new(1)), 0, Some(metrics.clone()), false)
            .await
            .unwrap();
        assert_eq!(metrics.execution_occupied.load(Ordering::Relaxed), 1);
        drop(permit);
        assert_eq!(metrics.execution_occupied.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.request_duration.snapshot().count, 0);
        assert_eq!(metrics.execution_wait.snapshot().count, 0);
        let subscription = Arc::new(SubscriptionCount::new(metrics.clone()));
        let in_flight = subscription.clone();
        drop(subscription);
        assert_eq!(metrics.subscriptions.load(Ordering::Relaxed), 1);
        drop(in_flight);
        assert_eq!(metrics.subscriptions.load(Ordering::Relaxed), 0);
    }

    /// RSS 解析严格校验单位与溢出，平台不可用时不输出伪零。
    /// RSS parsing validates units and overflow; unavailable platforms never emit fake zeros.
    #[test]
    fn process_samples_are_explicit() {
        assert_eq!(
            ProcessSample::rss("Name:\ttest\nVmRSS:\t123 kB\n"),
            Some(123 * 1024)
        );
        assert_eq!(ProcessSample::rss("VmRSS: 1 MB\n"), None);
        assert_eq!(ProcessSample::rss("VmRSS: 18446744073709551615 kB\n"), None);
        let sample = ProcessSample::collect();
        #[cfg(target_os = "linux")]
        {
            assert!(sample.rss_bytes.is_some_and(|value| value > 0));
            assert!(sample.cpu_seconds.is_some_and(|value| value >= 0.0));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(sample.rss_bytes.is_none());
            assert!(sample.cpu_seconds.is_none());
        }
    }
}

/// 不取消已提交的阻塞任务，许可在真实完成（含 panic）后归还。
/// Submitted blocking jobs are not cancelled; capacity returns only after real completion, including panic.
pub(crate) async fn blocking<T: Send + 'static>(
    permit: ExecutionPermit,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

/// 活跃订阅跟随最后一个在途消费者句柄；Active subscriptions follow the last in-flight consumer handle.
pub(crate) struct SubscriptionCount(pub(crate) Arc<Telemetry>);
impl SubscriptionCount {
    pub(crate) fn new(telemetry: Arc<Telemetry>) -> Self {
        add(&telemetry.subscriptions, 1);
        Self(telemetry)
    }
}
impl Drop for SubscriptionCount {
    fn drop(&mut self) {
        self.0.subscriptions.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Linux 进程采样；无法读取的项目保持 None，不伪造为零。
/// Linux process samples; unavailable fields remain None rather than pretending to be zero.
#[derive(Default)]
pub(crate) struct ProcessSample {
    /// 驻留集字节；Resident set bytes.
    pub(crate) rss_bytes: Option<u64>,
    /// 全进程累计 CPU 秒；Cumulative process CPU seconds.
    pub(crate) cpu_seconds: Option<f64>,
}

impl ProcessSample {
    /// 只在 INFO 或抓取时采样；Samples only during INFO or metric collection.
    pub(crate) fn collect() -> Self {
        #[cfg(target_os = "linux")]
        {
            let rss_bytes = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|text| Self::rss(&text));
            let cpu = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
            Self {
                rss_bytes,
                cpu_seconds: Some(cpu.tv_sec as f64 + cpu.tv_nsec as f64 / 1_000_000_000.0),
            }
        }
        #[cfg(not(target_os = "linux"))]
        Self::default()
    }

    /// 内核 VmRSS 单位严格为 kB，乘法溢出视为不可用。
    /// Kernel VmRSS must use kB; multiplication overflow makes the sample unavailable.
    #[cfg(any(target_os = "linux", test))]
    fn rss(text: &str) -> Option<u64> {
        let line = text.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
        let mut fields = line.split_whitespace();
        let value: u64 = fields.next()?.parse().ok()?;
        (fields.next()? == "kB" && fields.next().is_none())
            .then(|| value.checked_mul(1024))
            .flatten()
    }
}
