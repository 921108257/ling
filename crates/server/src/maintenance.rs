//! 单个 TTL 维护循环，与请求共享有界执行许可，关闭时停止派发。
//! One TTL maintenance loop shares bounded execution permits with requests and stops dispatching at shutdown.

use ling_engine::Engine;
use std::{io, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, watch};

/// 每轮只派发一个有限工作批次；睡眠粒度不决定逻辑可见性。
/// Dispatches one finite work batch per round; sleep granularity never determines logical visibility.
pub(crate) async fn run(
    engine: Arc<Engine>,
    slots: Arc<Semaphore>,
    budget: usize,
    mut closing: watch::Receiver<bool>,
    runtime: Option<Arc<crate::operations::Runtime>>,
) -> io::Result<()> {
    let mut maintenance = std::time::Instant::now();
    let mut delay = Duration::from_millis(1);
    let mut previous_error = String::new();
    loop {
        tokio::select! {
            biased;
            _ = closing.changed() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        let permit = tokio::select! {
            biased;
            _ = closing.changed() => return Ok(()),
            permit = crate::telemetry::acquire(slots.clone(), 0, runtime.as_ref().map(|runtime| runtime.telemetry.clone()), true) => permit.map_err(|_| io::Error::other("Execution permits closed"))?,
        };
        if *closing.borrow() {
            return Ok(());
        }
        let compact = maintenance.elapsed() >= Duration::from_secs(1);
        if compact {
            maintenance = std::time::Instant::now();
            // 中毒不可恢复：按故障策略记录稳定错误码后退出，交给服务管理器重启；与保留维护同为每秒一次，检测延迟至多约一秒。
            // Poison is unrecoverable: per the failure policy, log the stable code and exit so the supervisor restarts us; checked once per second like retention, so detection lags by about a second at most.
            if let Some(error) = poisoned(&engine) {
                if let Some(runtime) = &runtime {
                    runtime.log(
                        "error",
                        &format!("{error}; exiting so the supervisor can restart"),
                    );
                }
                return Err(io::Error::other(error));
            }
        }
        let engine = engine.clone();
        let metrics = runtime.as_ref().map(|runtime| runtime.telemetry.clone());
        let result = crate::telemetry::blocking(permit, move || {
            if let Some(metrics) = &metrics {
                crate::telemetry::add(&metrics.maintenance_cycles, 1);
            }
            // 即使到期记账遇到容量限制，保留清理仍可让后续重试恢复。
            // Retention cleanup can unblock later retries even when expiry accounting is capacity-limited.
            if compact {
                if let Some(events) = engine.events() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|_| {
                            ling_types::DbError::new(
                                ling_types::ErrorCode::Busy,
                                "System clock unavailable",
                            )
                        })?;
                    events.maintain(now.as_millis().min(u128::from(u64::MAX)) as u64)?;
                }
            }
            engine.expire_due(budget)
        })
        .await
        .map_err(|_| io::Error::other("TTL maintenance task failed"))?;
        match result {
            Ok(_) => {
                delay = Duration::from_millis(1);
                previous_error.clear();
            }
            Err(e)
                if matches!(
                    e.code.as_str(),
                    "ERR_JOURNAL_FULL" | "ERR_JOURNAL_UNAVAILABLE" | "ERR_COMMIT_UNKNOWN"
                ) =>
            {
                delay = Duration::from_millis(100);
                if previous_error != e.code {
                    if let Some(runtime) = &runtime {
                        runtime.log("warn", &e.code);
                    }
                    previous_error = e.code;
                }
            }
            Err(e) => return Err(io::Error::other(e)),
        }
    }
}

/// 引擎（预算锁或任一分片锁）或事件状态锁中毒即不可恢复；返回应记录并导致退出的稳定错误码。
/// Poison in the engine (budget lock or any shard lock) or the event state lock is unrecoverable; returns the stable error to log before exiting.
fn poisoned(engine: &Engine) -> Option<ling_types::DbError> {
    if engine.is_poisoned() {
        return Some(ling_types::DbError::new(
            ling_types::ErrorCode::Busy,
            "Data engine lock poisoned; unrecoverable",
        ));
    }
    if engine.events().is_some_and(|events| events.is_poisoned()) {
        return Some(ling_types::DbError::new(
            ling_types::ErrorCode::JournalUnavailable,
            "Event state lock poisoned; unrecoverable",
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ling_types::Value;

    /// 即使没有网络请求，后台仍会回收条目与其预算。
    /// Background work reclaims entries and their budget even without network requests.
    #[tokio::test]
    async fn idle_server_reclaims_expired_memory() {
        let engine = Arc::new(Engine::new(ling_engine::Config::default(), 1).unwrap());
        engine
            .execute(
                0,
                "SET",
                &[
                    Value::String("idle".into()),
                    Value::Null,
                    Value::String("PX".into()),
                    Value::Int(20),
                ],
                4096,
            )
            .unwrap();
        assert!(engine.memory_usage().unwrap() > 0);
        let (closing, receiver) = watch::channel(false);
        let runtime = Arc::new(crate::operations::Runtime::new(Arc::new(
            crate::Config::default(),
        )));
        let task = tokio::spawn(run(
            engine.clone(),
            Arc::new(Semaphore::new(1)),
            1,
            receiver,
            Some(runtime.clone()),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.memory_usage().unwrap() != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.scheduled_expirations().unwrap(), 0);
        assert_eq!(engine.maintenance_reclaimed().unwrap(), 1);
        assert!(
            runtime
                .telemetry
                .maintenance_cycles
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );
        closing.send_replace(true);
        task.await.unwrap().unwrap();
    }

    /// 分片锁中毒后维护循环返回带稳定错误码的错误，交给服务管理器重启；
    /// 耗时接近一秒说明退出来自每秒一次的中毒检测，而不是更早的到期回收错误。
    /// After a shard lock is poisoned the maintenance loop returns an error with a stable code so the supervisor restarts the process;
    /// an elapsed time near one second shows the exit came from the once-per-second poison check rather than an earlier expiry error.
    #[tokio::test]
    async fn poisoned_shard_stops_maintenance() {
        let engine = Arc::new(Engine::new(ling_engine::Config::default(), 1).unwrap());
        assert!(!engine.is_poisoned());
        engine.poison_shard_for_test();
        assert!(engine.is_poisoned());
        let (_closing, receiver) = watch::channel(false);
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(engine, Arc::new(Semaphore::new(1)), 1, receiver, None),
        )
        .await
        .expect("maintenance did not stop after poison");
        let error = result.unwrap_err();
        assert!(error.to_string().contains("ERR_BUSY"), "{error}");
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
    }

    /// Fast 发布回调 panic 会让事件状态锁中毒，维护按稳定 Journal 错误退出而不永久重试。
    /// A panic in a Fast publication callback poisons the event-state lock; maintenance exits with a stable Journal error instead of retrying forever.
    #[tokio::test]
    async fn poisoned_event_state_stops_maintenance() {
        let store =
            Arc::new(ling_events::EventStore::open(ling_events::Config::default(), 1, 1).unwrap());
        let change = ling_events::Change {
            db: 0,
            shard: 0,
            kind: ling_types::EventKind::Created,
            key: "k".into(),
            generation: ling_types::Generation {
                epoch: 1,
                counter: 1,
            },
            revision: 1,
            timestamp_ms: 1,
        };
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.commit(&[change], || panic!("test publication panic"));
        }));
        assert!(failed.is_err());
        assert!(store.is_poisoned());
        let config = ling_engine::Config {
            databases: 1,
            shard_count: 1,
            ..ling_engine::Config::default()
        };
        let engine = Arc::new(
            Engine::with_events(config, 1, Arc::new(ling_engine::SystemClock), Some(store))
                .unwrap(),
        );
        let (_closing, receiver) = watch::channel(false);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            run(engine, Arc::new(Semaphore::new(1)), 1, receiver, None),
        )
        .await
        .expect("maintenance kept retrying poisoned event state")
        .unwrap_err();
        assert!(
            error.to_string().contains("ERR_JOURNAL_UNAVAILABLE"),
            "{error}"
        );
    }

    /// 许可被请求占用时，关闭通知仍可取消排队的维护。
    /// Shutdown cancels queued maintenance even while requests hold all execution permits.
    #[tokio::test]
    async fn shutdown_interrupts_waiting_for_a_permit() {
        let slots = Arc::new(Semaphore::new(0));
        let engine = Arc::new(Engine::new(ling_engine::Config::default(), 1).unwrap());
        let (closing, receiver) = watch::channel(false);
        let runtime = Arc::new(crate::operations::Runtime::new(Arc::new(
            crate::Config::default(),
        )));
        let task = tokio::spawn(run(engine, slots, 1, receiver, Some(runtime.clone())));
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime
                .telemetry
                .execution_queued
                .load(std::sync::atomic::Ordering::Relaxed)
                == 0
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            runtime
                .telemetry
                .maintenance_deferred
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        closing.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime
                .telemetry
                .execution_queued
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            runtime
                .telemetry
                .execution_wait_cancelled
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}
