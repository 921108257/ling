//! 固定认证身份和数据库的有界连接池；租约不会暴露可变 SELECT。
//! Bounded pool with fixed credentials and database; leases never expose mutable SELECT state.

use super::*;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// 池容量与排队期限；Pool capacity and queue deadline.
#[derive(Clone, Debug)]
pub struct PoolOptions {
    /// 包含在用、空闲和正在建连的总连接上限。
    /// Total connections including checked-out, idle, and connecting sockets.
    pub max_connections: usize,
    /// 等待容量的期限，零值无效；Capacity wait deadline; zero is invalid.
    pub acquire_timeout: Duration,
}
impl Default for PoolOptions {
    /// 默认最多八条连接，等待五秒；Defaults to eight connections and a five-second wait.
    fn default() -> Self {
        Self {
            max_connections: 8,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}
/// 克隆共享同一容量与关闭状态；关闭不会自动重放在途请求。
/// Clones share capacity and close state; closing never replays in-flight requests.
#[derive(Clone)]
pub struct Pool(Arc<Inner>);
/// 凭据独立于诊断数据，所有连接采用同一个初始化上下文。
/// Credentials stay outside diagnostics; every connection uses the same initialization context.
struct Inner {
    /// 固定地址；Fixed address.
    address: SocketAddr,
    /// 不派生 Debug；Never derives Debug.
    password: Option<String>,
    /// 固定数据库；Fixed database.
    db: u16,
    /// 传输设置；Transport settings.
    options: Options,
    /// 排队期限；Queue deadline.
    acquire_timeout: Duration,
    /// 容量含正在建立的连接；Capacity includes connections under construction.
    capacity: Arc<Semaphore>,
    /// 只存放健康普通连接；Only healthy ordinary connections are retained.
    idle: Mutex<Vec<Client>>,
    /// 关闭会唤醒等待者并取消在途等待；Close wakes waiters and cancels in-flight waits.
    closing: watch::Sender<bool>,
}
impl Pool {
    /// 验证设置但延迟建连；首次 acquire 完成全部握手后才返回。
    /// Validates settings and connects lazily; acquire returns only after complete initialization.
    pub fn new(
        address: SocketAddr,
        password: Option<String>,
        db: u16,
        options: Options,
        pool: PoolOptions,
    ) -> Result<Self, Error> {
        if pool.max_connections == 0
            || pool.max_connections > Semaphore::MAX_PERMITS
            || pool.acquire_timeout.is_zero()
            || options.connect_timeout.is_zero()
            || options.request_timeout.is_zero()
            || [
                pool.acquire_timeout,
                options.connect_timeout,
                options.request_timeout,
            ]
            .iter()
            .any(|d| std::time::Instant::now().checked_add(*d).is_none())
        {
            return Err(Error::local(LocalError::InvalidRequest, false));
        }
        let (closing, _) = watch::channel(false);
        Ok(Self(Arc::new(Inner {
            address,
            password,
            db,
            options,
            acquire_timeout: pool.acquire_timeout,
            capacity: Arc::new(Semaphore::new(pool.max_connections)),
            idle: Mutex::new(Vec::new()),
            closing,
        })))
    }
    /// 获取固定库租约；取消排队或建连会自动释放容量。
    /// Acquires a fixed-database lease; cancellation during waiting or connecting releases capacity.
    pub async fn acquire(&self) -> Result<PooledClient, Error> {
        let mut closing = self.0.closing.subscribe();
        if *closing.borrow() {
            return Err(Error::local(LocalError::Closed, false));
        }
        let permit = timeout(
            self.0.acquire_timeout,
            self.0.capacity.clone().acquire_owned(),
        )
        .await
        .map_err(|_| Error::local(LocalError::Timeout, false))?
        .map_err(|_| Error::local(LocalError::Closed, false))?;
        let idle = self
            .0
            .idle
            .lock()
            .map_err(|_| Error::local(LocalError::Closed, false))?
            .pop();
        let client = if let Some(client) = idle {
            client
        } else {
            tokio::select! {
                biased;
                _ = closing.changed() => return Err(Error::local(LocalError::Closed, false)),
                result = Client::connect(self.0.address, self.0.password.as_deref(), self.0.db, self.0.options.clone()) => result?,
            }
        };
        if *closing.borrow() {
            return Err(Error::local(LocalError::Closed, false));
        }
        Ok(PooledClient {
            pool: self.clone(),
            client: Some(client),
            _permit: permit,
        })
    }
    /// 一次普通调用；失败连接不入池，请求不自动重放。
    /// Executes one ordinary call; failed connections are discarded and requests never replay.
    pub async fn execute(&self, command: &str, arguments: Vec<Value>) -> Result<Value, Error> {
        self.acquire().await?.execute(command, arguments).await
    }
    /// 立即释放空闲连接、唤醒排队者并取消在途等待；闲置租约归还时释放。
    /// Releases idle sockets, wakes waiters, and cancels active waits; unused leases release on return.
    pub fn close(&self) {
        self.0.closing.send_replace(true);
        self.0.capacity.close();
        if let Ok(mut idle) = self.0.idle.lock() {
            idle.clear();
        }
    }
    /// 固定数据库不会因其他调用切换；The fixed database never changes through another call.
    pub fn database(&self) -> u16 {
        self.0.db
    }
}

/// 独占租约支持同连接 SCAN；禁止认证、切库和订阅状态变更。
/// Exclusive lease supports connection-bound SCAN while rejecting auth, database, and subscription changes.
pub struct PooledClient {
    /// 来源池；Originating pool.
    pool: Pool,
    /// 取消请求后 Client 自动变为关闭；Client closes automatically when requests are cancelled.
    client: Option<Client>,
    /// 租约归还容量；The lease returns capacity on drop.
    _permit: OwnedSemaphorePermit,
}
impl PooledClient {
    /// 在固定上下文执行普通命令；取消或关闭后的连接不再复用。
    /// Executes ordinary commands in a fixed context; cancelled or closed connections are never reused.
    pub async fn execute(&mut self, command: &str, arguments: Vec<Value>) -> Result<Value, Error> {
        if matches!(
            command.to_ascii_uppercase().as_str(),
            "HELLO" | "AUTH" | "SELECT" | "QUIT" | "SUBSCRIBE" | "UNSUBSCRIBE" | "ACK"
        ) {
            return Err(Error::local(LocalError::InvalidRequest, false));
        }
        let mut closing = self.pool.0.closing.subscribe();
        if *closing.borrow() {
            self.client.take();
            return Err(Error::local(LocalError::Closed, false));
        }
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        tokio::select! {
            biased;
            _ = closing.changed() => { self.client.take(); Err(Error::local(LocalError::Closed, true)) },
            result = client.execute(command, arguments) => result,
        }
    }
}
impl Drop for PooledClient {
    /// 只有健康且池未关闭的连接可归还；Only healthy sockets return to an open pool.
    fn drop(&mut self) {
        if let Some(client) = self.client.take().filter(|c| !c.is_closed()) {
            if let Ok(mut idle) = self.pool.0.idle.lock() {
                if !*self.pool.0.closing.borrow() {
                    idle.push(client);
                }
            }
        }
    }
}
