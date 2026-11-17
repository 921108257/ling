//! 接入层失败处置：accept 瞬时错误退避重试，连接拒绝与 accept 错误只计数并限频告警。
//! Accept-side failure handling: transient accept errors back off and retry; rejections and accept errors are only counted and warned about at a limited rate.

use std::{
    future::Future,
    io,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// 退避起点；每次失败翻倍，成功接受一个连接后复位。
/// Backoff start; the delay doubles on each failure and resets once a connection is accepted.
const BACKOFF_START: Duration = Duration::from_millis(10);
/// 退避上限：句柄释放后至多多等一秒即可恢复接入。
/// Backoff cap: once descriptors are freed, accepting resumes after at most one more second.
const BACKOFF_MAX: Duration = Duration::from_secs(1);
/// 同一类告警的最短间隔，与认证失败告警一致。
/// Minimum interval between warnings of one kind, matching the authentication-failure warning.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// 表示本进程暂时缺少句柄或内存的系统错误号（ENOMEM、ENFILE、EMFILE、ENOBUFS）。
/// 工作区禁止 unsafe 且不依赖 libc，因此按平台写出常量。
/// OS error numbers meaning this process is temporarily short of memory or descriptors (ENOMEM, ENFILE, EMFILE, ENOBUFS).
/// The workspace forbids unsafe and does not depend on libc, so the constants are listed per platform.
#[cfg(target_os = "linux")]
const EXHAUSTED: &[i32] = &[12, 23, 24, 105];
#[cfg(target_os = "macos")]
const EXHAUSTED: &[i32] = &[12, 23, 24, 55];
#[cfg(windows)]
const EXHAUSTED: &[i32] = &[10024, 10055];
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const EXHAUSTED: &[i32] = &[12, 23, 24];

/// 一次 accept 错误的处置方式。
/// How a single accept error is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// 对端在接受前已中止，直接接受下一个连接。
    /// The peer gave up before accept; accept the next connection at once.
    Retry,
    /// 本进程暂时缺少句柄或内存，退避后重试。
    /// The process is temporarily short of descriptors or memory; back off, then retry.
    Backoff,
    /// 监听器不可用或原因未知：沿用原行为，把错误交还调用方。
    /// The listener is unusable or the cause is unknown: keep the previous behaviour and hand the error back.
    Fatal,
}

/// 只把已确认的瞬时错误归为可恢复；未知错误保持致命，避免掩盖真正损坏的监听器。
/// Only confirmed transient errors are recoverable; unknown errors stay fatal so a genuinely broken listener is not masked.
pub(crate) fn classify(error: &io::Error) -> Disposition {
    match error.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::Interrupted
        | io::ErrorKind::WouldBlock
        | io::ErrorKind::TimedOut => Disposition::Retry,
        io::ErrorKind::OutOfMemory => Disposition::Backoff,
        _ if error
            .raw_os_error()
            .is_some_and(|code| EXHAUSTED.contains(&code)) =>
        {
            Disposition::Backoff
        }
        _ => Disposition::Fatal,
    }
}

/// 下一次退避时长：首次取起点，之后翻倍直至上限。
/// Next backoff delay: the start on the first failure, then doubling up to the cap.
fn next_backoff(previous: Option<Duration>) -> Duration {
    previous.map_or(BACKOFF_START, |delay| {
        delay.saturating_mul(2).min(BACKOFF_MAX)
    })
}

/// 重复执行 `accept` 直到得到连接或遇到致命错误。`backoff` 由调用方保存，跨调用保留，成功后复位；
/// 取消本 future 只丢弃当前等待，不丢失已接受的连接。
/// Repeats `accept` until it yields a connection or a fatal error. The caller owns `backoff`, which persists across calls and resets on success;
/// dropping this future only discards the current wait and never loses an accepted connection.
pub(crate) async fn recover<T, F, Fut>(
    mut accept: F,
    backoff: &mut Option<Duration>,
    mut observe: impl FnMut(&io::Error),
) -> io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    loop {
        let error = match accept().await {
            Ok(value) => {
                *backoff = None;
                return Ok(value);
            }
            Err(error) => error,
        };
        match classify(&error) {
            Disposition::Fatal => return Err(error),
            Disposition::Retry => {
                observe(&error);
                tokio::task::yield_now().await;
            }
            Disposition::Backoff => {
                observe(&error);
                let delay = next_backoff(*backoff);
                *backoff = Some(delay);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// 固定间隔限频器，只记住上次放行的时刻，不保存任何请求内容。
/// Fixed-interval limiter that remembers only the last permitted instant and never stores request content.
struct Throttle(Mutex<Option<Instant>>);

impl Throttle {
    /// 创建尚未放行过的限频器。
    /// Creates a limiter that has not yet permitted anything.
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// 距上次放行不足间隔则拒绝；`now` 由调用方传入，测试不依赖真实时钟。
    /// Refuses within the interval of the last permit; the caller supplies `now` so tests avoid the real clock.
    fn allow(&self, now: Instant) -> bool {
        let Ok(mut last) = self.0.lock() else {
            return false;
        };
        if last.is_some_and(|previous| now.duration_since(previous) < WARNING_INTERVAL) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// 接入层内部计数与限频告警，不含地址、Key 或口令；对外指标放到 0.1.5，避免本版改动 INFO 契约。
/// Internal accept-side counters and rate-limited warnings without addresses, keys or passwords; public metrics wait for 0.1.5 so this version leaves the INFO contract alone.
pub(crate) struct AcceptStats {
    /// 因连接上限被拒绝的连接累计数；Connections rejected at the connection limit.
    rejected: AtomicU64,
    /// accept 错误累计数，含已恢复的；Accept errors seen, including recovered ones.
    failures: AtomicU64,
    /// 连接任务 panic 累计数；Connection task panics seen.
    panics: AtomicU64,
    /// 连接拒绝告警；Connection-rejection warning.
    reject_warning: Throttle,
    /// 业务端口 accept 告警；Business-listener accept warning.
    data_warning: Throttle,
    /// 指标端口 accept 告警，与业务端口互不抑制；Metrics-listener accept warning, independent of the business one.
    metrics_warning: Throttle,
    /// 连接任务 panic 告警；Connection-panic warning.
    panic_warning: Throttle,
}

impl AcceptStats {
    /// 创建全零计数与未触发的限频器。
    /// Creates zeroed counters and untriggered limiters.
    pub(crate) fn new() -> Self {
        Self {
            rejected: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            reject_warning: Throttle::new(),
            data_warning: Throttle::new(),
            metrics_warning: Throttle::new(),
            panics: AtomicU64::new(0),
            panic_warning: Throttle::new(),
        }
    }

    /// 每次拒绝都计数；告警按间隔限频，由 `warn` 写出固定格式的消息。
    /// Every rejection is counted; warnings are interval-limited and `warn` receives a fixed-format message.
    pub(crate) fn rejected(&self, now: Instant, warn: impl FnOnce(&str)) {
        let total = self
            .rejected
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if self.reject_warning.allow(now) {
            warn(&format!(
                "Connection limit reached; {total} connections rejected since start"
            ));
        }
    }

    /// 每个 accept 错误都计数；消息只含系统错误号或错误种类，业务端口与指标端口分别限频。
    /// Every accept error is counted; messages carry only the OS error number or error kind, with separate limits for the business and metrics listeners.
    pub(crate) fn failed(
        &self,
        metrics: bool,
        error: &io::Error,
        now: Instant,
        warn: impl FnOnce(&str),
    ) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let (throttle, listener) = if metrics {
            (&self.metrics_warning, "Metrics")
        } else {
            (&self.data_warning, "Business")
        };
        if throttle.allow(now) {
            let cause = error.raw_os_error().map_or_else(
                || format!("{:?}", error.kind()),
                |code| format!("os error {code}"),
            );
            warn(&format!(
                "{listener} listener accept error ({cause}); continuing"
            ));
        }
    }

    /// 连接任务 panic 只终止该连接：每次都计数，告警限频，消息不含任务内容。
    /// A connection task panic ends only that connection: every one is counted and warnings are limited, with no task content in the message.
    pub(crate) fn panicked(&self, now: Instant, warn: impl FnOnce(&str)) {
        let total = self
            .panics
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if self.panic_warning.allow(now) {
            warn(&format!(
                "Connection task panicked; {total} panics since start; other connections unaffected"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// 连接任务 panic 每次都计数，告警限频；间隔后的告警携带最新累计数。
    /// Every connection task panic is counted while warnings are limited; the warning after the interval carries the latest total.
    #[test]
    fn panics_are_counted_and_warnings_limited() {
        let stats = AcceptStats::new();
        let start = Instant::now();
        let warnings = RefCell::new(Vec::<String>::new());
        let warn = |m: &str| warnings.borrow_mut().push(m.to_owned());
        for _ in 0..10 {
            stats.panicked(start, warn);
        }
        assert_eq!(stats.panics.load(Ordering::Relaxed), 10);
        assert_eq!(warnings.borrow().len(), 1);
        assert_eq!(
            warnings.borrow()[0],
            "Connection task panicked; 1 panics since start; other connections unaffected"
        );
        stats.panicked(start + WARNING_INTERVAL, warn);
        assert_eq!(stats.panics.load(Ordering::Relaxed), 11);
        assert_eq!(warnings.borrow().len(), 2);
        assert!(
            warnings.borrow()[1].contains("11 panics"),
            "{}",
            warnings.borrow()[1]
        );
    }

    /// 属于句柄耗尽类的系统错误。
    /// An OS error from the descriptor-exhaustion class.
    fn exhausted() -> io::Error {
        io::Error::from_raw_os_error(EXHAUSTED[0])
    }

    /// 脚本化 accept：依次返回给定错误，随后成功；返回结果、观察回调次数与（可暂停时钟下的）耗时。
    /// Scripted accept: yields the given errors in order, then succeeds; returns the result, observer calls and elapsed time (on a pausable clock).
    async fn drive(
        mut errors: Vec<io::Error>,
        backoff: &mut Option<Duration>,
    ) -> (io::Result<u32>, usize, Duration) {
        errors.reverse();
        let script = RefCell::new(errors);
        let observed = Cell::new(0usize);
        let start = tokio::time::Instant::now();
        let result = recover(
            || {
                let next = script.borrow_mut().pop();
                async move { next.map_or(Ok(7), Err) }
            },
            backoff,
            |_| observed.set(observed.get() + 1),
        )
        .await;
        (result, observed.get(), start.elapsed())
    }

    /// 按错误种类与系统错误号核对处置分类；未知错误必须保持致命。
    /// Checks classification by error kind and OS error number; unknown errors must stay fatal.
    #[test]
    fn classification_table() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::TimedOut,
        ] {
            assert_eq!(
                classify(&io::Error::from(kind)),
                Disposition::Retry,
                "{kind:?}"
            );
        }
        assert_eq!(
            classify(&io::Error::from(io::ErrorKind::OutOfMemory)),
            Disposition::Backoff
        );
        for code in EXHAUSTED {
            assert_eq!(
                classify(&io::Error::from_raw_os_error(*code)),
                Disposition::Backoff,
                "{code}"
            );
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::AddrNotAvailable,
            io::ErrorKind::Other,
        ] {
            assert_eq!(
                classify(&io::Error::from(kind)),
                Disposition::Fatal,
                "{kind:?}"
            );
        }
    }

    /// 退避从起点倍增，到达上限后保持不变。
    /// Backoff doubles from its start and stays at the cap once reached.
    #[test]
    fn backoff_doubles_to_the_cap() {
        let mut delay = None;
        let mut seen = Vec::new();
        for _ in 0..9 {
            let next = next_backoff(delay);
            seen.push(next.as_millis());
            delay = Some(next);
        }
        assert_eq!(seen, [10, 20, 40, 80, 160, 320, 640, 1000, 1000]);
    }

    /// 连续句柄耗尽后退避累计等待，成功接受后状态复位。
    /// Consecutive exhaustion accumulates backoff waits, and the state resets after a successful accept.
    #[tokio::test(start_paused = true)]
    async fn exhaustion_backs_off_then_recovers() {
        let mut backoff = None;
        let (result, observed, elapsed) =
            drive(vec![exhausted(), exhausted(), exhausted()], &mut backoff).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(observed, 3);
        // 10 + 20 + 40 毫秒；Waits of 10 + 20 + 40 ms.
        assert!(
            elapsed >= Duration::from_millis(70) && elapsed < Duration::from_millis(200),
            "{elapsed:?}"
        );
        assert_eq!(backoff, None);
    }

    /// 长时间耗尽时每次等待不超过上限：12 次失败共 6270 毫秒，不封顶则约 40950 毫秒。
    /// Prolonged exhaustion never waits longer than the cap: 12 failures take 6270 ms, versus about 40950 ms without a cap.
    #[tokio::test(start_paused = true)]
    async fn backoff_is_capped_during_prolonged_exhaustion() {
        let mut backoff = None;
        let errors = (0..12).map(|_| exhausted()).collect();
        let (result, observed, elapsed) = drive(errors, &mut backoff).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(observed, 12);
        assert!(
            elapsed >= Duration::from_millis(6270) && elapsed < Duration::from_secs(7),
            "{elapsed:?}"
        );
    }

    /// 对端在接受前中止属于无需等待的重试，且不留下退避状态。
    /// A peer aborting before accept is retried without waiting and leaves no backoff state.
    #[tokio::test(start_paused = true)]
    async fn aborted_connections_retry_without_waiting() {
        let mut backoff = None;
        let errors = (0..5)
            .map(|_| io::Error::from(io::ErrorKind::ConnectionAborted))
            .collect();
        let (result, observed, elapsed) = drive(errors, &mut backoff).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(observed, 5);
        assert!(elapsed < Duration::from_millis(1), "{elapsed:?}");
        assert_eq!(backoff, None);
    }

    /// 未知错误立即原样返回，不观察、不等待，保持原有的致命语义。
    /// An unknown error is returned at once and unchanged, without observation or waiting, preserving the previous fatal semantics.
    #[tokio::test(start_paused = true)]
    async fn unknown_error_stays_fatal() {
        let mut backoff = None;
        let errors = vec![io::Error::from(io::ErrorKind::PermissionDenied)];
        let (result, observed, elapsed) = drive(errors, &mut backoff).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(observed, 0);
        assert!(elapsed < Duration::from_millis(1), "{elapsed:?}");
    }

    /// 限频器每个间隔只放行一次；时刻由测试注入。
    /// The limiter permits once per interval; the test injects the instants.
    #[test]
    fn throttle_permits_once_per_interval() {
        let throttle = Throttle::new();
        let start = Instant::now();
        assert!(throttle.allow(start));
        assert!(!throttle.allow(start + Duration::from_secs(59)));
        assert!(throttle.allow(start + WARNING_INTERVAL));
        assert!(!throttle.allow(start + WARNING_INTERVAL + Duration::from_secs(1)));
    }

    /// 每次拒绝都计数，告警限频；间隔后的告警携带最新累计数。
    /// Every rejection is counted while warnings are limited; the warning after the interval carries the latest total.
    #[test]
    fn rejections_are_counted_and_warnings_limited() {
        let stats = AcceptStats::new();
        let start = Instant::now();
        let warnings = RefCell::new(Vec::<String>::new());
        let warn = |m: &str| warnings.borrow_mut().push(m.to_owned());
        for _ in 0..100 {
            stats.rejected(start, warn);
        }
        assert_eq!(stats.rejected.load(Ordering::Relaxed), 100);
        assert_eq!(warnings.borrow().len(), 1);
        assert_eq!(
            warnings.borrow()[0],
            "Connection limit reached; 1 connections rejected since start"
        );
        stats.rejected(start + WARNING_INTERVAL, warn);
        assert_eq!(stats.rejected.load(Ordering::Relaxed), 101);
        assert_eq!(warnings.borrow().len(), 2);
        assert_eq!(
            warnings.borrow()[1],
            "Connection limit reached; 101 connections rejected since start"
        );
    }

    /// accept 错误按端口分别限频，告警只含错误号或种类，不泄露地址或口令。
    /// Accept errors are limited per listener, and warnings carry only the error number or kind, never addresses or passwords.
    #[test]
    fn accept_errors_are_limited_per_listener_without_leaking_detail() {
        let stats = AcceptStats::new();
        let start = Instant::now();
        let warnings = RefCell::new(Vec::<String>::new());
        let warn = |m: &str| warnings.borrow_mut().push(m.to_owned());
        let os = io::Error::from_raw_os_error(EXHAUSTED[1]);
        let detailed = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer 10.0.0.9:5000 password=hunter2",
        );
        stats.failed(false, &os, start, warn);
        stats.failed(false, &detailed, start, warn);
        stats.failed(true, &detailed, start, warn);
        assert_eq!(stats.failures.load(Ordering::Relaxed), 3);
        let seen = warnings.borrow();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("Business"), "{}", seen[0]);
        assert!(
            seen[0].contains(&format!("os error {}", EXHAUSTED[1])),
            "{}",
            seen[0]
        );
        assert!(seen[1].starts_with("Metrics"), "{}", seen[1]);
        assert!(seen[1].contains("PermissionDenied"), "{}", seen[1]);
        assert!(
            seen.iter()
                .all(|m| !m.contains("10.0.0.9") && !m.contains("hunter2"))
        );
    }
}
