//! 固定分片内存引擎；按 (db, shard) 取得执行权，预检后整体发布。
//! Fixed-shard memory engine; acquires ownership in (db, shard) order and publishes after preflight.

mod administration;
mod containers;
mod generic;
mod numeric;
#[cfg(test)]
mod ownership_tests;
mod support;
mod transaction;

use linger_types::{DbError, ErrorCode, Generation, Value, ValueLimits};
use std::{
    collections::{BTreeMap, hash_map::RandomState},
    hash::BuildHasher,
    mem::size_of,
    sync::{Mutex, MutexGuard, OnceLock, mpsc},
    time::Instant,
};
use support::{error, response_size};

/// 固定拓扑与业务预算；零内存或 Key 配额表示不限制。
/// Fixed topology and business budgets; zero memory or key quotas mean unlimited.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 隔离数据库数；Number of isolated databases.
    pub databases: u32,
    /// 每库固定分片数，0 按系统并行度解析；Fixed shards per DB; zero uses available parallelism.
    pub shard_count: u32,
    /// 全实例受管字节上限；Instance-wide managed-byte cap.
    pub max_memory_bytes: usize,
    /// 每数据库活跃 Key 上限；Active-key cap per database.
    pub max_keys: usize,
    /// 单值编码、深度与名称预算；Value wire, depth, and name budgets.
    pub value: ValueLimits,
}

impl Default for Config {
    /// 默认 16 个库，自动固定分片，不限制缓存总容量。
    /// Defaults to 16 databases, automatic fixed sharding, and unlimited cache capacity.
    fn default() -> Self {
        Self {
            databases: 16,
            shard_count: 0,
            max_memory_bytes: 0,
            max_keys: 0,
            value: ValueLimits::default(),
        }
    }
}

impl Config {
    /// 解析固定分片数；与数据库数量无关，不按拓扑创建线程。
    /// Resolves the fixed shard count independently of DB count, without one thread per shard.
    pub fn resolved_shards(&self) -> u32 {
        if self.shard_count == 0 {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get().min(u32::MAX as usize) as u32)
        } else {
            self.shard_count
        }
    }

    /// 分配前校验拓扑控制块预算及业务边界。
    /// Checks topology control-block budgets and business boundaries before allocation.
    pub fn validate(&self) -> Result<(), DbError> {
        let topology = (self.databases as usize)
            .checked_mul(self.resolved_shards() as usize)
            .and_then(|n| n.checked_mul(size_of::<Mutex<Index>>()));
        if !(1..=65536).contains(&self.databases)
            || !(1..=64).contains(&self.value.max_depth)
            || self.value.max_bytes == 0
            || self.value.max_key_bytes == 0
            || topology.is_none_or(|n| n > 128 * 1024 * 1024)
        {
            return Err(error(
                ErrorCode::ConfigInvalid,
                "Invalid engine limits or topology exceeds 128 MiB",
            ));
        }
        Ok(())
    }
}
/// 同时保存单调到期点和查询用 Unix 毫秒，留给 TTL 阶段使用。
/// Stores both a monotonic deadline and queryable Unix milliseconds for the TTL stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    /// 可见性使用的单调期限；Monotonic deadline used for visibility.
    pub monotonic: Instant,
    /// 查询使用的 Unix 毫秒；Unix milliseconds retained for queries.
    pub unix_ms: i64,
}

/// 条目身份与修订；读取元数据不暴露存储值。
/// Entry identity and revision; inspecting metadata does not expose its stored value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// 不复用的生命周期身份；Non-reusable lifecycle identity.
    pub generation: Generation,
    /// 实际状态变化时递增；Increments only when state actually changes.
    pub revision: u64,
    /// 可选期限，局部修改保留；Optional deadline preserved by local mutations.
    pub deadline: Option<Deadline>,
}

/// 分片索引使用有序 Key，游标可按排他下界继续而无需全库快照。
/// Shard indexes order keys so cursors resume from an exclusive bound without a DB snapshot.
type Index = BTreeMap<String, Entry>;

/// 条目和值、身份及逻辑字节共同发布。
/// Entries publish their values, identities, and logical byte charge together.
struct Entry {
    /// 完整业务值；Complete business value.
    value: Value,
    /// 生命周期与期限；Lifecycle and deadline.
    metadata: Metadata,
    /// 保守逻辑内存，不等于 RSS；Conservative logical memory, not RSS.
    bytes: usize,
}

/// 全实例短提交锁；永远在全部分片锁之后取得。
/// Short instance commit lock; always acquired after every shard lock.
#[derive(Default)]
struct Budget {
    /// 包含尚未回收条目的逻辑占用；Logical usage including unreclaimed entries.
    memory: usize,
    /// 只在发布成功时消耗的创建计数；Creation counter consumed only on publication.
    creations: u64,
}

/// 分片内可变状态由各自锁保护；没有全局读锁或每分片专属线程。
/// Each shard lock protects its state; no global read lock or thread per shard exists.
pub struct Engine {
    /// 启动配置，分片数已解析；Startup settings with the shard count resolved.
    config: Config,
    /// 按 (db, shard) 排列的独立所有权单元；Independent ownership units in (db, shard) order.
    shards: Vec<Mutex<Index>>,
    /// 进程内路由哈希种子；Process-local routing hash seed.
    hash: RandomState,
    /// 本进程生命周期代号；Process lifecycle identifier.
    epoch: u128,
    /// 全局受管预算及身份分配；Global managed budget and identity allocation.
    budget: Mutex<Budget>,
    /// 有界后台释放队列，首次 ASYNC 时创建。
    /// Bounded background reclamation queue, created on the first ASYNC.
    reclaimer: OnceLock<Option<mpsc::SyncSender<Vec<Index>>>>,
}

/// 命令持有的有序分片执行权；私有入口避免绕过协调器。
/// Ordered shard ownership held by a command; private entry points prevent bypassing coordination.
struct Transaction<'a> {
    /// 引擎只读引用；Read-only engine reference.
    engine: &'a Engine,
    /// 业务限制副本，供命令检查；Business limits copied for command validation.
    config: Config,
    /// 已排序且去重的锁，覆盖本次全部读写；Sorted, deduplicated guards covering all reads and writes.
    guards: Vec<(usize, MutexGuard<'a, Index>)>,
}

impl Engine {
    /// 分配固定拓扑；epoch 来自进程入口，测试可指定。
    /// Allocates the fixed topology; the process entry point supplies the epoch, tests may fix it.
    pub fn new(mut config: Config, epoch: u128) -> Result<Self, DbError> {
        config.validate()?;
        config.shard_count = config.resolved_shards();
        let count = config.databases as usize * config.shard_count as usize;
        let mut shards = Vec::new();
        shards
            .try_reserve_exact(count)
            .map_err(|_| support::budget_error())?;
        shards.extend((0..count).map(|_| Mutex::new(Index::new())));
        Ok(Self {
            config,
            shards,
            hash: RandomState::new(),
            epoch,
            budget: Mutex::new(Budget::default()),
            reclaimer: OnceLock::new(),
        })
    }

    /// 读取逻辑受管内存，锁损坏时明确返回 ERR_BUSY。
    /// Reads logically managed memory, returning ERR_BUSY explicitly on a poisoned lock.
    pub fn memory_usage(&self) -> Result<usize, DbError> {
        Ok(self.budget.lock().map_err(|_| unavailable())?.memory)
    }

    /// 返回逻辑受管内存；保留阶段 2 API，锁损坏代表内部错误。
    /// Returns managed memory; retains the stage 2 API, with poison indicating an internal failure.
    pub fn managed_bytes(&self) -> usize {
        self.memory_usage().expect("engine budget lock poisoned")
    }

    /// 当前实际分片数，启动后不变。
    /// Actual shard count, immutable after startup.
    pub fn shard_count(&self) -> u32 {
        self.config.shard_count
    }

    /// 诊断内部路由；客户端不得据此自行分片。
    /// Inspects internal routing for diagnostics; clients must not route by this value.
    pub fn shard_for(&self, key: &str) -> u32 {
        (self.hash.hash_one(key) % u64::from(self.config.shard_count)) as u32
    }

    /// 读取身份也取得同一个分片执行权，并严格过滤到期条目。
    /// Metadata reads take the same shard ownership and strictly filter expired entries.
    pub fn metadata(&self, db: u16, key: &str) -> Result<Metadata, DbError> {
        self.config.value.validate_key(key)?;
        let tx = self.lock(vec![self.location(db, key)?])?;
        Ok(tx.entry(db, key, Instant::now())?.metadata)
    }

    /// 在所有相关分片锁取得后采样时间；响应预算包含 status，不含帧头。
    /// Samples time after all relevant shards are locked; response budget includes status, not the header.
    pub fn execute(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        response_bytes: usize,
    ) -> Result<Value, DbError> {
        self.validate_db(db)?;
        let name = name.to_ascii_uppercase();
        validate_arguments(&name, args, self.config.value)?;
        let locations = self.locations(db, &name, args)?;
        let mut tx = self.lock(locations)?;
        let now = Instant::now();
        if matches!(name.as_str(), "FLUSHDB" | "FLUSHALL" | "SWAPDB") {
            return tx.administration(db, &name, args, now, response_bytes);
        }
        let effect =
            match name.as_str() {
                "RENAME" | "RENAMENX" | "COPY" | "MOVE" => tx.relocate(db, &name, args, now)?,
                "SET" | "GET" | "GETSET" | "MGET" | "MSET" | "DEL" | "EXISTS" | "TYPE"
                | "DBSIZE" => tx.generic(db, &name, args, now, response_bytes)?,
                "INCR" | "DECR" | "INCRBY" | "DECRBY" | "INCRFLOAT" | "APPEND" | "STRLEN"
                | "BLEN" => tx.numeric(db, &name, args, now)?,
                _ => tx.container(db, &name, args, now, response_bytes)?,
            };
        response_size(&effect.result, response_bytes, self.config.value)?;
        tx.publish(db, effect, now)
    }

    /// 后台释放队列满或线程不可用时同步释放，绝不积压无界退休索引。
    /// Frees synchronously when the background queue is full or unavailable, never accumulating unbounded retired indexes.
    fn retire(&self, indexes: Vec<Index>) {
        let sender = self.reclaimer.get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<Vec<Index>>(1);
            std::thread::Builder::new()
                .name("linger-reclaim".into())
                .spawn(move || {
                    while let Ok(indexes) = rx.recv() {
                        drop(indexes);
                    }
                })
                .ok()
                .map(|_| tx)
        });
        if let Some(sender) = sender {
            if let Err(error) = sender.try_send(indexes) {
                drop(error);
            }
        }
    }

    /// 校验数据库编号，不以平台整数截断宽编号。
    /// Validates database numbers without truncating wide input integers.
    fn validate_db(&self, db: u16) -> Result<(), DbError> {
        if u32::from(db) >= self.config.databases {
            Err(error(ErrorCode::DbOutOfRange, "Database out of range"))
        } else {
            Ok(())
        }
    }

    /// 将命名空间与路由映射为全局锁序位置。
    /// Maps a namespace and route to the global lock-order position.
    fn location(&self, db: u16, key: &str) -> Result<usize, DbError> {
        self.validate_db(db)?;
        Ok(db as usize * self.config.shard_count as usize + self.shard_for(key) as usize)
    }

    /// 数据库全部分片在全局锁序中的连续区间。
    /// Contiguous range of a database's shards in global lock order.
    fn db_locations(&self, db: u16) -> std::ops::Range<usize> {
        let start = db as usize * self.config.shard_count as usize;
        start..start + self.config.shard_count as usize
    }

    /// 先完成签名相关的路由；Key 配额写入锁定全库以核对活跃数量。
    /// Routes by signature first; writes with key quotas lock the database to count active entries.
    fn locations(&self, db: u16, name: &str, args: &[Value]) -> Result<Vec<usize>, DbError> {
        if name == "FLUSHALL" {
            return Ok((0..self.shards.len()).collect());
        }
        if name == "SWAPDB" {
            let [a, b] = args else {
                return Err(support::argument_error());
            };
            let (a, b) = (self.database_arg(a)?, self.database_arg(b)?);
            return Ok(self.db_locations(a).chain(self.db_locations(b)).collect());
        }
        if matches!(name, "COPY" | "MOVE" | "RENAME" | "RENAMENX") {
            let target = if name == "MOVE" {
                if args.len() != 2 {
                    return Err(support::argument_error());
                }
                self.database_arg(&args[1])?
            } else if name == "COPY" {
                copy_options(self, db, args)?.0
            } else {
                db
            };
            // 涉及容量协调时预先锁全库；不会持有目标锁再逆序追加。
            // Lock whole involved databases up front for quotas; never append locks in reverse order.
            return Ok(self
                .db_locations(db)
                .chain(self.db_locations(target))
                .collect());
        }
        if name == "DBSIZE" || name == "FLUSHDB" || (self.config.max_keys != 0 && !read_only(name))
        {
            return Ok(self.db_locations(db).collect());
        }
        let keys: Vec<_> = match name {
            "MSET" => args.iter().step_by(2).collect(),
            "MGET" | "DEL" | "EXISTS" | "SUNION" | "SINTER" | "SDIFF" | "SUNIONSTORE"
            | "SINTERSTORE" | "SDIFFSTORE" => args.iter().collect(),
            _ => args.first().into_iter().collect(),
        };
        keys.into_iter()
            .map(|v| {
                let key = support::text(v)?;
                self.config.value.validate_key(key)?;
                self.location(db, key)
            })
            .collect()
    }

    /// 将 UInt 参数转为合法数据库编号，禁止截断。
    /// Converts a UInt argument to a valid database number without truncation.
    fn database_arg(&self, value: &Value) -> Result<u16, DbError> {
        let n = support::count(value)?;
        let db = u16::try_from(n)
            .map_err(|_| error(ErrorCode::DbOutOfRange, "Database out of range"))?;
        self.validate_db(db)?;
        Ok(db)
    }

    /// 排序去重后才加锁；所有路径都遵守此顺序，不逆序追加锁。
    /// Sorts and deduplicates before locking; every path follows this order without appending locks.
    fn lock(&self, mut locations: Vec<usize>) -> Result<Transaction<'_>, DbError> {
        locations.sort_unstable();
        locations.dedup();
        let guards = locations
            .into_iter()
            .map(|id| {
                self.shards[id]
                    .lock()
                    .map(|guard| (id, guard))
                    .map_err(|_| unavailable())
            })
            .collect::<Result<_, _>>()?;
        Ok(Transaction {
            engine: self,
            config: self.config,
            guards,
        })
    }
}

/// 精确区分只读命令，未知命令不以只读身份绕过资源协调。
/// Classifies read-only commands exactly; unknown commands cannot bypass budget coordination.
fn read_only(name: &str) -> bool {
    matches!(
        name,
        "GET"
            | "MGET"
            | "EXISTS"
            | "TYPE"
            | "DBSIZE"
            | "STRLEN"
            | "BLEN"
            | "LLEN"
            | "LINDEX"
            | "LRANGE"
            | "FLEN"
            | "FGET"
            | "FKEYS"
            | "FVALS"
            | "FGETALL"
            | "FEXISTS"
            | "SCARD"
            | "SMEMBERS"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SRANDMEMBER"
            | "SUNION"
            | "SINTER"
            | "SDIFF"
    )
}

/// 协议封套与真实业务值使用独立预算，不改变标签。
/// Envelopes and actual business values use separate budgets without changing tags.
fn validate_arguments(name: &str, args: &[Value], limits: ValueLimits) -> Result<(), DbError> {
    for (index, value) in args.iter().enumerate() {
        value.validate(if linger_types::is_value_argument(name, index) {
            limits
        } else {
            ValueLimits {
                max_bytes: usize::MAX,
                max_depth: limits.max_depth + 4,
                max_key_bytes: usize::MAX,
            }
        })?;
    }
    Ok(())
}

/// 锁损坏不返回伪造成功或丢弃错误。
/// Poisoned locks never produce fabricated success or discarded failures.
fn unavailable() -> DbError {
    error(ErrorCode::Busy, "Data engine unavailable")
}

/// 保守统计条目、树节点、Key 与容器堆空间，所有算术先校验。
/// Conservatively charges entries, tree nodes, keys, and container heaps with checked arithmetic.
fn entry_size(key: &str, value: &Value) -> Result<usize, DbError> {
    size_of::<Entry>()
        .checked_add(12 * size_of::<(String, Entry)>() + 16 * size_of::<usize>())
        .and_then(|n| n.checked_add(key.len()))
        .and_then(|n| n.checked_add(support::heap_size(value).ok()?))
        .ok_or_else(support::budget_error)
}

/// COPY 选项严格验证，重复项与未知选项均拒绝。
/// COPY options are strict; duplicate and unknown options are rejected.
fn copy_options(engine: &Engine, db: u16, args: &[Value]) -> Result<(u16, bool), DbError> {
    if args.len() < 2 {
        return Err(support::argument_error());
    }
    let mut target = None;
    let mut replace = false;
    let mut pos = 2;
    while pos < args.len() {
        match support::text(&args[pos])?.to_ascii_uppercase().as_str() {
            "DB" if target.is_none() && pos + 1 < args.len() => {
                target = Some(engine.database_arg(&args[pos + 1])?);
                pos += 2;
            }
            "REPLACE" if !replace => {
                replace = true;
                pos += 1;
            }
            _ => return Err(support::argument_error()),
        }
    }
    Ok((target.unwrap_or(db), replace))
}
