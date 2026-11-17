//! 固定种子的独立键值参考模型，逐步核对类型、TTL 与失败原子性。
//! Seeded independent key/value model checking types, TTL, and failure atomicity at every step.

use ling_engine::{Clock, Config, Engine, Metadata, TimeSnapshot};
use ling_types::{DbError, Float, Scalar, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// 墙钟可独立跳变，单调时钟只前进；Wall time can jump independently; monotonic time only advances.
struct ModelClock(Mutex<TimeSnapshot>);
impl Clock for ModelClock {
    /// 返回同一次配对观测；Returns one coherent paired observation.
    fn now(&self) -> Result<TimeSnapshot, DbError> {
        Ok(*self.0.lock().unwrap())
    }
}

/// 参考模型仅存值与单调毫秒期限，不复用引擎的索引或期限实现。
/// The model stores only values and monotonic millisecond deadlines, without engine internals.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    value: Value,
    deadline: Option<i64>,
}

/// 无外部随机状态，失败日志中的种子可直接重放；No external random state; logged seeds replay directly.
struct Random(u64);
impl Random {
    /// xorshift64 的固定序列；A fixed xorshift64 sequence.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// 构造文本参数；Builds a text argument.
fn s(value: &str) -> Value {
    Value::String(value.into())
}

/// 只允许确实缺失映射为 None，其他错误必须暴露。
/// Only real absence maps to None; every other error must surface.
fn identities(engine: &Engine) -> Vec<Option<Metadata>> {
    (0..10)
        .map(|n| match engine.metadata(0, &format!("k{n}")) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.code == "ERR_KEY_NOT_FOUND" => None,
            Err(error) => panic!("unexpected metadata error: {error:?}"),
        })
        .collect()
}

/// 覆盖全部十种标签、嵌套容器与数值边界；Covers all ten tags, nested containers, and numeric edges.
fn value(index: u64) -> Value {
    match index % 10 {
        0 => Value::Null,
        1 => Value::Bool(index % 2 == 1),
        2 => Value::Int(if index % 3 == 0 { i64::MAX } else { -7 }),
        3 => Value::UInt(if index % 3 == 0 { u64::MAX } else { 0 }),
        4 => Value::Float(Float::new(-0.0).unwrap()),
        5 => s("中文"),
        6 => Value::Bytes(vec![0, 255, index as u8]),
        7 => Value::List(vec![Value::Null, Value::Int(1)]),
        8 => Value::Map(BTreeMap::from([("f".into(), Value::Null)])),
        _ => Value::Set(HashSet::from([Scalar::Int(1), Scalar::UInt(1)])),
    }
}

/// 用命令规范直接解释模型，不调用任何引擎命令辅助函数。
/// Interprets the documented commands directly without any engine command helpers.
fn model(
    entries: &mut BTreeMap<String, Entry>,
    key: &str,
    command: &str,
    args: &[Value],
    now: i64,
    wall: i64,
) -> Result<Value, &'static str> {
    match command {
        "SET" => {
            entries.insert(
                key.into(),
                Entry {
                    value: args[1].clone(),
                    deadline: None,
                },
            );
            return Ok(Value::Bool(true));
        }
        "MSET" => return Err("ERR_INVALID_ARGUMENT"),
        "DEL" => return Ok(Value::UInt(u64::from(entries.remove(key).is_some()))),
        "PTTL" => {
            return Ok(Value::Int(entries.get(key).map_or(-2, |entry| {
                entry.deadline.map_or(-1, |deadline| deadline - now)
            })));
        }
        "PEXPIRE" | "PEXPIREAT" => {
            let Some(entry) = entries.get_mut(key) else {
                return Ok(Value::Bool(false));
            };
            let Value::Int(input) = args[1] else {
                unreachable!()
            };
            let remaining = if command == "PEXPIRE" {
                input
            } else {
                input - wall
            };
            if remaining <= 0 {
                entries.remove(key);
            } else {
                entry.deadline = Some(now + remaining);
            }
            return Ok(Value::Bool(true));
        }
        "PERSIST" => {
            return Ok(Value::Bool(
                entries
                    .get_mut(key)
                    .is_some_and(|entry| entry.deadline.take().is_some()),
            ));
        }
        _ => {}
    }
    let entry = entries.get_mut(key).ok_or("ERR_KEY_NOT_FOUND")?;
    match (command, &mut entry.value) {
        ("GET", value) => Ok(value.clone()),
        ("INCRBY", value @ (Value::Int(_) | Value::UInt(_))) => {
            let Value::Int(delta) = args[1] else {
                unreachable!()
            };
            let result = match value {
                Value::Int(old) => {
                    i64::try_from(i128::from(*old) + i128::from(delta)).map(Value::Int)
                }
                Value::UInt(old) => {
                    u64::try_from(i128::from(*old) + i128::from(delta)).map(Value::UInt)
                }
                _ => unreachable!(),
            }
            .map_err(|_| "ERR_OVERFLOW")?;
            *value = result.clone();
            Ok(result)
        }
        ("APPEND", Value::String(text)) => {
            let Value::String(suffix) = &args[1] else {
                unreachable!()
            };
            text.push_str(suffix);
            Ok(Value::UInt(text.len() as u64))
        }
        ("BLEN", Value::Bytes(bytes)) => Ok(Value::UInt(bytes.len() as u64)),
        ("RPUSH", Value::List(items)) => {
            items.push(args[1].clone());
            Ok(Value::UInt(items.len() as u64))
        }
        ("LPOP", Value::List(items)) => Ok(Value::List(if items.is_empty() {
            Vec::new()
        } else {
            vec![items.remove(0)]
        })),
        ("FSET", Value::Map(fields)) => {
            let Value::String(field) = &args[1] else {
                unreachable!()
            };
            Ok(Value::UInt(u64::from(
                fields.insert(field.clone(), args[2].clone()).is_none(),
            )))
        }
        ("FDEL", Value::Map(fields)) => {
            let Value::String(field) = &args[1] else {
                unreachable!()
            };
            Ok(Value::UInt(u64::from(fields.remove(field).is_some())))
        }
        ("SADD" | "SREM", Value::Set(members)) => {
            let scalar = Scalar::try_from(args[1].clone()).unwrap();
            Ok(Value::UInt(u64::from(if command == "SADD" {
                members.insert(scalar)
            } else {
                members.remove(&scalar)
            })))
        }
        _ => Err("ERR_TYPE_MISMATCH"),
    }
}

/// 每步检查所有键和剩余期限，并在失败后核对元数据与值完全不变。
/// Checks every key and deadline after each step, including unchanged metadata and values on failure.
#[test]
fn seeded_model_preserves_values_deadlines_and_failed_mutations() {
    for seed in [1, 7, 42, 20261117, 0xdead_beef, 0xa11c_e551] {
        let mut random = Random(seed);
        let clock = Arc::new(ModelClock(Mutex::new(TimeSnapshot {
            monotonic: Instant::now(),
            unix_ms: 1_700_000_000_000,
        })));
        let engine = Engine::with_clock(
            Config {
                databases: 1,
                shard_count: 4,
                ..Config::default()
            },
            u128::from(seed),
            clock.clone(),
        )
        .unwrap();
        let mut entries = BTreeMap::<String, Entry>::new();
        let mut now = 0i64;
        for step in 0..1200 {
            let advance = (random.next() % 5) as i64;
            now += advance;
            let wall = {
                let mut time = clock.0.lock().unwrap();
                time.monotonic += Duration::from_millis(advance as u64);
                time.unix_ms += (random.next() % 101) as i64 - 50;
                time.unix_ms
            };
            entries.retain(|_, entry| entry.deadline.is_none_or(|deadline| deadline > now));
            let key = format!("k{}", random.next() % 10);
            let mut args = vec![s(&key)];
            let command = match random.next() % 20 {
                0..=3 => {
                    args.push(value(random.next()));
                    "SET"
                }
                4 => "GET",
                5 => {
                    args.push(Value::Int((random.next() % 7) as i64 - 3));
                    "INCRBY"
                }
                6 => {
                    args.push(s("尾"));
                    "APPEND"
                }
                7 => {
                    args.push(value(random.next()));
                    "RPUSH"
                }
                8 => "LPOP",
                9 => {
                    args.extend([s("f"), value(random.next())]);
                    "FSET"
                }
                10 => {
                    args.push(s("f"));
                    "FDEL"
                }
                11 => {
                    args.push(Value::Int((random.next() % 3) as i64));
                    "SADD"
                }
                12 => {
                    args.push(Value::UInt(random.next() % 3));
                    "SREM"
                }
                13 => {
                    args.push(Value::Int((random.next() % 31) as i64 - 2));
                    "PEXPIRE"
                }
                14 => {
                    args.push(Value::Int(wall + (random.next() % 31) as i64 - 2));
                    "PEXPIREAT"
                }
                15 => "PERSIST",
                16 => "DEL",
                17 => "PTTL",
                18 => {
                    args.extend([Value::Null, s(&key), Value::Int(1)]);
                    "MSET"
                }
                _ => "BLEN",
            };
            // 先触发逻辑过期，再快照；失败原子性不把先前已到期的数据当作有效状态。
            // Trigger logical expiry before snapshotting; already-expired data is not live failure state.
            let metadata = identities(&engine);
            let before = entries.clone();
            let expected = model(&mut entries, &key, command, &args, now, wall);
            let actual = engine
                .execute(0, command, &args, 1024 * 1024)
                .map_err(|error| error.code);
            assert_eq!(
                actual.as_ref().map_err(String::as_str),
                expected.as_ref().map_err(|code| *code),
                "seed={seed}, step={step}, command={command}, args={args:?}"
            );
            if expected.is_err() {
                assert_eq!(
                    entries, before,
                    "model failure mutated state; seed={seed}, step={step}"
                );
                let after = identities(&engine);
                assert_eq!(
                    after, metadata,
                    "failure mutated identity; seed={seed}, step={step}"
                );
            }
            for n in 0..10 {
                let observed_key = format!("k{n}");
                let actual = engine
                    .execute(0, "GET", &[s(&observed_key)], 1024 * 1024)
                    .map_err(|e| e.code);
                let expected = entries
                    .get(&observed_key)
                    .map(|entry| entry.value.clone())
                    .ok_or("ERR_KEY_NOT_FOUND");
                assert_eq!(
                    actual.as_ref().map_err(String::as_str),
                    expected.as_ref().map_err(|code| *code),
                    "value seed={seed}, step={step}, key={observed_key}"
                );
                let ttl = entries.get(&observed_key).map_or(-2, |entry| {
                    entry.deadline.map_or(-1, |deadline| deadline - now)
                });
                assert_eq!(
                    engine
                        .execute(0, "PTTL", &[s(&observed_key)], 1024)
                        .unwrap(),
                    Value::Int(ttl),
                    "TTL seed={seed}, step={step}, key={observed_key}"
                );
            }
        }
    }
}
