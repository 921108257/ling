//! 单所有者内存引擎：先准备结果和变更，通过限额检查后一次发布。
//! Single-owner memory engine: prepare results and changes, then publish after checking limits.

mod containers;
mod generic;
mod numeric;
mod support;

use linger_types::{DbError, ErrorCode, Generation, Value, ValueLimits};
use std::{collections::HashMap, mem::size_of, time::Instant};
use support::{Effect, budget_error, error, response_size};

/// 单分片阶段的存储与资源配置；零预算表示不限制。
/// Storage and resource settings for the single-shard stage; zero budgets are unlimited.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 隔离数据库数量；Number of isolated database namespaces.
    pub databases: u32,
    /// 全实例受管字节上限；Instance-wide managed-byte limit.
    pub max_memory_bytes: usize,
    /// 每数据库活跃 Key 上限；Active-key limit per database.
    pub max_keys: usize,
    /// 业务值大小、深度及名称限额；Business-value size, depth, and name limits.
    pub value: ValueLimits,
}

impl Default for Config {
    /// 默认 16 个库且不限制受管内存和 Key 数。
    /// Defaults to 16 databases with unlimited managed memory and key count.
    fn default() -> Self {
        Self {
            databases: 16,
            max_memory_bytes: 0,
            max_keys: 0,
            value: ValueLimits::default(),
        }
    }
}

impl Config {
    /// 校验配置边界，不创建数据库或分配索引。
    /// Validates configuration boundaries without allocating database indexes.
    pub fn validate(&self) -> Result<(), DbError> {
        if !(1..=65536).contains(&self.databases)
            || !(1..=64).contains(&self.value.max_depth)
            || self.value.max_bytes == 0
            || self.value.max_key_bytes == 0
        {
            return Err(error(
                ErrorCode::ConfigInvalid,
                "Invalid engine configuration",
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

/// 索引条目；受管字节与元数据和值共同发布。
/// Indexed entry; its managed charge is published with metadata and value.
struct Entry {
    /// 条目当前持有的完整业务值。
    /// Complete business value owned by this entry.
    value: Value,
    /// 与值原子发布的生命周期及期限。
    /// Lifecycle and deadline published atomically with the value.
    metadata: Metadata,
    /// 本条目占用的保守受管字节数。
    /// Conservative managed-byte charge for this entry.
    bytes: usize,
}

/// 通过独占可变引用执行命令；服务端负责在外层取得唯一执行权。
/// Executes commands through an exclusive mutable reference; the server acquires ownership.
pub struct Engine {
    /// 启动时固定的业务和容量限制。
    /// Business and capacity limits fixed at startup.
    config: Config,
    /// 命名空间独立；索引仅在持有引擎执行权时访问。
    /// Isolated namespaces; indexes are accessed only under engine ownership.
    databases: Vec<HashMap<String, Entry>>,
    /// 本进程生命周期代号，不写入用户数据。
    /// Process lifecycle identifier, separate from user data.
    epoch: u128,
    /// 全引擎创建计数，成功发布前不消耗。
    /// Engine-wide creation counter, consumed only by successful publication.
    creations: u64,
    /// 跨库受管占用总和，不代表进程 RSS。
    /// Managed usage summed across databases, not process RSS.
    managed_bytes: usize,
}

impl Engine {
    /// 建立数据库命名空间；epoch 由入口生成，测试可指定固定值。
    /// Creates namespaces; the entry point generates the epoch, while tests may fix it.
    pub fn new(config: Config, epoch: u128) -> Result<Self, DbError> {
        config.validate()?;
        let databases = (0..config.databases).map(|_| HashMap::new()).collect();
        Ok(Self {
            config,
            databases,
            epoch,
            creations: 0,
            managed_bytes: 0,
        })
    }

    /// 返回逻辑受管内存，不代表进程 RSS。
    /// Returns logically managed memory, not process RSS.
    pub fn managed_bytes(&self) -> usize {
        self.managed_bytes
    }

    /// 读取活跃条目元数据；不存在和逻辑过期都返回缺失错误。
    /// Reads active-entry metadata; missing and logically expired entries both fail.
    pub fn metadata(&self, db: u16, key: &str) -> Result<Metadata, DbError> {
        self.config.value.validate_key(key)?;
        Ok(self.entry(db, key, Instant::now())?.metadata)
    }

    /// 在取得执行权后采样一次时间；响应字节上限包含 status，不含帧头。
    /// Samples time after ownership is acquired; the response budget includes status, not the header.
    pub fn execute(
        &mut self,
        db: u16,
        name: &str,
        args: &[Value],
        response_bytes: usize,
    ) -> Result<Value, DbError> {
        self.execute_at(db, name, args, response_bytes, Instant::now())
    }

    /// 单一观察时点用于全部查找；只在准备、预算和身份检查全部成功后修改索引。
    /// One observation time governs all lookups; the index changes only after preparation and checks.
    fn execute_at(
        &mut self,
        db: u16,
        name: &str,
        args: &[Value],
        response_bytes: usize,
        now: Instant,
    ) -> Result<Value, DbError> {
        self.database(db)?;
        let name = name.to_ascii_uppercase();
        for (index, value) in args.iter().enumerate() {
            let limits = if linger_types::is_value_argument(&name, index) {
                self.config.value
            } else {
                ValueLimits {
                    max_bytes: usize::MAX,
                    max_depth: self.config.value.max_depth + 4,
                    max_key_bytes: usize::MAX,
                }
            };
            value.validate(limits)?;
        }
        let effect =
            match name.as_str() {
                "SET" | "GET" | "GETSET" | "MGET" | "MSET" | "DEL" | "EXISTS" | "TYPE"
                | "DBSIZE" => self.generic(db, &name, args, now, response_bytes)?,
                "INCR" | "DECR" | "INCRBY" | "DECRBY" | "INCRFLOAT" | "APPEND" | "STRLEN"
                | "BLEN" => self.numeric(db, &name, args, now)?,
                _ => self.container(db, &name, args, now, response_bytes)?,
            };
        response_size(&effect.result, response_bytes, self.config.value)?;
        self.publish(db, effect, now)
    }

    /// 校验库号后借用对应索引；Validate the database number before borrowing its index.
    fn database(&self, db: u16) -> Result<&HashMap<String, Entry>, DbError> {
        self.databases
            .get(usize::from(db))
            .ok_or_else(|| error(ErrorCode::DbOutOfRange, "Database out of range"))
    }

    /// 所有命令共用严格的到期可见性过滤。
    /// Every command shares the same strict expiration-visibility filter.
    fn entry(&self, db: u16, key: &str, now: Instant) -> Result<&Entry, DbError> {
        self.database(db)?
            .get(key)
            .filter(|entry| entry.metadata.deadline.is_none_or(|d| now < d.monotonic))
            .ok_or_else(|| error(ErrorCode::KeyNotFound, "Key not found"))
    }

    /// 先计算整组变更的容量及版本，再发布；失败不会消耗身份或删除旧条目。
    /// Preflights the whole change set, then publishes; failures consume no identities or old entries.
    fn publish(&mut self, db: u16, effect: Effect, now: Instant) -> Result<Value, DbError> {
        if effect.changes.is_empty() {
            return Ok(effect.result);
        }
        let index = self.database(db)?;
        let mut memory = self.managed_bytes;
        let mut keys = index
            .values()
            .filter(|e| e.metadata.deadline.is_none_or(|d| now < d.monotonic))
            .count();
        let mut creations = self.creations;
        let mut prepared = Vec::with_capacity(effect.changes.len());
        for change in effect.changes {
            let old = index.get(&change.key);
            let active = old.filter(|e| e.metadata.deadline.is_none_or(|d| now < d.monotonic));
            let deadline = if change.keep_deadline {
                active.and_then(|e| e.metadata.deadline)
            } else {
                None
            };
            if active.is_some_and(|e| {
                change.value.as_ref() == Some(&e.value) && e.metadata.deadline == deadline
            }) {
                continue;
            }
            if change.value.is_none() && old.is_none() {
                continue;
            }
            if let Some(old) = old {
                memory -= old.bytes;
            }
            if active.is_some() {
                keys -= 1;
            }
            let next = if let Some(value) = change.value {
                value.validate(self.config.value)?;
                let bytes = entry_size(&change.key, &value)?;
                memory = memory.checked_add(bytes).ok_or_else(budget_error)?;
                keys += 1;
                let metadata = if let Some(entry) = active {
                    Metadata {
                        generation: entry.metadata.generation,
                        revision: entry
                            .metadata
                            .revision
                            .checked_add(1)
                            .ok_or_else(|| error(ErrorCode::Overflow, "Revision exhausted"))?,
                        deadline,
                    }
                } else {
                    creations = creations.checked_add(1).ok_or_else(|| {
                        error(ErrorCode::Overflow, "Generation counter exhausted")
                    })?;
                    Metadata {
                        generation: Generation {
                            epoch: self.epoch,
                            counter: creations,
                        },
                        revision: 1,
                        deadline,
                    }
                };
                Some(Entry {
                    value,
                    metadata,
                    bytes,
                })
            } else {
                None
            };
            prepared.push((change.key, next));
        }
        // 缩小预算的写入允许在已有占用超过新上限时释放资源。
        // Shrinking writes may release resources even when current usage exceeds a limit.
        if (self.config.max_memory_bytes != 0
            && memory > self.config.max_memory_bytes
            && memory > self.managed_bytes)
            || (self.config.max_keys != 0 && keys > self.config.max_keys)
        {
            return Err(budget_error());
        }
        let index = &mut self.databases[usize::from(db)];
        for (key, entry) in prepared {
            match entry {
                Some(entry) => {
                    index.insert(key, entry);
                }
                None => {
                    index.remove(&key);
                }
            }
        }
        self.creations = creations;
        self.managed_bytes = memory;
        Ok(effect.result)
    }
}

/// 按保守逻辑模型统计索引、Key、条目及容器堆空间，使用检查算术。
/// Charges index, key, entry, and container heap space conservatively with checked arithmetic.
fn entry_size(key: &str, value: &Value) -> Result<usize, DbError> {
    size_of::<Entry>()
        .checked_add(5 * size_of::<usize>())
        .and_then(|n| n.checked_add(key.len()))
        .and_then(|n| n.checked_add(support::heap_size(value).ok()?))
        .ok_or_else(budget_error)
}
