//! 单个 TTL 维护循环，与请求共享有界执行许可，关闭时停止派发。
//! One TTL maintenance loop shares bounded execution permits with requests and stops dispatching at shutdown.

use linger_engine::Engine;
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
            permit = slots.clone().acquire_owned() => permit.map_err(|_| io::Error::other("Execution permits closed"))?,
        };
        if *closing.borrow() {
            return Ok(());
        }
        let engine = engine.clone();
        let compact = maintenance.elapsed() >= Duration::from_secs(1);
        if compact {
            maintenance = std::time::Instant::now();
        }
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // 即使到期记账遇到容量限制，保留清理仍可让后续重试恢复。
            // Retention cleanup can unblock later retries even when expiry accounting is capacity-limited.
            if compact {
                if let Some(events) = engine.events() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|_| {
                            linger_types::DbError::new(
                                linger_types::ErrorCode::Busy,
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

#[cfg(test)]
mod tests {
    use super::*;
    use linger_types::Value;

    /// 即使没有网络请求，后台仍会回收条目与其预算。
    /// Background work reclaims entries and their budget even without network requests.
    #[tokio::test]
    async fn idle_server_reclaims_expired_memory() {
        let engine = Arc::new(Engine::new(linger_engine::Config::default(), 1).unwrap());
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
        let task = tokio::spawn(run(
            engine.clone(),
            Arc::new(Semaphore::new(1)),
            1,
            receiver,
            None,
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.memory_usage().unwrap() != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.scheduled_expirations().unwrap(), 0);
        closing.send_replace(true);
        task.await.unwrap().unwrap();
    }

    /// 许可被请求占用时，关闭通知仍可取消排队的维护。
    /// Shutdown cancels queued maintenance even while requests hold all execution permits.
    #[tokio::test]
    async fn shutdown_interrupts_waiting_for_a_permit() {
        let slots = Arc::new(Semaphore::new(0));
        let engine = Arc::new(Engine::new(linger_engine::Config::default(), 1).unwrap());
        let (closing, receiver) = watch::channel(false);
        let task = tokio::spawn(run(engine, slots, 1, receiver, None));
        tokio::time::sleep(Duration::from_millis(10)).await;
        closing.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
