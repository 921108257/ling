//! 固定负载的闭环基准；真实 TCP 与直接引擎结果分别标记，不混合解释。
//! Fixed-workload closed-loop benchmark; real TCP and direct engine results are labeled separately.

use clap::Parser;
use linger_client::{Client, Options, Value};
use linger_engine::{Config, Engine};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Instant};
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
                    linger_client::Error::Server(error) => error.code,
                    linger_client::Error::Local {
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
            Client::connect(args.address, None, 0, Options::default()).await?,
        ))
    })
}

/// 单个客户端的原始纳秒样本和错误计数；计时中不排序、不打印。
/// Raw nanosecond samples and error counts for one client; no sorting or printing in the timed loop.
#[derive(Default)]
struct Samples {
    /// 成功请求延迟；Successful-request latency.
    ns: Vec<u64>,
    /// 所有错误分类；All failure categories.
    errors: BTreeMap<String, u64>,
}

/// 写入内容随序号变化，避免重复 SET 退化成无变化操作。
/// Write payloads vary with sequence numbers so repeated SETs do not become no-ops.
async fn workload(
    target: &mut Target,
    args: &Args,
    worker: usize,
    operations: usize,
    seed: u64,
) -> Samples {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut samples = Samples {
        ns: Vec::with_capacity(operations),
        ..Samples::default()
    };
    for sequence in 0..operations {
        let key = Value::String(format!("baseline:{:08}", rng.random_range(0..args.keys)));
        let write = rng.random_range(0..100) < args.write_percent;
        let (name, values) = if write {
            let stamp = ((worker as u64) << 48 | sequence as u64)
                .wrapping_add(seed)
                .to_le_bytes();
            let payload = stamp.into_iter().cycle().take(args.value_bytes).collect();
            ("SET", vec![key, Value::Bytes(payload)])
        } else {
            ("GET", vec![key])
        };
        let started = Instant::now();
        let result = target.execute(name, values).await;
        let ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        match result {
            Ok(Value::Bool(true)) if write => samples.ns.push(ns),
            Ok(Value::Bytes(bytes)) if !write && bytes.len() == args.value_bytes => {
                samples.ns.push(ns)
            }
            Ok(_) => {
                *samples
                    .errors
                    .entry("unexpected_response".into())
                    .or_default() += 1
            }
            Err(error) => *samples.errors.entry(error).or_default() += 1,
        }
    }
    samples
}

/// 从排序的成功样本按最近秩提取分位数；零成功时返回 null。
/// Uses nearest-rank percentiles of sorted successful samples; no successes returns null.
fn percentile(ns: &[u64], per_mille: usize) -> Option<f64> {
    ns.get((ns.len() * per_mille).div_ceil(1000).saturating_sub(1))
        .map(|n| *n as f64 / 1000.0)
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
            let warmup = workload(&mut connection, &args, worker, args.warmup, seed).await;
            barrier.wait().await;
            ready.changed().await.expect("benchmark start signal");
            let result = workload(
                &mut connection,
                &args,
                worker,
                args.operations,
                seed ^ u64::MAX,
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
        all.ns.extend(samples.ns);
        for (key, count) in samples.errors {
            *all.errors.entry(key).or_default() += count;
        }
        for (key, count) in warm {
            *warmup_errors.entry(key).or_default() += count;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    all.ns.sort_unstable();
    let mut histogram = BTreeMap::<u64, u64>::new();
    for n in &all.ns {
        *histogram.entry(n / 1000).or_default() += 1;
    }
    println!(
        "{}",
        serde_json::json!({
            "mode": if args.engine { "engine" } else { "tcp" },
            "clients": args.clients, "pipeline": 1, "keys": args.keys, "value_bytes": args.value_bytes,
            "write_percent": args.write_percent, "seed": args.seed, "shards": args.shards,
            "operations_per_client": args.operations, "warmup_per_client": args.warmup,
            "elapsed_seconds": elapsed, "successes": all.ns.len(),
            "successes_per_second": all.ns.len() as f64 / elapsed,
            "errors": all.errors, "warmup_errors": warmup_errors,
            "p50_us": percentile(&all.ns, 500), "p95_us": percentile(&all.ns, 950),
            "p99_us": percentile(&all.ns, 990), "p999_us": percentile(&all.ns, 999),
            "success_latency_us_histogram": histogram,
            "managed_bytes": engine.as_ref().map(|e| e.memory_usage()).transpose()?,
        })
    );
    if !all.errors.is_empty() || !warmup_errors.is_empty() {
        return Err("Benchmark observed failed requests".into());
    }
    Ok(())
}
