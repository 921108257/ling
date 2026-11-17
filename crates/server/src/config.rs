//! 严格解析启动配置；未知字段和不支持的能力均明确拒绝。
//! Strict startup configuration parsing; unknown fields and unsupported features are rejected.

use ling_protocol::Limits;
use ling_types::{DbError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, path::Path, time::Duration};

/// 当前阶段支持的配置全集；默认值在加载后仍经过验证。
/// Complete supported configuration for this stage; defaults are validated after loading.
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 监听和连接预算。
    /// Listening and connection budgets.
    pub server: ServerConfig,
    /// 实例级认证设置。
    /// Instance-wide authentication settings.
    pub auth: AuthConfig,
    /// TLS 身份与信任；TLS identity and trust.
    pub tls: TlsConfig,
    /// 数据库命名空间设置。
    /// Database namespace settings.
    pub database: DatabaseConfig,
    /// 帧和解码入口设置。
    /// Frame and decoding settings.
    pub protocol: ProtocolConfig,
    /// 数据引擎限额；Data-engine resource limits.
    pub engine: EngineConfig,
    /// 内存和 Key 预算达到上限时的候选选择。
    /// Candidate selection when memory or key budgets are exhausted.
    pub eviction: EvictionConfig,
    /// 毫秒精度与每轮到期维护预算。
    /// Millisecond precision and per-round expiry maintenance budget.
    pub ttl: TtlConfig,
    /// 事件缓冲与消费策略；Event buffering and consumption policy.
    pub events: EventsConfig,
    /// Reliable 磁盘预算；Reliable disk budget.
    pub event_journal: EventJournalConfig,
    /// 日志输出策略；Log output policy.
    pub log: LogConfig,
    /// Prometheus 监听策略；Prometheus listener policy.
    pub metrics: MetricsConfig,
    /// 重载来源，不属于公开配置字段；Reload source, not a public configuration field.
    #[serde(skip)]
    pub source: Option<std::path::PathBuf>,
}

/// 日志只包含稳定操作信息，不包含参数与凭据。
/// Logs contain stable operational facts, never arguments or credentials.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// trace/debug/info/warn/error；Allowed severity level.
    pub level: String,
    /// text 或 json；Text or JSON.
    pub format: String,
    /// stdout 或 stderr；Standard output or standard error.
    pub output: String,
}
impl Default for LogConfig {
    /// 默认信息级文本输出至 stdout；Defaults to info text on stdout.
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "text".into(),
            output: "stdout".into(),
        }
    }
}
/// 单独有界的指标端点，不暴露业务数据。
/// Separate bounded metrics endpoint without business data.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// 默认关闭；Disabled by default.
    pub enabled: bool,
    /// 默认回环地址；Loopback by default.
    pub host: IpAddr,
    /// 非零端口；Nonzero port.
    pub port: u16,
    /// 固定 HTTP GET 路径；Fixed HTTP GET path.
    pub path: String,
}
impl Default for MetricsConfig {
    /// 默认回环 9090 的 /metrics；Defaults to loopback port 9090 and /metrics.
    fn default() -> Self {
        Self {
            enabled: false,
            host: std::net::Ipv4Addr::LOCALHOST.into(),
            port: 9090,
            path: "/metrics".into(),
        }
    }
}

/// 服务端 TLS；证书路径相对配置文件解析。
/// Server TLS with identity paths resolved relative to the config file.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// 启用时不再接受明文；Rejects plaintext when enabled.
    pub enabled: bool,
    /// PEM 证书链；PEM certificate chain.
    pub cert_file: std::path::PathBuf,
    /// PEM 私钥；PEM private key.
    pub key_file: std::path::PathBuf,
    /// 非空要求客户端证书；Nonempty requires client certificates.
    pub ca_file: std::path::PathBuf,
    /// tls1.2 或 tls1.3；Either tls1.2 or tls1.3.
    pub min_version: String,
}
impl Default for TlsConfig {
    /// 默认关闭，最低 TLS 1.2；Disabled by default, minimum TLS 1.2.
    fn default() -> Self {
        Self {
            enabled: false,
            cert_file: Default::default(),
            key_file: Default::default(),
            ca_file: Default::default(),
            min_version: "tls1.2".into(),
        }
    }
}

/// 启动固定的事件与消费策略；Event and consumer policy fixed at startup.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    /// 是否生成事件；Whether events are generated.
    pub enabled: bool,
    /// fast 或 reliable；Either fast or reliable.
    pub mode: String,
    /// 统一保留天数；Global retention in days.
    pub retention_days: u64,
    /// 事件与消费元数据内存预算；Memory budget for events and consumption metadata.
    pub buffer_bytes: usize,
    /// 每组交付窗口；Delivery window per group.
    pub max_pending_events: usize,
    /// 命名及临时组上限；Named and temporary group cap.
    pub max_groups: usize,
    /// 消费租约毫秒；Consumer lease milliseconds.
    pub consumer_timeout_ms: u64,
    /// 最小重投间隔毫秒；Minimum redelivery interval in milliseconds.
    pub retry_interval_ms: u64,
    /// 提交或租约变化唤醒订阅；false 保留每 10 ms 检查的兼容路径。
    /// Wakes subscriptions on commits and lease changes; false retains the legacy 10 ms polling path.
    pub notification_enabled: bool,
}
impl Default for EventsConfig {
    /// 默认有界 Fast 与七天保留；Defaults to bounded Fast with seven-day retention.
    fn default() -> Self {
        let c = ling_events::Config::default();
        Self {
            enabled: c.enabled,
            mode: "fast".into(),
            retention_days: 7,
            buffer_bytes: c.buffer_bytes,
            max_pending_events: c.max_pending,
            max_groups: c.max_groups,
            consumer_timeout_ms: c.consumer_timeout_ms,
            retry_interval_ms: c.retry_interval_ms,
            notification_enabled: true,
        }
    }
}
/// 仅 Reliable 打开的目录与容量；Directory and capacity opened only in Reliable mode.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventJournalConfig {
    /// 相对配置文件目录解析；Resolved relative to the configuration file.
    pub dir: std::path::PathBuf,
    /// 含替换预留的总物理容量；Total physical capacity including replacement reserve.
    pub max_bytes: usize,
    /// 允许的主动聚合等待预算，当前逐事务同步；Allowed aggregation wait budget; currently syncs each transaction.
    pub group_commit_ms: u64,
}
impl Default for EventJournalConfig {
    /// 默认空目录与 1 GiB 容量；Defaults to no directory and one GiB capacity.
    fn default() -> Self {
        Self {
            dir: Default::default(),
            max_bytes: 1024 * 1024 * 1024,
            group_commit_ms: 2,
        }
    }
}

/// 精度固定为毫秒，维护工作量不改变逻辑到期边界。
/// Precision is fixed at milliseconds; maintenance work limits never alter logical expiry boundaries.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtlConfig {
    /// 固定为 1，不提供近似过期精度开关。
    /// Fixed at one, with no approximate-expiry precision switch.
    pub precision_ms: u64,
    /// 每轮最多检查的到期候选数，必须为正。
    /// Maximum due candidates checked per round; must be positive.
    pub maintenance_budget: usize,
}
impl Default for TtlConfig {
    /// 默认每轮 1024 个候选，不承诺回收延迟上限。
    /// Defaults to 1024 candidates per round without promising a reclamation latency bound.
    fn default() -> Self {
        Self {
            precision_ms: 1,
            maintenance_budget: 1024,
        }
    }
}

/// 启动时确定的固定分片和业务资源配置。
/// Fixed sharding and business resource settings resolved at startup.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// 0 按系统可用并行度解析；Zero resolves to system available parallelism.
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
    /// 默认自动固定分片与标准业务值限额。
    /// Defaults to automatic fixed sharding and standard business-value limits.
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

/// 淘汰策略与采样数量；不接受未知策略或零采样数。
/// Eviction policy and sample count; unknown policies and zero samples are rejected.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvictionConfig {
    /// 精确小写策略名；Exact lowercase policy name.
    pub policy: String,
    /// LRU / LFU 候选数量；LRU / LFU candidate count.
    pub sample_size: usize,
}

impl Default for EvictionConfig {
    /// 默认不淘汰，采样数量为 5。
    /// Defaults to no eviction and five samples.
    fn default() -> Self {
        Self {
            policy: "noeviction".into(),
            sample_size: 5,
        }
    }
}

/// TCP 监听、连接数量、超时和单连接缓冲限制。
/// TCP listening, connection count, timeout, and per-connection buffer limits.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// 监听 IP，默认回环地址。
    /// Listening IP, loopback by default.
    pub host: IpAddr,
    /// 非零监听端口。
    /// Nonzero listening port.
    pub port: u16,
    /// 操作系统接入队列长度。
    /// OS accept backlog length.
    pub backlog: u32,
    /// 同时存活的连接上限。
    /// Maximum simultaneous live connections.
    pub max_connections: usize,
    /// 帧间空闲超时，零表示禁用。
    /// Idle timeout between frames; zero disables it.
    pub idle_timeout_ms: u64,
    /// 半帧接收及响应发送期限，零禁用接收期限。
    /// Partial-frame read and response-write timeout; zero disables the read deadline.
    pub request_timeout_ms: u64,
    /// 请求负载字节上限，不含帧头。
    /// Request payload cap excluding the header.
    pub max_request_bytes: usize,
    /// 响应负载字节上限，在引擎发布前检查。
    /// Response payload cap checked before engine publication.
    pub max_response_bytes: usize,
    /// 关闭排空任务的最长等待毫秒数。
    /// Maximum milliseconds to drain tasks during shutdown.
    pub shutdown_timeout_ms: u64,
    /// 声明的每连接在途上限；当前实际串行执行。
    /// Configured per-connection inflight cap; execution is currently serial.
    pub max_inflight_per_connection: usize,
    /// 单次有界写出的分块字节数。
    /// Chunk bytes used for bounded writes.
    pub write_buffer_bytes: usize,
}

impl Default for ServerConfig {
    /// 默认回环监听与有界连接、超时和缓冲。
    /// Defaults to loopback listening with bounded connections, deadlines, and buffers.
    fn default() -> Self {
        Self {
            host: std::net::Ipv4Addr::LOCALHOST.into(),
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

// 不实现 Debug：结构展示与解析错误均不得泄露凭据。
// No Debug implementation: neither this structure nor parse errors may reveal credentials.
/// 实例级可选密码认证；不派生 Debug，避免泄露凭据。
/// Optional instance password authentication; no derived Debug to avoid credential exposure.
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// 是否要求客户端认证。
    /// Whether clients must authenticate.
    pub enabled: bool,
    /// 启用时必须非空；日志和错误不得包含其内容。
    /// Must be nonempty when enabled; never included in logs or errors.
    pub password: String,
}

/// 数据库命名空间数量及新连接默认选择。
/// Database namespace count and default selection for new connections.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// 命名空间数量，范围 1..=65536。
    /// Namespace count, from 1 through 65536.
    pub databases: u32,
    /// 新连接初始库号，必须小于数据库数量。
    /// Initial database number, smaller than the namespace count.
    pub default_db: u16,
}
impl Default for DatabaseConfig {
    /// 默认 16 个隔离数据库，新连接选择 0。
    /// Defaults to 16 isolated databases with database zero for new connections.
    fn default() -> Self {
        Self {
            databases: 16,
            default_db: 0,
        }
    }
}

/// 协议帧与批量限制；批量仍须通过命令白名单及响应预算检查。
/// Frame and batch limits; batches still require command admission and response budgets.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProtocolConfig {
    /// 完整帧上限，必须容纳最大方向负载及帧头。
    /// Complete-frame cap, large enough for either payload plus its header.
    pub max_frame_bytes: usize,
    /// 批量编码最大子项数。
    /// Maximum items in a batch encoding.
    pub max_batch_size: usize,
    /// 保留的调度开关；当前仍逐连接串行处理。
    /// Reserved scheduling setting; each connection is currently processed serially.
    pub pipeline_enabled: bool,
}
impl Default for ProtocolConfig {
    /// 保留协议默认帧和 Batch 数量限制。
    /// Retains default frame and Batch item limits.
    fn default() -> Self {
        Self {
            max_frame_bytes: 128 * 1024 * 1024,
            max_batch_size: 1000,
            pipeline_enabled: true,
        }
    }
}

/// 生成不含配置源片段的安全错误。
/// Builds an error without configuration source excerpts.
fn invalid(message: &'static str) -> DbError {
    DbError::new(ErrorCode::ConfigInvalid, message)
}

/// 暴露面只生成启动告警，不替代配置校验或拒绝合法监听设置。
/// Exposure checks generate startup warnings without rejecting valid listener settings.
pub(crate) fn exposure_warnings(config: &Config) -> Vec<&'static str> {
    let mut warnings = Vec::new();
    if !config.server.host.is_loopback() && !config.auth.enabled {
        warnings.push("Business listener is not loopback and authentication is disabled");
    }
    if config.auth.enabled && !config.tls.enabled {
        warnings.push("Authentication is enabled without TLS; passwords travel in plaintext");
    }
    if config.metrics.enabled && !config.metrics.host.is_loopback() {
        warnings.push("Prometheus listener is not loopback and has no authentication or TLS");
    }
    warnings
}

/// 为监听器、指标端点、Journal 文件与日志预留的句柄余量，位于连接上限之外。
/// Descriptor headroom for listeners, the metrics endpoint, journal files and logs beyond the connection limit.
#[cfg(any(target_os = "linux", test))]
const DESCRIPTOR_RESERVE: u64 = 128;

/// 从 `/proc/self/limits` 文本取出 "Max open files" 的软上限；`unlimited` 记为 `u64::MAX`，无法解析时返回 None。
/// Extracts the soft "Max open files" limit from `/proc/self/limits` text; `unlimited` maps to `u64::MAX` and unparsable input yields None.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn open_files_soft_limit(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    match line.split_whitespace().nth(3)? {
        "unlimited" => Some(u64::MAX),
        value => value.parse().ok(),
    }
}

/// 软上限低于连接上限加预留量时给出告警；只告警，不拒绝启动。
/// Warns when the soft limit is below the connection limit plus reserve; warns only and never rejects startup.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn descriptor_limit_warning(soft_limit: u64, max_connections: usize) -> Option<String> {
    let needed = (max_connections as u64).saturating_add(DESCRIPTOR_RESERVE);
    (soft_limit < needed).then(|| {
        format!(
            "Open file limit {soft_limit} is below max_connections plus reserve {needed}; accept may fail with EMFILE"
        )
    })
}

impl Config {
    /// 加载 UTF-8 TOML 并校验；错误隐藏可能包含密码的原文。
    /// Loads UTF-8 TOML and validates it; errors omit source text that could contain passwords.
    pub fn load(path: &Path) -> Result<Self, DbError> {
        let absolute = std::path::absolute(path).map_err(|_| invalid("Invalid config path"))?;
        let path = absolute.as_path();
        let source =
            std::fs::read_to_string(path).map_err(|_| invalid("Cannot read configuration file"))?;
        // TOML 错误可能附带含 auth.password 的源片段，因此只返回安全说明。
        // TOML errors may include auth.password source excerpts, so return only a safe description.
        let mut config: Self = toml::from_str(&source)
            .map_err(|_| invalid("Invalid configuration syntax, field or type"))?;
        for item in [
            &mut config.event_journal.dir,
            &mut config.tls.cert_file,
            &mut config.tls.key_file,
            &mut config.tls.ca_file,
        ] {
            if !item.as_os_str().is_empty() && item.is_relative() {
                *item = path.parent().unwrap_or(Path::new(".")).join(&*item);
            }
        }
        config.source =
            Some(std::path::absolute(path).map_err(|_| invalid("Invalid config path"))?);
        config.validate()?;
        Ok(config)
    }

    /// 监听前验证范围与组合关系，不创建引擎或分配索引。
    /// Validates ranges and combinations before listening without allocating engine indexes.
    pub fn validate(&self) -> Result<(), DbError> {
        self.event_config()?.validate()?;
        if !matches!(
            self.log.level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) || !matches!(self.log.format.as_str(), "text" | "json")
            || !matches!(self.log.output.as_str(), "stdout" | "stderr")
        {
            return Err(invalid("Invalid log settings"));
        }
        if self.metrics.port == 0
            || !self.metrics.path.starts_with('/')
            || self.metrics.path.len() > 1024
            || self
                .metrics
                .path
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            || (self.metrics.enabled
                && self.metrics.port == self.server.port
                && (self.metrics.host == self.server.host
                    || self.metrics.host.is_unspecified()
                    || self.server.host.is_unspecified()))
        {
            return Err(invalid("Invalid metrics listener"));
        }
        if !matches!(self.tls.min_version.as_str(), "tls1.2" | "tls1.3")
            || (self.tls.enabled
                && (self.tls.cert_file.as_os_str().is_empty()
                    || self.tls.key_file.as_os_str().is_empty()))
            || (!self.tls.enabled
                && [&self.tls.cert_file, &self.tls.key_file, &self.tls.ca_file]
                    .iter()
                    .any(|p| !p.as_os_str().is_empty()))
        {
            return Err(invalid("Invalid TLS settings"));
        }
        if self.ttl.precision_ms != 1 || self.ttl.maintenance_budget == 0 {
            return Err(invalid("Invalid TTL precision or maintenance budget"));
        }
        self.eviction
            .policy
            .parse::<ling_engine::EvictionPolicy>()?;
        self.engine_config()?.validate()?;
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
        if self.events.enabled
            && (self.engine.max_key_bytes.saturating_add(768) > s.max_response_bytes
                || self
                    .engine
                    .max_key_bytes
                    .saturating_mul(2)
                    .saturating_add(256)
                    > self.events.buffer_bytes / 4)
        {
            return Err(invalid("Event key exceeds response or event budget"));
        }
        Ok(())
    }

    /// 将外部名称转换为已校验事件策略；Converts public names into validated event policy.
    pub fn event_config(&self) -> Result<ling_events::Config, DbError> {
        if !matches!(self.events.mode.as_str(), "fast" | "reliable")
            || self.event_journal.group_commit_ms > 1000
        {
            return Err(invalid("Invalid event mode or group commit interval"));
        }
        Ok(ling_events::Config {
            enabled: self.events.enabled,
            reliable: self.events.mode == "reliable",
            directory: self.event_journal.dir.clone(),
            max_bytes: self.event_journal.max_bytes,
            buffer_bytes: self.events.buffer_bytes,
            max_pending: self.events.max_pending_events,
            max_groups: self.events.max_groups,
            consumer_timeout_ms: self.events.consumer_timeout_ms,
            retry_interval_ms: self.events.retry_interval_ms,
            retention_ms: self
                .events
                .retention_days
                .checked_mul(86400000)
                .ok_or_else(|| invalid("Retention overflow"))?,
        })
    }

    /// 生成网络层使用的同一组配置限额。
    /// Builds the same configured limits used by the network layer.
    pub fn limits(&self) -> Limits {
        Limits {
            max_frame_bytes: self.protocol.max_frame_bytes,
            max_request_bytes: self.server.max_request_bytes,
            max_response_bytes: self.server.max_response_bytes,
            max_batch_size: self.protocol.max_batch_size,
            value: ling_types::ValueLimits {
                max_bytes: self.engine.max_value_bytes,
                max_depth: self.engine.max_nest_depth,
                max_key_bytes: self.engine.max_key_bytes,
            },
            ..Limits::default()
        }
    }

    /// 将配置转换为不依赖网络的引擎配置。
    /// Converts settings into the network-independent engine configuration.
    pub fn engine_config(&self) -> Result<ling_engine::Config, DbError> {
        Ok(ling_engine::Config {
            databases: self.database.databases,
            eviction: self.eviction.policy.parse()?,
            sample_size: self.eviction.sample_size,
            shard_count: self.engine.shard_count,
            max_memory_bytes: self.engine.max_memory_bytes,
            max_keys: self.engine.max_keys,
            value: ling_types::ValueLimits {
                max_bytes: self.engine.max_value_bytes,
                max_depth: self.engine.max_nest_depth,
                max_key_bytes: self.engine.max_key_bytes,
            },
        })
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    /// 典型 `/proc/self/limits` 头部与相邻行，用于解析样例。
    /// Typical `/proc/self/limits` header and neighbouring rows used as parsing samples.
    fn sample(open_files: &str) -> String {
        format!(
            "Limit                     Soft Limit           Hard Limit           Units     \nMax cpu time              unlimited            unlimited            seconds   \n{open_files}\nMax locked memory         8388608              8388608              bytes     \n"
        )
    }

    /// 软上限取第二列数值；`unlimited`、缺行与损坏数值分别映射为最大值、None、None。
    /// The soft limit is the first numeric column; `unlimited`, a missing row and a corrupt value map to the maximum, None and None.
    #[test]
    fn soft_limit_parsing() {
        for (row, expected) in [
            (
                "Max open files            1024                 524288               files     ",
                Some(1024),
            ),
            (
                "Max open files            65536                65536                files     ",
                Some(65536),
            ),
            (
                "Max open files            unlimited            unlimited            files     ",
                Some(u64::MAX),
            ),
            (
                "Max open files            lots                 524288               files     ",
                None,
            ),
            ("Max open files", None),
            (
                "Max processes             1024                 1024                 processes ",
                None,
            ),
        ] {
            assert_eq!(open_files_soft_limit(&sample(row)), expected, "{row}");
        }
        assert_eq!(open_files_soft_limit(""), None);
    }

    /// 阈值为连接上限加预留量：恰好等于不告警，少一个单位告警；`unlimited` 不告警；消息不含 Key 或口令。
    /// The threshold is the connection limit plus reserve: equality is silent, one below warns; unlimited is silent; messages carry no keys or passwords.
    #[test]
    fn warning_threshold() {
        let connections = 10_000usize;
        let needed = connections as u64 + DESCRIPTOR_RESERVE;
        assert!(descriptor_limit_warning(1024, connections).is_some());
        assert!(descriptor_limit_warning(needed - 1, connections).is_some());
        assert!(descriptor_limit_warning(needed, connections).is_none());
        assert!(descriptor_limit_warning(65536, connections).is_none());
        assert!(descriptor_limit_warning(u64::MAX, connections).is_none());
        assert!(descriptor_limit_warning(1024, 1).is_none());
        let message = descriptor_limit_warning(1024, connections).unwrap();
        assert!(
            message.contains("1024") && message.contains("EMFILE"),
            "{message}"
        );
    }
}

#[cfg(test)]
mod exposure_tests {
    use super::*;

    /// 各个告警仅由对应的暴露组合决定，返回内容不包含真实口令。
    /// Each warning follows its exposure combination and never includes the configured password.
    #[test]
    fn exposure_combinations_and_redaction() {
        for (host, auth, tls, metrics, metrics_host, expected) in [
            ("127.0.0.1", false, false, false, "0.0.0.0", vec![]),
            ("::1", false, false, true, "::1", vec![]),
            ("0.0.0.0", false, false, false, "127.0.0.1", vec![0]),
            ("127.0.0.1", true, false, false, "127.0.0.1", vec![1]),
            ("::", true, true, true, "::", vec![2]),
            ("0.0.0.0", false, false, true, "0.0.0.0", vec![0, 2]),
            ("0.0.0.0", true, false, true, "0.0.0.0", vec![1, 2]),
        ] {
            let mut config = Config::default();
            config.server.host = host.parse().unwrap();
            config.auth.enabled = auth;
            config.auth.password = "never-log-this-secret".into();
            config.tls.enabled = tls;
            config.metrics.enabled = metrics;
            config.metrics.host = metrics_host.parse().unwrap();
            let warnings = exposure_warnings(&config);
            let prefixes = ["Business", "Authentication", "Prometheus"];
            assert_eq!(warnings.len(), expected.len());
            for (warning, index) in warnings.iter().zip(expected) {
                assert!(warning.starts_with(prefixes[index]), "{warning}");
                assert!(!warning.contains(&config.auth.password));
            }
        }
    }
}
