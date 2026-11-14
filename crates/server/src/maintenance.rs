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
) -> io::Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = closing.changed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
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
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            engine.expire_due(budget)
        })
        .await
        .map_err(|_| io::Error::other("TTL maintenance task failed"))?
        .map_err(io::Error::other)?;
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
        let task = tokio::spawn(run(engine, slots, 1, receiver));
        tokio::time::sleep(Duration::from_millis(10)).await;
        closing.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
