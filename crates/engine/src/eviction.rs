//! 淘汰候选仅在完整提交预检后删除；访问热度不改变业务修订号。
//! Eviction candidates are removed only after full commit preflight; access heat never changes business revisions.

use crate::{Transaction, support::*};
use linger_types::{DbError, ErrorCode};
use rand::seq::IteratorRandom;
use std::{
    collections::{BTreeMap, HashSet},
    str::FromStr,
    time::Instant,
};

/// 内存和 Key 配额达到上限后的选择策略。
/// Selection policy when memory or key quotas are exceeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// 拒绝扩大占用的写入；Reject writes that increase over-limit usage.
    #[default]
    NoEviction,
    /// 采样中最久未访问条目；Least recently accessed entry within a sample.
    Lru,
    /// 采样中衰减后频率最低条目；Lowest decayed access frequency within a sample.
    Lfu,
    /// 均匀随机选择候选；Select a random candidate uniformly.
    Random,
    /// 仅选有 TTL 的候选中最早到期条目；Earliest deadline among candidates with TTL.
    TtlFirst,
}

impl EvictionPolicy {
    /// 用于配置和诊断的稳定名称。
    /// Stable name used by configuration and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoEviction => "noeviction",
            Self::Lru => "lru",
            Self::Lfu => "lfu",
            Self::Random => "random",
            Self::TtlFirst => "ttl-first",
        }
    }
}

impl FromStr for EvictionPolicy {
    type Err = DbError;
    /// 只接受文档声明的精确小写配置值。
    /// Accepts only the exact lowercase settings documented by the configuration contract.
    fn from_str(value: &str) -> Result<Self, DbError> {
        match value {
            "noeviction" => Ok(Self::NoEviction),
            "lru" => Ok(Self::Lru),
            "lfu" => Ok(Self::Lfu),
            "random" => Ok(Self::Random),
            "ttl-first" => Ok(Self::TtlFirst),
            _ => Err(error(ErrorCode::ConfigInvalid, "Invalid eviction policy")),
        }
    }
}

/// 热度数据仅属于内部淘汰策略，不是用户状态。
/// Heat metadata belongs only to the internal eviction policy, not user-visible state.
#[derive(Clone, Copy)]
pub(crate) struct Access {
    /// 单调时间毫秒的最后访问点；Last access in monotonic milliseconds.
    pub last: u64,
    /// 饱和计数；Saturating frequency counter.
    pub frequency: u64,
    /// 上次衰减的单调分钟；Monotonic minute of the last decay.
    pub minute: u64,
}

impl Access {
    /// 每分钟右移一位衰减，最多 63 位，避免长期空闲条目永久热门。
    /// Decays by one bit per minute, at most 63 bits, so idle entries do not stay permanently hot.
    pub fn decay(self, millis: u64) -> u64 {
        self.frequency >> (millis / 60_000).saturating_sub(self.minute).min(63)
    }
}

impl Transaction<'_> {
    /// 根据最终预算选择足够的受害者；失败时原始索引完全不变。
    /// Selects enough victims for final budgets; failure leaves every original index unchanged.
    pub(crate) fn victims(
        &self,
        protected: &BTreeMap<u16, HashSet<String>>,
        memory: &mut usize,
        counts: &mut BTreeMap<u16, usize>,
        original_memory: usize,
        now: Instant,
    ) -> Result<Vec<(u16, String)>, DbError> {
        let mut victims = Vec::new();
        let mut chosen: BTreeMap<u16, HashSet<String>> = BTreeMap::new();
        loop {
            let quota_db = counts
                .iter()
                .find_map(|(db, count)| (*count > self.config.max_keys).then_some(*db));
            let memory_full = self.config.max_memory_bytes != 0
                && *memory > self.config.max_memory_bytes
                && *memory > original_memory;
            if quota_db.is_none() && !memory_full {
                return Ok(victims);
            }
            if self.config.eviction == EvictionPolicy::NoEviction {
                return Err(budget_error());
            }
            let candidates = self
                .guards
                .iter()
                .flat_map(|(id, index)| {
                    let db = (*id / self.config.shard_count as usize) as u16;
                    index.iter().map(move |(key, entry)| (db, key, entry))
                })
                .filter(|(db, key, entry)| {
                    quota_db.is_none_or(|target| *db == target)
                        && entry.metadata.deadline.is_none_or(|d| now < d.monotonic)
                        && !protected.get(db).is_some_and(|set| set.contains(*key))
                        && !chosen.get(db).is_some_and(|set| set.contains(*key))
                        && (self.config.eviction != EvictionPolicy::TtlFirst
                            || entry.metadata.deadline.is_some())
                });
            let mut rng = rand::rng();
            let sample = if self.config.eviction == EvictionPolicy::Random {
                // 多项蓄水池的返回顺序不随机，不能取第一项冒充均匀选择。
                // A multi-item reservoir is not randomly ordered, so its first item is not a uniform choice.
                candidates.choose(&mut rng).into_iter().collect()
            } else if self.config.eviction == EvictionPolicy::TtlFirst {
                // ttl-first 承诺最早期限，必须检查所有有期限候选。
                // ttl-first promises the earliest deadline, so inspect all eligible timed candidates.
                candidates
                    .min_by_key(|(_, _, e)| e.metadata.deadline.unwrap().monotonic)
                    .into_iter()
                    .collect()
            } else {
                candidates.choose_multiple(&mut rng, self.config.sample_size)
            };
            let millis = self.engine.millis(now);
            let selected = match self.config.eviction {
                EvictionPolicy::Lru => sample
                    .into_iter()
                    .min_by_key(|(_, _, e)| e.access.get().last),
                EvictionPolicy::Lfu => sample
                    .into_iter()
                    .min_by_key(|(_, _, e)| (e.access.get().decay(millis), e.access.get().last)),
                _ => sample.into_iter().next(),
            }
            .ok_or_else(budget_error)?;
            let (db, key, entry) = selected;
            *memory -= entry.bytes;
            if let Some(count) = counts.get_mut(&db) {
                *count -= 1;
            }
            chosen.entry(db).or_default().insert(key.clone());
            victims.push((db, key.clone()));
        }
    }
}
