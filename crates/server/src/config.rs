use linger_protocol::Limits;
use linger_types::{DbError, ErrorCode};
use serde::Deserialize;
use std::{net::IpAddr, path::Path, time::Duration};

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub database: DatabaseConfig,
    pub protocol: ProtocolConfig,
    /// 数据引擎限额；Data-engine resource limits.
    pub engine: EngineConfig,
}

/// 单分片阶段接受的引擎配置，其他分片数量暂不开放。
/// Engine settings supported by the single-shard stage; other shard counts are not yet supported.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// 0 自动解析为 1；0 resolves automatically to 1.
    pub shard_count: u32,
    /// 全实例受管字节；Instance-wide managed bytes.
    pub max_memory_bytes: usize,
    /// 每库活跃 Key 数；Active keys per database.
    pub max_keys: usize,
    /// Key 的 UTF-8 字节数；UTF-8 bytes per key.
    pub max_key_bytes: usize,
    /// 单个完整编码 Value 上限；Complete encoded bytes per Value.
    pub max_value_bytes: usize,
    /// 业务容器深度；Business-container depth.
    pub max_nest_depth: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            shard_count: 0,
            max_memory_bytes: 0,
            max_keys: 0,
            max_key_bytes: 1024,
            max_value_bytes: 64 * 1024 * 1024,
            max_nest_depth: 8,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub backlog: u32,
    pub max_connections: usize,
    pub idle_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub shutdown_timeout_ms: u64,
    pub max_inflight_per_connection: usize,
    pub write_buffer_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".parse().unwrap(),
            port: 6380,
            backlog: 1024,
            max_connections: 10000,
            idle_timeout_ms: 300000,
            request_timeout_ms: 30000,
            max_request_bytes: 64 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
            shutdown_timeout_ms: 5000,
            max_inflight_per_connection: 1024,
            write_buffer_bytes: 8 * 1024 * 1024,
        }
    }
}

// No Debug implementation: neither this structure nor parse errors may reveal credentials.
#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub enabled: bool,
    pub password: String,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub databases: u32,
    pub default_db: u16,
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            databases: 16,
            default_db: 0,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProtocolConfig {
    pub max_frame_bytes: usize,
    pub max_batch_size: usize,
    pub pipeline_enabled: bool,
}
impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 128 * 1024 * 1024,
            max_batch_size: 1000,
            pipeline_enabled: true,
        }
    }
}

fn invalid(message: &'static str) -> DbError {
    DbError::new(ErrorCode::ConfigInvalid, message)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, DbError> {
        let source =
            std::fs::read_to_string(path).map_err(|_| invalid("Cannot read configuration file"))?;
        // toml::Error includes source excerpts, potentially including auth.password.
        let config: Self = toml::from_str(&source)
            .map_err(|_| invalid("Invalid configuration syntax, field or type"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), DbError> {
        if self.engine.shard_count > 1 {
            return Err(invalid("Stage 2 supports a single shard"));
        }
        self.engine_config().validate()?;
        let s = &self.server;
        if s.port == 0
            || s.backlog == 0
            || s.max_connections == 0
            || s.max_connections > tokio::sync::Semaphore::MAX_PERMITS
            || s.max_inflight_per_connection == 0
            || s.write_buffer_bytes == 0
            || s.max_request_bytes < 1024
            || s.max_response_bytes < 1024
            || s.shutdown_timeout_ms == 0
        {
            return Err(invalid("Invalid server limits"));
        }
        for millis in [
            s.idle_timeout_ms,
            s.request_timeout_ms,
            s.shutdown_timeout_ms,
        ] {
            if std::time::Instant::now()
                .checked_add(Duration::from_millis(millis))
                .is_none()
            {
                return Err(invalid("Timeout exceeds runtime range"));
            }
        }
        if !(1..=65536).contains(&self.database.databases)
            || u32::from(self.database.default_db) >= self.database.databases
        {
            return Err(invalid("Invalid database range"));
        }
        if self.auth.enabled == self.auth.password.is_empty() {
            return Err(invalid(
                "Authentication requires a nonempty password only when enabled",
            ));
        }
        let max_payload = s.max_request_bytes.max(s.max_response_bytes);
        let max_frame = self.protocol.max_frame_bytes;
        if max_payload > u32::MAX as usize
            || max_payload.checked_add(18).is_none_or(|n| n > max_frame)
            || max_frame < 18
            || (max_frame as u64) > u64::from(u32::MAX) + 18
            || self.protocol.max_batch_size == 0
            || self.protocol.max_batch_size > u32::MAX as usize
        {
            return Err(invalid("Invalid protocol byte or Batch limits"));
        }
        Ok(())
    }

    pub fn limits(&self) -> Limits {
        Limits {
            max_frame_bytes: self.protocol.max_frame_bytes,
            max_request_bytes: self.server.max_request_bytes,
            max_response_bytes: self.server.max_response_bytes,
            max_batch_size: self.protocol.max_batch_size,
            value: self.engine_config().value,
            ..Limits::default()
        }
    }

    /// 将配置转换为不依赖网络的引擎配置。
    /// Converts settings into the network-independent engine configuration.
    pub fn engine_config(&self) -> linger_engine::Config {
        linger_engine::Config {
            databases: self.database.databases,
            max_memory_bytes: self.engine.max_memory_bytes,
            max_keys: self.engine.max_keys,
            value: linger_types::ValueLimits {
                max_bytes: self.engine.max_value_bytes,
                max_depth: self.engine.max_nest_depth,
                max_key_bytes: self.engine.max_key_bytes,
            },
        }
    }
}
