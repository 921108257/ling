//! TCP 服务装配：分片所有权属于引擎，阻塞执行任务数量受控。
//! TCP service assembly: the engine owns shards and blocking execution tasks are bounded.

mod accept;
mod batch;
mod config;
mod connection;
mod maintenance;
mod operations;
mod telemetry;
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
    engine: Arc<ling_engine::Engine>,
    /// 实例专属测试故障点；发行构建不包含。
    /// Instance-local test failpoint, absent from release builds.
    #[cfg(test)]
    panic_next_connection: bool,
    /// 实例专属指标监听故障注入；Instance-local metrics-listener fault injection.
    #[cfg(test)]
    fail_metrics_accept: bool,
    /// 测试只读观察本实例的接入计数；Tests observe this instance's accept counters only.
    #[cfg(test)]
    observe_runtime: Option<tokio::sync::oneshot::Sender<Arc<operations::Runtime>>>,
}

impl Server {
    /// 校验配置后创建引擎并绑定监听端口；配置失败不接入请求。
    /// Validates configuration, creates the engine, and binds the listener; invalid settings accept no requests.
    pub async fn bind(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let tls = make_tls(&config)?;
        let engine_config = config.engine_config()?;
        let events = Arc::new(ling_events::EventStore::open(
            config.event_config()?,
            engine_config.databases,
            engine_config.resolved_shards(),
        )?);
        events.set_metrics_enabled(config.metrics.enabled)?;
        let engine = Arc::new(ling_engine::Engine::with_events(
            engine_config,
            rand::random(),
            Arc::new(ling_engine::SystemClock),
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
            #[cfg(test)]
            panic_next_connection: false,
            #[cfg(test)]
            fail_metrics_accept: false,
            #[cfg(test)]
            observe_runtime: None,
        })
    }

    /// 使用已有监听器，适合进程管理器和临时端口测试；仍完整校验配置。
    /// Uses an existing listener for supervisors and ephemeral-port tests; still validates all configuration.
    pub fn from_listener(
        listener: TcpListener,
        config: Config,
    ) -> Result<Self, ling_types::DbError> {
        config.validate()?;
        let tls = make_tls(&config)?;
        let engine_config = config.engine_config()?;
        let events = Arc::new(ling_events::EventStore::open(
            config.event_config()?,
            engine_config.databases,
            engine_config.resolved_shards(),
        )?);
        events.set_metrics_enabled(config.metrics.enabled)?;
        let engine = Arc::new(ling_engine::Engine::with_events(
            engine_config,
            rand::random(),
            Arc::new(ling_engine::SystemClock),
            Some(events),
        )?);
        Ok(Self {
            listener,
            tls,
            config: Arc::new(config),
            engine,
            #[cfg(test)]
            panic_next_connection: false,
            #[cfg(test)]
            fail_metrics_accept: false,
            #[cfg(test)]
            observe_runtime: None,
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
        // 每份许可保守覆盖一帧编码与两份 max_decode_bytes（包括 Batch 参数副本），合计至多 CPU 并行度份；
        // 固定头和 8 KiB 读块仍由连接数约束。许可只在有效帧头后取得。
        // Each permit reserves one frame plus twice max_decode_bytes (including Batch argument copies), up to CPU parallelism;
        // fixed headers and 8 KiB read chunks remain connection-bounded. Acquire only after a valid header.
        let read_budget = Arc::new(Semaphore::new(slots.available_permits()));
        let (closing, receiver) = watch::channel(false);
        let runtime = Arc::new(operations::Runtime::new(self.config.clone()));
        #[cfg(test)]
        let mut panic_next_connection = self.panic_next_connection;
        #[cfg(test)]
        {
            runtime.fail_metrics_accept.store(
                self.fail_metrics_accept,
                std::sync::atomic::Ordering::Relaxed,
            );
            if let Some(observer) = self.observe_runtime {
                let _ = observer.send(runtime.clone());
            }
        }
        for warning in config::exposure_warnings(&self.config) {
            runtime.log("warn", warning);
        }
        // 仅 Linux 暴露 /proc；读取或解析失败时静默跳过，自检不影响启动。
        // Only Linux exposes /proc; read or parse failures are skipped silently so the check never affects startup.
        #[cfg(target_os = "linux")]
        {
            if let Some(warning) = std::fs::read_to_string("/proc/self/limits")
                .ok()
                .and_then(|text| config::open_files_soft_limit(&text))
                .and_then(|limit| {
                    config::descriptor_limit_warning(limit, self.config.server.max_connections)
                })
            {
                runtime.log("warn", &warning);
            }
        }
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
                "ling {} listening {}: {} shards/database; events {}",
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
        // 接入退避状态跨循环保留，成功接受后复位。
        // Accept backoff state persists across iterations and resets after a successful accept.
        let mut backoff = None;
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
                    // 连接任务 panic 只终止该连接，计数并限频告警；其他连接与监听不受影响。
                    // A connection task panic ends only that connection with a counted, rate-limited warning; other connections and the listener are unaffected.
                    if result.is_err() {
                        runtime.accept.panicked(std::time::Instant::now(), |m| runtime.log("error", m));
                    }
                }
                accepted = accept::recover(
                    || self.listener.accept(),
                    &mut backoff,
                    |error| runtime.accept.failed(false, error, std::time::Instant::now(), |m| runtime.log("warn", m)),
                ) => {
                    let (stream, _) = match accepted {
                        Ok(value) => value,
                        Err(e) => {
                            runtime.accept.failed(false, &e, std::time::Instant::now(), |m| runtime.log("error", m));
                            break Err(e);
                        }
                    };
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                        drop(stream);
                        runtime.accept.rejected(std::time::Instant::now(), |m| runtime.log("warn", m));
                        continue;
                    };
                    let config = self.config.clone(); let receiver = receiver.clone(); let engine = self.engine.clone(); let slots = slots.clone();
                    let tls = self.tls.clone();
                    let runtime = runtime.clone();
                    let read_budget = read_budget.clone();
                    #[cfg(test)]
                    let panic_connection = std::mem::take(&mut panic_next_connection);
                    connections.spawn(async move {
                        let _permit = permit;
                        #[cfg(test)]
                        assert!(!panic_connection, "failpoint: connection task panic");
                        let stream: ling_network::Stream = if let Some(tls) = tls {
                            let mut closing = receiver.clone();
                            let handshake = tokio::time::timeout(Duration::from_millis(config.server.shutdown_timeout_ms), tls.accept(stream));
                            tokio::select! {
                                _ = closing.changed() => return,
                                result = handshake => match result { Ok(Ok(s)) => Box::new(s), _ => return },
                            }
                        } else { Box::new(stream) };
                        connection::serve(stream, &config, &engine, slots, read_budget, receiver, runtime).await;
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
fn make_tls(config: &Config) -> Result<Option<tokio_rustls::TlsAcceptor>, ling_types::DbError> {
    if !config.tls.enabled {
        return Ok(None);
    }
    ling_network::server_tls(
        &config.tls.cert_file,
        &config.tls.key_file,
        (!config.tls.ca_file.as_os_str().is_empty()).then_some(config.tls.ca_file.as_path()),
        &config.tls.min_version,
    )
    .map(Some)
}

#[cfg(test)]
mod failure_policy_tests {
    use super::*;

    /// 分片锁中毒属不可恢复故障：`run` 在约一秒内返回带稳定错误码的错误，而不是长期返回 ERR_BUSY。
    /// A poisoned shard lock is unrecoverable: `run` returns an error carrying a stable code within about a second instead of answering ERR_BUSY indefinitely.
    #[tokio::test]
    async fn poisoned_engine_stops_the_server() {
        let mut config = Config::default();
        config.events.enabled = false;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server::from_listener(listener, config).unwrap();
        server.engine.poison_shard_for_test();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.run(std::future::pending::<()>()),
        )
        .await
        .expect("server kept running after an unrecoverable poison");
        let error = result.unwrap_err();
        assert!(error.to_string().contains("ERR_BUSY"), "{error}");
    }

    /// 连接任务 panic 只终止该连接：出事连接被关闭，随后的新连接仍能握手并执行 PING，服务可正常关闭。
    /// A connection task panic ends only that connection: it is closed, a later connection can still handshake and PING, and the server shuts down cleanly.
    #[tokio::test]
    async fn connection_panic_leaves_other_connections_alive() {
        use ling_protocol::{Body, Frame, Limits, Request, read_frame, write_frame};
        use tokio::net::TcpStream;

        let mut config = Config::default();
        config.events.enabled = false;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut server = Server::from_listener(listener, config).unwrap();
        server.panic_next_connection = true;
        let address = server.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async move {
            let _ = stopped.await;
        }));

        let mut doomed = TcpStream::connect(address).await.unwrap();
        let closed = tokio::time::timeout(
            Duration::from_secs(2),
            read_frame(&mut doomed, Limits::default()),
        )
        .await
        .expect("panicked connection was not closed");
        assert!(matches!(closed, Ok(None) | Err(_)), "{closed:?}");
        let mut healthy = TcpStream::connect(address).await.unwrap();
        for (id, name, args) in [
            (1u64, "HELLO", vec![ling_types::Value::UInt(1)]),
            (2, "PING", vec![]),
        ] {
            let frame = Frame {
                request_id: id,
                body: Body::Request(Request::new(name, args)),
            };
            write_frame(&mut healthy, &frame, Limits::default())
                .await
                .unwrap();
            let reply = read_frame(&mut healthy, Limits::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply.request_id, id);
            assert!(
                matches!(reply.body, Body::Response(Ok(_))),
                "{:?}",
                reply.body
            );
        }
        let _ = stop.send(());
        task.await.unwrap().unwrap();
    }

    /// 真实连接占满上限后，额外连接全部关闭并逐次计数，一分钟内只告警一次，已有连接仍可用。
    /// Once real connections fill the limit, excess connections close and are counted individually, warning only once per minute while the accepted connection remains usable.
    #[tokio::test]
    async fn excess_connections_are_counted_with_one_warning() {
        use ling_client::{Client, Options, Value};
        use tokio::{io::AsyncReadExt, net::TcpStream};

        let mut config = Config::default();
        config.events.enabled = false;
        config.server.max_connections = 1;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut server = Server::from_listener(listener, config).unwrap();
        let address = server.local_addr().unwrap();
        let (observe, observed) = tokio::sync::oneshot::channel();
        server.observe_runtime = Some(observe);
        let task = tokio::spawn(server.run(std::future::pending()));
        let runtime = observed.await.unwrap();
        let mut healthy = Client::connect(address, None, 0, Options::default())
            .await
            .unwrap();
        let mut excess = Vec::new();
        for _ in 0..4 {
            excess.push(TcpStream::connect(address).await.unwrap());
        }
        for mut stream in excess {
            let mut byte = [0];
            let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
                .await
                .unwrap();
            assert!(matches!(result, Ok(0) | Err(_)), "{result:?}");
        }
        assert_eq!(runtime.accept.observed(), (4, 1, 0));
        assert_eq!(
            healthy.execute("PING", vec![]).await.unwrap(),
            Value::String("PONG".into())
        );
        healthy.close().await.unwrap();
        runtime.stop.send_replace(true);
        task.await.unwrap().unwrap();
    }

    /// 指标监听器连续致命错误只影响指标端点，业务连接在重试前后仍可 PING，关闭仍及时完成。
    /// Repeated fatal metrics-listener errors stay local: business PING succeeds across retries and shutdown still completes promptly.
    #[tokio::test]
    async fn persistent_metrics_accept_errors_leave_data_listener_alive() {
        use ling_client::{Client, Options, Value};

        let mut config = Config::default();
        config.events.enabled = false;
        config.metrics.enabled = true;
        let metrics = TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.metrics.port = metrics.local_addr().unwrap().port();
        drop(metrics);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut server = Server::from_listener(listener, config).unwrap();
        let address = server.local_addr().unwrap();
        let (observe, observed) = tokio::sync::oneshot::channel();
        server.observe_runtime = Some(observe);
        server.fail_metrics_accept = true;
        let task = tokio::spawn(server.run(std::future::pending()));
        let runtime = observed.await.unwrap();
        let mut healthy = Client::connect(address, None, 0, Options::default())
            .await
            .unwrap();
        for failures in [1, 3] {
            tokio::time::timeout(Duration::from_secs(5), async {
                while runtime.accept.observed().2 < failures {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("metrics accept retries did not continue");
            assert_eq!(
                healthy.execute("PING", vec![]).await.unwrap(),
                Value::String("PONG".into())
            );
        }
        healthy.close().await.unwrap();
        runtime.stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
