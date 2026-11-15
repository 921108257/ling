//! TCP 服务装配：分片所有权属于引擎，阻塞执行任务数量受控。
//! TCP service assembly: the engine owns shards and blocking execution tasks are bounded.

mod batch;
mod config;
mod connection;
mod maintenance;
mod operations;
pub use config::{
    AuthConfig, Config, DatabaseConfig, EngineConfig, EventJournalConfig, EventsConfig,
    EvictionConfig, LogConfig, MetricsConfig, ProtocolConfig, ServerConfig, TlsConfig, TtlConfig,
};
use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{Semaphore, watch},
    task::JoinSet,
};

/// 拥有监听器、不可变配置和进程内数据引擎。
/// Owns the listener, immutable configuration, and in-process data engine.
pub struct Server {
    /// 本实例唯一监听器。
    /// Listener owned exclusively by this instance.
    listener: TcpListener,
    /// 监听前校验的 TLS 身份；TLS identity validated before listening.
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// 所有连接只读共享的启动配置。
    /// Startup configuration shared read-only by connections.
    config: Arc<Config>,
    /// 分片锁由引擎协调，网络层只持有共享句柄。
    /// The engine coordinates shard locks; networking holds only a shared handle.
    engine: Arc<linger_engine::Engine>,
}

impl Server {
    /// 校验配置后创建引擎并绑定监听端口；配置失败不接入请求。
    /// Validates configuration, creates the engine, and binds the listener; invalid settings accept no requests.
    pub async fn bind(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let tls = make_tls(&config)?;
        let engine_config = config.engine_config()?;
        let events = Arc::new(linger_events::EventStore::open(
            config.event_config()?,
            engine_config.databases,
            engine_config.resolved_shards(),
        )?);
        let engine = Arc::new(linger_engine::Engine::with_events(
            engine_config,
            rand::random(),
            Arc::new(linger_engine::SystemClock),
            Some(events),
        )?);
        let socket = if config.server.host.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.set_reuseaddr(true)?;
        socket.bind(SocketAddr::new(config.server.host, config.server.port))?;
        let listener = socket.listen(config.server.backlog)?;
        Ok(Self {
            listener,
            tls,
            config: Arc::new(config),
            engine,
        })
    }

    /// 使用已有监听器，适合进程管理器和临时端口测试；仍完整校验配置。
    /// Uses an existing listener for supervisors and ephemeral-port tests; still validates all configuration.
    pub fn from_listener(
        listener: TcpListener,
        config: Config,
    ) -> Result<Self, linger_types::DbError> {
        config.validate()?;
        let tls = make_tls(&config)?;
        let engine_config = config.engine_config()?;
        let events = Arc::new(linger_events::EventStore::open(
            config.event_config()?,
            engine_config.databases,
            engine_config.resolved_shards(),
        )?);
        let engine = Arc::new(linger_engine::Engine::with_events(
            engine_config,
            rand::random(),
            Arc::new(linger_engine::SystemClock),
            Some(events),
        )?);
        Ok(Self {
            listener,
            tls,
            config: Arc::new(config),
            engine,
        })
    }
    /// 返回实际监听地址，支持由操作系统选择测试端口。
    /// Returns the actual listening address, including OS-assigned test ports.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 读取已解析的固定分片数供启动日志使用。
    /// Reads the resolved fixed shard count for startup diagnostics.
    pub fn shard_count(&self) -> u32 {
        self.engine.shard_count()
    }

    /// 运行有连接数上限的接入循环；关闭时排空任务，超时后取消剩余连接。
    /// Runs a connection-limited accept loop; shutdown drains tasks and cancels remaining connections after its deadline.
    /// 已开始的阻塞命令仍会完成；关闭期限不能强制回滚执行中的修改。
    /// Started blocking commands still finish; the shutdown deadline cannot roll back in-progress mutations.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.server.max_connections));
        // 在提交阻塞任务前取得许可，任务和等待者分别受 CPU 并行度与连接数约束。
        // Acquire permits before spawning; CPU parallelism bounds tasks and connection count bounds waiters.
        let slots = Arc::new(Semaphore::new(
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(self.config.server.max_connections),
        ));
        let (closing, receiver) = watch::channel(false);
        let runtime = Arc::new(operations::Runtime::new(self.config.clone()));
        let mut background = JoinSet::new();
        background.spawn(maintenance::run(
            self.engine.clone(),
            slots.clone(),
            self.config.ttl.maintenance_budget,
            receiver.clone(),
            Some(runtime.clone()),
        ));
        let mut requested = runtime.stop.subscribe();
        runtime.log(
            "info",
            &format!(
                "linger-server {} listening {}: {} shards/database; events {}",
                env!("CARGO_PKG_VERSION"),
                self.listener.local_addr()?,
                self.engine.shard_count(),
                if self.config.events.enabled {
                    self.config.events.mode.as_str()
                } else {
                    "disabled"
                }
            ),
        );
        if self.config.metrics.enabled {
            let listener = TcpListener::bind(SocketAddr::new(
                self.config.metrics.host,
                self.config.metrics.port,
            ))
            .await?;
            background.spawn(operations::serve_metrics(
                listener,
                runtime.clone(),
                self.engine.clone(),
                receiver.clone(),
            ));
        }
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        let result = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break Ok(()),
                _ = requested.changed() => break Ok(()),
                Some(result) = background.join_next() => {
                    break match result {
                        Ok(Err(error)) => Err(error),
                        _ => Err(io::Error::other("TTL maintenance stopped unexpectedly")),
                    };
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(e) = result { break Err(io::Error::other(format!("Connection task failed: {e}"))); }
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted { Ok(value) => value, Err(e) => break Err(e) };
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else { drop(stream); continue; };
                    let config = self.config.clone(); let receiver = receiver.clone(); let engine = self.engine.clone(); let slots = slots.clone();
                    let tls = self.tls.clone();
                    let runtime = runtime.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        let stream: linger_network::Stream = if let Some(tls) = tls {
                            let mut closing = receiver.clone();
                            let handshake = tokio::time::timeout(Duration::from_millis(config.server.shutdown_timeout_ms), tls.accept(stream));
                            tokio::select! {
                                _ = closing.changed() => return,
                                result = handshake => match result { Ok(Ok(s)) => Box::new(s), _ => return },
                            }
                        } else { Box::new(stream) };
                        connection::serve(stream, &config, &engine, slots, receiver, runtime).await;
                    });
                }
            }
        };
        runtime.log("info", "server draining");
        closing.send_replace(true);
        let drain = async {
            while connections.join_next().await.is_some() {}
            while background.join_next().await.is_some() {}
        };
        if tokio::time::timeout(
            Duration::from_millis(self.config.server.shutdown_timeout_ms),
            drain,
        )
        .await
        .is_err()
        {
            connections.abort_all();
            background.abort_all();
            while connections.join_next().await.is_some() {}
            while background.join_next().await.is_some() {}
        }
        runtime.log("info", "server stopped");
        result
    }
}

/// 在业务监听前拒绝不可用身份；Rejects unavailable identities before accepting business traffic.
fn make_tls(config: &Config) -> Result<Option<tokio_rustls::TlsAcceptor>, linger_types::DbError> {
    if !config.tls.enabled {
        return Ok(None);
    }
    linger_network::server_tls(
        &config.tls.cert_file,
        &config.tls.key_file,
        (!config.tls.ca_file.as_os_str().is_empty()).then_some(config.tls.ca_file.as_path()),
        &config.tls.min_version,
    )
    .map(Some)
}
