//! 固定负载的闭环基准；真实 TCP 与直接引擎结果分别标记，不混合解释。
//! Fixed-workload closed-loop benchmark; real TCP and direct engine results are labeled separately.

use clap::Parser;
use ling_client::{Client, ClientTls, Options, Value};
use ling_engine::{Config, Engine};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Barrier, watch},
    task::JoinSet,
};

/// 单次实验参数；由仓库基准脚本保存完整参数及环境。
/// Parameters for one experiment; the repository runner records them and the environment.
#[derive(Clone, Parser)]
struct Args {
    /// 独立服务端地址；Address of the separate server process.
    #[arg(long, default_value = "127.0.0.1:6380")]
    address: SocketAddr,
    /// 直接测量引擎，跳过网络；Measure the engine directly, bypassing networking.
    #[arg(long)]
    engine: bool,
    /// 并发闭环客户端数；Concurrent closed-loop clients.
    #[arg(long, default_value_t = 8)]
    clients: usize,
    /// 每客户端测量操作数；Measured operations per client.
    #[arg(long, default_value_t = 10000)]
    operations: usize,
    /// 每客户端预热操作数；Warmup operations per client.
    #[arg(long, default_value_t = 1000)]
    warmup: usize,
    /// 均匀采样的预填充 Key 数；Preloaded keys sampled uniformly.
    #[arg(long, default_value_t = 10000)]
    keys: usize,
    /// Bytes 值的负载字节数；Payload bytes per Bytes value.
    #[arg(long, default_value_t = 64)]
    value_bytes: usize,
    /// SET 的百分比，其余为命中 GET；SET percentage; the remainder are successful GETs.
    #[arg(long, default_value_t = 10)]
    write_percent: u8,
    /// 伪随机负载种子，不控制进程内部哈希；Workload seed, independent of internal routing hashes.
    #[arg(long, default_value_t = 20261114)]
    seed: u64,
    /// 每数据库固定分片数；Fixed shard count per database.
    #[arg(long, default_value_t = 4)]
    shards: u32,
    /// 固定测量秒数，零时使用 operations；Measurement seconds, or operations when zero.
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// 90% 请求集中到首 1% Key；Routes 90% of requests to the first 1% of keys.
    #[arg(long)]
    hot: bool,
    /// GET 中明确未命中的百分比；Explicit miss percentage among GETs.
    #[arg(long, default_value_t = 0)]
    miss_percent: u8,
    /// 每次写入重设的毫秒 TTL，零表示无 TTL；Renewed write TTL in milliseconds, zero disables TTL.
    #[arg(long, default_value_t = 0)]
    ttl_ms: u64,
    /// 可选验证式 TLS 名称；Optional verified TLS server name.
    #[arg(long)]
    tls_server_name: Option<String>,
    /// 自定义 CA；Custom CA.
    #[arg(long, requires = "tls_server_name")]
    tls_ca: Option<PathBuf>,
    /// 无消费者 / 快消费者 / 每事件等待 20ms；None, fast, or 20ms-delayed consumer.
    #[arg(long, default_value = "none", value_parser = ["none", "fast", "slow"])]
    consumer: String,
    /// 每客户端每秒最多操作数，零为闭环不限速；Per-client rate cap, zero runs uncapped closed-loop.
    #[arg(long, default_value_t = 0)]
    rate: u64,
}

/// 每个客户端拥有连接；引擎模式共享同一明确所有权实现。
/// Each client owns its connection; engine mode shares the same ownership implementation.
enum Target {
    /// 经过官方 SDK 的真实 TCP；Real TCP through the official SDK.
    Tcp(Box<Client>),
    /// 直接引擎调用，只用于微基准；Direct calls for the engine microbenchmark only.
    Engine(Arc<Engine>),
}

impl Target {
    /// 保留错误码；客户端失败作为独立分类，不重放失败写入。
    /// Preserves error codes; client failures are separate categories and writes are never replayed.
    async fn execute(&mut self, command: &str, args: Vec<Value>) -> Result<Value, String> {
        match self {
            Self::Tcp(client) => client
                .execute(command, args)
                .await
                .map_err(|error| match error {
                    ling_client::Error::Server(error) => error.code,
                    ling_client::Error::Local {
                        kind,
                        outcome_unknown,
                    } => {
                        format!("client_{kind:?}_unknown_{outcome_unknown}")
                    }
                }),
            Self::Engine(engine) => engine
                .execute(0, command, &args, 1024 * 1024)
                .map_err(|error| error.code),
        }
    }
}

/// 创建独占数据库 0 的实验连接；启动脚本为每次网络实验新建服务端。
/// Creates an experiment connection in DB 0; the runner starts a fresh server for each network run.
async fn target(
    args: &Args,
    engine: &Option<Arc<Engine>>,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    Ok(if let Some(engine) = engine {
        Target::Engine(engine.clone())
    } else {
        Target::Tcp(Box::new(
            Client::connect(
                args.address,
                None,
                0,
                Options {
                    tls: args.tls_server_name.clone().map(|server_name| ClientTls {
                        server_name,
                        ca_file: args.tls_ca.clone(),
                        cert_file: None,
                        key_file: None,
                    }),
                    ..Options::default()
                },
            )
            .await?,
        ))
    })
}

/// 有界对数直方图，分位数先取整微秒，再按小于 3.2% 的桶量化，持续运行不保留逐请求样本。
/// Bounded logarithmic histogram; integer-microsecond percentiles use under 3.2% bucket rounding without per-request storage.
#[derive(Default)]
struct Samples {
    /// 量化微秒上界与频数；Quantized microsecond upper bounds and counts.
    histogram: BTreeMap<u64, u64>,
    /// 合法响应次数，包含预期未命中；Valid responses including expected misses.
    successes: u64,
    /// 所有错误分类；All failure categories.
    errors: BTreeMap<String, u64>,
    /// TTL 负载下的缺失次数，单列而非掩盖；Missing TTL reads reported separately.
    misses: u64,
}
impl Samples {
    /// 每个二进制数量级分为 32 桶；Thirty-two buckets per binary order of magnitude.
    fn record(&mut self, started: Instant) {
        let us = started
            .elapsed()
            .as_micros()
            .max(1)
            .min(u128::from(u64::MAX)) as u64;
        let shift = (63 - us.leading_zeros()).saturating_sub(5);
        let width = 1u64 << shift;
        let upper = us.saturating_add(width - 1) / width * width;
        *self.histogram.entry(upper).or_default() += 1;
        self.successes += 1;
    }
    /// 最近秩位于哪个桶即返回其上界；Returns the upper bound containing the nearest rank.
    fn percentile(&self, per_mille: u64) -> Option<u64> {
        let rank = (self.successes * per_mille).div_ceil(1000);
        let mut count = 0;
        self.histogram.iter().find_map(|(&upper, &n)| {
            count += n;
            (count >= rank && rank > 0).then_some(upper)
        })
    }
    /// 合并计数而不合并分位数；Merges counts, never precomputed percentiles.
    fn merge(&mut self, other: Self) {
        self.successes += other.successes;
        self.misses += other.misses;
        for (bucket, count) in other.histogram {
            *self.histogram.entry(bucket).or_default() += count;
        }
        for (error, count) in other.errors {
            *self.errors.entry(error).or_default() += count;
        }
    }
}

/// 写入内容持续变化；计时包含 SDK 队列和完整响应，限速等待在计时外。
/// Writes keep changing; timing includes SDK queues and full responses but excludes rate pacing.
async fn workload(
    target: &mut Target,
    args: &Args,
    worker: usize,
    operations: usize,
    seed: u64,
    seconds: u64,
) -> Samples {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut samples = Samples::default();
    let phase = Instant::now();
    let mut sequence = 0u64;
    while if seconds == 0 {
        sequence < operations as u64
    } else {
        phase.elapsed() < Duration::from_secs(seconds)
    } {
        if args.rate > 0 {
            let due = phase + Duration::from_secs_f64(sequence as f64 / args.rate as f64);
            tokio::time::sleep_until(due.into()).await;
        }
        let count = if args.hot && rng.random_range(0..100) < 90 {
            (args.keys / 100).max(1)
        } else {
            args.keys
        };
        let index = rng.random_range(0..count);
        let write = rng.random_range(0..100) < args.write_percent;
        let missing = !write && rng.random_range(0..100) < args.miss_percent;
        let key = Value::String(format!(
            "{}:{index:08}",
            if missing { "missing" } else { "baseline" }
        ));
        let values = if write {
            let stamp = ((worker as u64) << 48 | sequence)
                .wrapping_add(seed)
                .to_le_bytes();
            let payload = stamp.into_iter().cycle().take(args.value_bytes).collect();
            let mut values = vec![key, Value::Bytes(payload)];
            if args.ttl_ms > 0 {
                values.extend([Value::String("PX".into()), Value::Int(args.ttl_ms as i64)]);
            }
            values
        } else {
            vec![key]
        };
        let started = Instant::now();
        let result = target
            .execute(if write { "SET" } else { "GET" }, values)
            .await;
        match result {
            Ok(Value::Bool(true)) if write => samples.record(started),
            Ok(Value::Bytes(bytes)) if !write && !missing && bytes.len() == args.value_bytes => {
                samples.record(started)
            }
            Err(code) if !write && code == "ERR_KEY_NOT_FOUND" && (missing || args.ttl_ms > 0) => {
                samples.misses += 1;
                samples.record(started);
            }
            Ok(_) => {
                *samples
                    .errors
                    .entry("unexpected_response".into())
                    .or_default() += 1
            }
            Err(error) => *samples.errors.entry(error).or_default() += 1,
        }
        sequence += 1;
    }
    samples
}

/// INFO 观测真实受管字节，直接引擎单列；RSS 由驱动读取 /proc。
/// INFO observes actual managed bytes, separately from direct engine state; the runner samples RSS.
async fn memory(target: &mut Target) -> Result<Value, String> {
    match target {
        Target::Engine(engine) => Ok(Value::UInt(
            engine.memory_usage().map_err(|e| e.code)? as u64
        )),
        Target::Tcp(_) => {
            target
                .execute("INFO", vec![Value::String("memory".into())])
                .await
        }
    }
}

/// 仅转换 INFO 的诊断字段；数据负载仍保留原生 Value。
/// Converts only INFO diagnostic fields; workload data remains native Value.
fn observe(value: Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(v) => v.into(),
        Value::UInt(v) => v.into(),
        Value::Int(v) => v.into(),
        Value::String(v) => v.into(),
        Value::Map(v) => {
            serde_json::Value::Object(v.into_iter().map(|(k, v)| (k, observe(v))).collect())
        }
        Value::List(v) => serde_json::Value::Array(v.into_iter().map(observe).collect()),
        _ => unreachable!("INFO returned an unsupported diagnostic type"),
    }
}

/// 验证实验资源规模，在计时前准备数据、连接与工作线程。
/// Validates experiment size and prepares data, connections, and workers before measurement.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    if args.clients == 0
        || args.clients > 256
        || args.operations == 0
        || args.keys == 0
        || args.keys > 1000000
        || args.write_percent > 100
        || args.miss_percent > 100
        || args.seconds > 3600
        || args.rate > 1000000
        || args.ttl_ms > i64::MAX as u64
        || (args.engine
            && (args.seconds > 0
                || args.consumer != "none"
                || args.tls_server_name.is_some()
                || args.ttl_ms > 0))
        || args.value_bytes == 0
        || args.value_bytes > 65536
        || args.warmup > 1000000
        || args
            .clients
            .checked_mul(args.operations)
            .is_none_or(|n| n > 10000000)
    {
        return Err("Invalid or excessive benchmark workload".into());
    }
    let engine = if args.engine {
        Some(Arc::new(Engine::new(
            Config {
                shard_count: args.shards,
                max_memory_bytes: 268435456,
                ..Config::default()
            },
            1,
        )?))
    } else {
        None
    };
    let mut setup = target(&args, &engine).await?;
    for i in 0..args.keys {
        setup
            .execute(
                "SET",
                vec![
                    Value::String(format!("baseline:{i:08}")),
                    Value::Bytes(vec![0; args.value_bytes]),
                ],
            )
            .await?;
    }
    let initial_managed_bytes = engine.as_ref().map(|e| e.memory_usage()).transpose()?;
    let preloaded_memory = observe(memory(&mut setup).await?);
    let mut consumer = JoinSet::new();
    let (stop_consumer, stopped) = watch::channel(false);
    if args.consumer != "none" {
        let Target::Tcp(client) = target(&args, &engine).await? else {
            unreachable!()
        };
        let mut subscriber = client
            .subscribe(vec!["baseline:*".into()], Some("benchmark"))
            .await?;
        let slow = args.consumer == "slow";
        let mut stopped = stopped;
        consumer.spawn(async move {
            let started = Instant::now();
            let mut count = 0u64;
            let mut errors = BTreeMap::<String, u64>::new();
            let mut age = BTreeMap::<u64,u64>::new();
            let mut failure_phase = None;
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    result = subscriber.next_event() => match result {
                        Ok(event) => {
                            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                            *age.entry(now.saturating_sub(event.timestamp_ms) / 10 * 10).or_default() += 1;
                            if slow { tokio::time::sleep(Duration::from_millis(20)).await; }
                            if let Err(e) = subscriber.ack(event.id).await { *errors.entry(e.to_string()).or_default() += 1; failure_phase = Some("ack"); break; }
                            count += 1;
                        }
                        Err(e) => { *errors.entry(e.to_string()).or_default() += 1; failure_phase = Some("receive"); break; }
                    }
                }
            }
            // 保留原始三元组，并另外记录是否因错误提前结束。
            // Retain the original tuple and separately record premature error termination.
            let status = serde_json::json!({
                "termination": if failure_phase.is_some() { "error" } else { "requested_stop" },
                "failure_phase": failure_phase, "elapsed_seconds": started.elapsed().as_secs_f64(),
            });
            ((count, errors, age), status)
        });
    }
    drop(setup);
    let barrier = Arc::new(Barrier::new(args.clients + 1));
    let (start, ready) = watch::channel(false);
    let mut workers = JoinSet::new();
    for worker in 0..args.clients {
        let (mut connection, args, barrier) =
            (target(&args, &engine).await?, args.clone(), barrier.clone());
        let mut ready = ready.clone();
        workers.spawn(async move {
            let seed = args.seed.wrapping_add(worker as u64);
            let warmup = workload(&mut connection, &args, worker, args.warmup, seed, 0).await;
            barrier.wait().await;
            ready.changed().await.expect("benchmark start signal");
            let result = workload(
                &mut connection,
                &args,
                worker,
                args.operations,
                seed ^ u64::MAX,
                args.seconds,
            )
            .await;
            (warmup.errors, result)
        });
    }
    barrier.wait().await;
    let started = Instant::now();
    start.send_replace(true);
    let mut all = Samples::default();
    let mut warmup_errors = BTreeMap::<String, u64>::new();
    while let Some(result) = workers.join_next().await {
        let (warm, samples) = result?;
        all.merge(samples);
        for (key, count) in warm {
            *warmup_errors.entry(key).or_default() += count;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    stop_consumer.send_replace(true);
    let consumption = consumer.join_next().await.transpose()?;
    let (consumption, consumer_status) = match consumption {
        Some((consumption, status)) => (Some(consumption), Some(status)),
        None => (None, None),
    };
    let mut inspect = target(&args, &engine).await?;
    let final_memory = observe(memory(&mut inspect).await?);
    let event_stats = if args.engine {
        None
    } else {
        Some(observe(
            inspect
                .execute("INFO", vec![Value::String("events".into())])
                .await?,
        ))
    };
    inspect.execute("FLUSHDB", vec![]).await?;
    let deleted_memory = observe(memory(&mut inspect).await?);
    // 留出驱动采样删除后的 RSS；Allow the runner to sample post-deletion RSS.
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!(
        "{}",
        serde_json::json!({
            "mode": if args.engine { "engine" } else { "tcp" },
            "clients": args.clients, "pipeline": 1, "keys": args.keys, "value_bytes": args.value_bytes,
            "write_percent": args.write_percent, "seed": args.seed, "shards": args.shards,
            "operations_per_client": args.operations, "warmup_per_client": args.warmup,
            "elapsed_seconds": elapsed, "successes": all.successes,
            "successes_per_second": all.successes as f64 / elapsed,
            "errors": all.errors, "warmup_errors": warmup_errors,
            "p50_us": all.percentile(500), "p95_us": all.percentile(950),
            "p99_us": all.percentile(990), "p999_us": all.percentile(999),
            "success_latency_us_histogram": all.histogram,
            "histogram": "floor to integer microseconds, then bucket upper bound; 32 subdivisions per power of two; <=3.2% bucket rounding",
            "seconds": args.seconds, "hot": args.hot, "miss_percent": args.miss_percent, "misses": all.misses,
            "ttl_ms": args.ttl_ms, "tls": args.tls_server_name.is_some(), "rate_per_client": args.rate,
            "preloaded_memory": preloaded_memory, "final_memory": final_memory, "deleted_memory": deleted_memory,
            "event_stats": event_stats, "consumer": args.consumer, "consumption": consumption,
            "consumer_status": consumer_status,
            "managed_bytes": initial_managed_bytes,
        })
    );
    if !all.errors.is_empty() || !warmup_errors.is_empty() {
        return Err("Benchmark observed failed requests".into());
    }
    Ok(())
}
