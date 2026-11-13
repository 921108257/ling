//! Network service only; the stage-1 build has no data engine or event source.

mod config;
mod connection;
pub use config::{AuthConfig, Config, DatabaseConfig, EngineConfig, ProtocolConfig, ServerConfig};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{Semaphore, watch},
    task::JoinSet,
};

pub struct Server {
    listener: TcpListener,
    config: Arc<Config>,
    /// 唯一执行权覆盖所有命名空间，临界区中没有 await。
    /// Exclusive ownership covers all namespaces; critical sections contain no await.
    engine: Arc<Mutex<linger_engine::Engine>>,
}

impl Server {
    pub async fn bind(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        let engine = Arc::new(Mutex::new(linger_engine::Engine::new(
            config.engine_config(),
            rand::random(),
        )?));
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
            config: Arc::new(config),
            engine,
        })
    }

    /// Useful for supervisors and tests that own an already bound listener.
    pub fn from_listener(
        listener: TcpListener,
        config: Config,
    ) -> Result<Self, linger_types::DbError> {
        config.validate()?;
        let engine = Arc::new(Mutex::new(linger_engine::Engine::new(
            config.engine_config(),
            rand::random(),
        )?));
        Ok(Self {
            listener,
            config: Arc::new(config),
            engine,
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.server.max_connections));
        let (closing, receiver) = watch::channel(false);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        let result = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break Ok(()),
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(e) = result { break Err(io::Error::other(format!("Connection task failed: {e}"))); }
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted { Ok(value) => value, Err(e) => break Err(e) };
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else { drop(stream); continue; };
                    let config = self.config.clone(); let receiver = receiver.clone(); let engine = self.engine.clone();
                    connections.spawn(async move { let _permit = permit; connection::serve(stream, &config, &engine, receiver).await; });
                }
            }
        };
        closing.send_replace(true);
        let drain = async { while connections.join_next().await.is_some() {} };
        if tokio::time::timeout(
            Duration::from_millis(self.config.server.shutdown_timeout_ms),
            drain,
        )
        .await
        .is_err()
        {
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
        result
    }
}
