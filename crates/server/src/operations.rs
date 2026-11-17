//! 运行状态、配置重载与无业务标签的指标；锁不跨越网络等待。
//! Runtime state, config reload, and metrics without business labels; locks never span network waits.

use crate::Config;
use ling_types::{DbError, ErrorCode, Value};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Duration, timeout},
};

/// 每实例共享的控制面，与缓存数据分离。
/// Per-instance control plane separated from cache data.
pub(crate) struct Runtime {
    /// 固定配置及重载来源；Static configuration and reload source.
    pub config: Arc<Config>,
    /// 唯一动态配置；Sole mutable setting.
    level: Mutex<String>,
    /// 启动时间；Start time.
    start: Instant,
    /// 关闭请求；Shutdown request.
    pub stop: watch::Sender<bool>,
    /// 完成请求数；Completed requests.
    requests: AtomicU64,
    /// 失败请求数；Failed requests.
    errors: AtomicU64,
    /// 当前业务连接；Current business connections.
    pub clients: AtomicU64,
    /// 认证失败告警每实例至多每分钟一次；Authentication warnings occur at most once per minute per instance.
    last_auth_warning: Mutex<Option<Instant>>,
    /// 接入层内部计数与限频告警；对外指标放到 0.1.5。
    /// Internal accept-side counters and rate-limited warnings; public metrics arrive in 0.1.5.
    pub accept: crate::accept::AcceptStats,
    /// 请求时延固定桶，无 Key 或命令标签；Fixed latency buckets without key or command labels.
    latency: [AtomicU64; 6],
    /// 总延迟微秒用于 histogram sum；Total latency microseconds for histogram sum.
    latency_micros: AtomicU64,
}
impl Runtime {
    /// 启动时建立控制状态；Creates control state at startup.
    pub fn new(config: Arc<Config>) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            level: Mutex::new(config.log.level.clone()),
            config,
            start: Instant::now(),
            stop,
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            clients: AtomicU64::new(0),
            last_auth_warning: Mutex::new(None),
            accept: crate::accept::AcceptStats::new(),
            latency: std::array::from_fn(|_| AtomicU64::new(0)),
            latency_micros: AtomicU64::new(0),
        }
    }
    /// 请求结束统计，时间包含排队执行；Records completed requests including queue time.
    pub fn record(&self, started: Instant, failed: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let micros = started.elapsed().as_micros();
        self.latency_micros
            .fetch_add(micros.min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        let bucket = [100, 1000, 10000, 100000, 1000000]
            .iter()
            .position(|n| micros <= *n)
            .unwrap_or(5);
        self.latency[bucket].fetch_add(1, Ordering::Relaxed);
    }
    /// 限频日志不记录源地址、口令或业务参数，也不新增对外指标。
    /// Rate-limited logs omit source addresses, passwords, and business arguments without adding public metrics.
    pub fn auth_failed(&self) {
        let Ok(mut last) = self.last_auth_warning.lock() else {
            return;
        };
        let now = Instant::now();
        if last.is_some_and(|previous| now.duration_since(previous).as_secs() < 60) {
            return;
        }
        *last = Some(now);
        drop(last);
        self.log(
            "warn",
            "ERR_AUTH_FAILED: authentication rejected; warning limited to once per minute",
        );
    }
    /// 日志消息由调用点固定提供，不能包含请求内容。
    /// Call sites provide fixed messages that must not contain request contents.
    pub fn log(&self, severity: &str, message: &str) {
        let rank = |level: &str| {
            ["trace", "debug", "info", "warn", "error"]
                .iter()
                .position(|v| *v == level)
                .unwrap_or(4)
        };
        let Ok(level) = self.level.lock() else {
            return;
        };
        if rank(severity) < rank(&level) {
            return;
        }
        let line = if self.config.log.format == "json" {
            serde_json::json!({"level":severity,"message":message}).to_string()
        } else {
            format!("{severity}: {message}")
        };
        if self.config.log.output == "stdout" {
            println!("{line}");
        } else {
            eprintln!("{line}");
        }
    }
    /// 配置查询一律脱敏，静态变更整体拒绝；可重载时只变更日志级别。
    /// Queries redact secrets and reject static changes atomically; reload only changes log level.
    pub fn configure(&self, args: &[Value]) -> Result<Value, DbError> {
        match args {
            [Value::String(op), Value::String(pattern)] if op.eq_ignore_ascii_case("GET") => {
                let mut entries = self.flatten(&self.config)?;
                entries.insert(
                    "log.level".into(),
                    Value::String(self.level.lock().map_err(|_| invalid())?.clone()),
                );
                entries.insert("auth.password".into(), Value::String("<redacted>".into()));
                entries.retain(|key, _| matches(pattern.as_bytes(), key.as_bytes()));
                Ok(Value::Map(entries))
            }
            [Value::String(op), Value::String(key), value] if op.eq_ignore_ascii_case("SET") => {
                if key != "log.level" {
                    return Err(DbError::new(
                        ErrorCode::ConfigImmutable,
                        "Configuration requires restart",
                    ));
                }
                let Value::String(level) = value else {
                    return Err(invalid());
                };
                if !["trace", "debug", "info", "warn", "error"].contains(&level.as_str()) {
                    return Err(invalid());
                }
                *self.level.lock().map_err(|_| invalid())? = level.clone();
                Ok(Value::Bool(true))
            }
            [Value::String(op)] if op.eq_ignore_ascii_case("RELOAD") => {
                let source = self.config.source.as_ref().ok_or_else(invalid)?;
                let candidate = Config::load(source)?;
                let mut before = self.flatten(&self.config)?;
                let mut after = self.flatten(&candidate)?;
                before.remove("log.level");
                after.remove("log.level");
                if before != after {
                    return Err(DbError::new(
                        ErrorCode::ConfigImmutable,
                        "Static configuration changed; reload rejected",
                    ));
                }
                *self.level.lock().map_err(|_| invalid())? = candidate.log.level;
                Ok(Value::Bool(true))
            }
            _ => Err(invalid()),
        }
    }
    /// 标准扁平字段名，实际分片数替代自动值。
    /// Standard flat field names with resolved shard count instead of automatic zero.
    fn flatten(&self, config: &Config) -> Result<BTreeMap<String, Value>, DbError> {
        let table = toml::Value::try_from(config).map_err(|_| invalid())?;
        let mut out = BTreeMap::new();
        for (group, fields) in table.as_table().ok_or_else(invalid)? {
            for (key, value) in fields.as_table().ok_or_else(invalid)? {
                let value = match value {
                    toml::Value::String(s) => Value::String(s.clone()),
                    toml::Value::Integer(i) => Value::UInt((*i).try_into().map_err(|_| invalid())?),
                    toml::Value::Boolean(b) => Value::Bool(*b),
                    _ => return Err(invalid()),
                };
                out.insert(format!("{group}.{key}"), value);
            }
        }
        out.insert(
            "engine.shard_count".into(),
            Value::UInt(config.engine_config()?.resolved_shards().into()),
        );
        Ok(out)
    }
    /// INFO 固定分类，未提供伪造 RSS 或未测量的性能数据。
    /// INFO uses fixed sections without invented RSS or unmeasured performance claims.
    pub fn info(&self, engine: &ling_engine::Engine, args: &[Value]) -> Result<Value, DbError> {
        let section = match args {
            [] => "all".to_owned(),
            [Value::String(s)] => s.to_ascii_lowercase(),
            _ => {
                return Err(DbError::new(
                    ErrorCode::InvalidArgument,
                    "Invalid INFO section",
                ));
            }
        };
        if ![
            "all", "server", "memory", "stats", "clients", "keyspace", "events",
        ]
        .contains(&section.as_str())
        {
            return Err(DbError::new(
                ErrorCode::InvalidArgument,
                "Invalid INFO section",
            ));
        }
        let mut out = BTreeMap::new();
        let mut add = |name: &str, fields: BTreeMap<String, Value>| {
            if section == "all" || section == name {
                out.insert(name.into(), Value::Map(fields));
            }
        };
        add(
            "server",
            BTreeMap::from([
                (
                    "version".into(),
                    Value::String(env!("CARGO_PKG_VERSION").into()),
                ),
                (
                    "uptime_ms".into(),
                    Value::UInt(self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
                ),
                (
                    "shards_per_database".into(),
                    Value::UInt(engine.shard_count().into()),
                ),
            ]),
        );
        add(
            "memory",
            BTreeMap::from([
                (
                    "managed_bytes".into(),
                    Value::UInt(engine.memory_usage()? as u64),
                ),
                (
                    "limit_bytes".into(),
                    Value::UInt(self.config.engine.max_memory_bytes as u64),
                ),
                (
                    "scheduled_expirations".into(),
                    Value::UInt(engine.scheduled_expirations()? as u64),
                ),
            ]),
        );
        add(
            "stats",
            BTreeMap::from([
                (
                    "requests".into(),
                    Value::UInt(self.requests.load(Ordering::Relaxed)),
                ),
                (
                    "errors".into(),
                    Value::UInt(self.errors.load(Ordering::Relaxed)),
                ),
            ]),
        );
        add(
            "clients",
            BTreeMap::from([(
                "connected".into(),
                Value::UInt(self.clients.load(Ordering::Relaxed)),
            )]),
        );
        if section == "all" || section == "keyspace" {
            let mut keys = BTreeMap::new();
            for db in 0..self.config.database.databases {
                let count = engine.execute(db as u16, "DBSIZE", &[], 1024)?;
                keys.insert(format!("db{db}"), count);
                if keys.len() * 64 > self.config.server.max_response_bytes {
                    return Err(DbError::new(
                        ErrorCode::LimitExceeded,
                        "INFO keyspace exceeds response budget",
                    ));
                }
            }
            add("keyspace", keys);
        }
        if let Some(store) = engine
            .events()
            .filter(|_| section == "all" || section == "events")
        {
            let stats = store.stats()?;
            let mut fields = BTreeMap::from([
                ("committed".into(), Value::UInt(stats.committed)),
                ("dropped".into(), Value::UInt(stats.dropped)),
                ("redelivered".into(), Value::UInt(stats.redelivered)),
                (
                    "journal_bytes".into(),
                    Value::UInt(stats.journal_bytes as u64),
                ),
                ("journal_errors".into(), Value::UInt(stats.journal_errors)),
                ("groups".into(), Value::UInt(stats.groups as u64)),
                ("pending".into(), Value::UInt(stats.pending as u64)),
            ]);
            fields.insert(
                "oldest_retained_ms".into(),
                stats.oldest_retained_ms.map_or(Value::Null, Value::UInt),
            );
            fields.insert(
                "by_kind".into(),
                Value::List(stats.by_kind.into_iter().map(Value::UInt).collect()),
            );
            if stats.groups.saturating_mul(
                (engine.shard_count() as usize)
                    .saturating_mul(64)
                    .saturating_add(512),
            ) > self.config.server.max_response_bytes
            {
                return Err(DbError::new(
                    ErrorCode::LimitExceeded,
                    "INFO groups exceed response budget",
                ));
            }
            let groups = store
                .group_stats(Instant::now())?
                .into_iter()
                .map(|g| {
                    Value::Map(BTreeMap::from([
                        ("name".into(), Value::String(g.name)),
                        ("db".into(), Value::UInt(g.db.into())),
                        ("expired".into(), Value::Bool(g.expired)),
                        ("pending".into(), Value::UInt(g.pending as u64)),
                        (
                            "oldest_pending_ms".into(),
                            g.oldest_pending_ms.map_or(Value::Null, Value::UInt),
                        ),
                        (
                            "watermarks".into(),
                            Value::List(
                                g.watermarks
                                    .into_iter()
                                    .map(|(shard, seq)| {
                                        Value::List(vec![
                                            Value::UInt(shard.into()),
                                            Value::UInt(seq),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ]))
                })
                .collect();
            fields.insert("consumer_groups".into(), Value::List(groups));
            add("events", fields);
        }
        Ok(Value::Map(out))
    }
    /// Prometheus 文本只使用固定标签，避免业务高基数。
    /// Prometheus text uses fixed labels to avoid business cardinality.
    fn metrics(&self, engine: &ling_engine::Engine) -> Result<String, DbError> {
        let mut text = format!(
            "# TYPE linger_requests_total counter\nlinger_requests_total {}\n# TYPE linger_errors_total counter\nlinger_errors_total {}\n# TYPE linger_clients gauge\nlinger_clients {}\n# TYPE linger_managed_bytes gauge\nlinger_managed_bytes {}\n# TYPE linger_scheduled_expirations gauge\nlinger_scheduled_expirations {}\n",
            self.requests.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.clients.load(Ordering::Relaxed),
            engine.memory_usage()?,
            engine.scheduled_expirations()?
        );
        let mut count = 0;
        text.push_str("# TYPE linger_request_duration_seconds histogram\n");
        for (index, bound) in ["0.0001", "0.001", "0.01", "0.1", "1", "+Inf"]
            .iter()
            .enumerate()
        {
            count += self.latency[index].load(Ordering::Relaxed);
            text.push_str(&format!(
                "linger_request_duration_seconds_bucket{{le=\"{bound}\"}} {count}\n"
            ));
        }
        text.push_str(&format!("linger_request_duration_seconds_count {count}\n"));
        text.push_str(&format!(
            "linger_request_duration_seconds_sum {}\n",
            self.latency_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
        ));
        if let Some(store) = engine.events() {
            let s = store.stats()?;
            for (index, kind) in [
                "created",
                "updated",
                "expired",
                "deleted",
                "evicted",
                "ttl_changed",
            ]
            .iter()
            .enumerate()
            {
                text.push_str(&format!(
                    "linger_events_by_kind_total{{kind=\"{kind}\"}} {}\n",
                    s.by_kind[index]
                ));
            }
            for (name, value) in [
                ("events_committed_total", s.committed),
                ("events_dropped_total", s.dropped),
                ("events_redelivered_total", s.redelivered),
                ("journal_errors_total", s.journal_errors),
                ("journal_bytes", s.journal_bytes as u64),
                ("events_pending", s.pending as u64),
            ] {
                text.push_str(&format!("linger_{name} {value}\n"));
            }
        }
        Ok(text)
    }
}
/// 名称匹配使用有界星号回退，无递归或分配。
/// Name matching uses bounded star fallback without recursion or allocation.
fn matches(pattern: &[u8], key: &[u8]) -> bool {
    let (mut p, mut k, mut star, mut saved) = (0, 0, None, 0);
    while k < key.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == key[k]) {
            p += 1;
            k += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            saved = k;
        } else if let Some(s) = star {
            saved += 1;
            k = saved;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
/// 公开错误仅包含安全说明；Public errors contain only safe diagnostics.
fn invalid() -> DbError {
    DbError::new(ErrorCode::ConfigInvalid, "Invalid configuration operation")
}

/// 独立 HTTP 指标监听器，最多 16 个请求，头部至多 4096 字节。
/// Separate HTTP metrics listener with at most sixteen requests and 4096 header bytes.
pub(crate) async fn serve_metrics(
    listener: TcpListener,
    runtime: Arc<Runtime>,
    engine: Arc<ling_engine::Engine>,
    mut closing: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let capacity = Arc::new(Semaphore::new(16));
    let mut tasks = JoinSet::new();
    let mut backoff = None;
    // 计数与限频告警统一经接入层统计，指标端口与业务端口互不抑制。
    // Counting and rate-limited warnings go through the accept-side stats; the metrics and business ports never suppress each other.
    let note = |error: &std::io::Error| {
        runtime
            .accept
            .failed(true, error, Instant::now(), |m| runtime.log("warn", m));
    };
    loop {
        tokio::select! {
            _=closing.changed()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=crate::accept::recover(||listener.accept(),&mut backoff,&note)=>{
                let (mut socket,_)=match accepted {
                    Ok(value)=>value,
                    Err(error)=>{
                        // 指标端点的未知错误同样只记录并延后重试，不得带停数据服务。
                        // Unknown metrics errors are likewise only recorded and retried later; they must not stop the data service.
                        note(&error);
                        tokio::select! {
                            _=closing.changed()=>break,
                            _=tokio::time::sleep(Duration::from_secs(1))=>{}
                        }
                        continue;
                    }
                };
                let Ok(permit)=capacity.clone().try_acquire_owned() else {continue;};
                let runtime=runtime.clone();let engine=engine.clone();
                tasks.spawn(async move {
                    let work=async {
                        let mut bytes=Vec::new();let mut buffer=[0;512];
                        while bytes.len()<4096 {
                            let n=socket.read(&mut buffer).await?; if n==0 {return Ok::<(),std::io::Error>(());}
                            bytes.extend_from_slice(&buffer[..n]);if bytes.windows(4).any(|s|s==b"\r\n\r\n") {break;}
                        }
                        let expected=format!("GET {} HTTP/1.",runtime.config.metrics.path);
                        // 超时不能停止阻塞采样，许可随采样保留，防止慢锁累积无界任务。
                        // A timed-out blocking sample retains its permit to prevent unbounded lock waiters.
                        let (status,body,_permit)=if bytes.starts_with(expected.as_bytes()) && bytes.len()<=4096 && bytes.windows(4).any(|s|s==b"\r\n\r\n") {
                            let (sample,permit)=tokio::task::spawn_blocking(move || (runtime.metrics(&engine),permit)).await.map_err(std::io::Error::other)?;
                            match sample {
                                Ok(s)=>("200 OK",s,permit),Err(_)=>("503 Service Unavailable",String::new(),permit)
                            }
                        } else {("404 Not Found",String::new(),permit)};
                        let response=format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                        socket.write_all(response.as_bytes()).await
                    };
                    let _=timeout(Duration::from_secs(5),work).await;
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
