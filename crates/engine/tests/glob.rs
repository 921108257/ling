//! 三条 Key 筛选路径共享字节通配语义。
//! All three key-filtering paths share byte-glob semantics.

use ling_engine::{Config, Engine, ScanState, SystemClock};
use ling_events::EventStore;
use ling_types::Value;
use std::{collections::BTreeSet, sync::Arc, time::Instant};

/// 构造文本参数；Builds a text argument.
fn text(value: &str) -> Value {
    Value::String(value.into())
}

/// 提取无顺序承诺的 Key 集合；Extracts an unordered key set.
fn keys(value: Value) -> BTreeSet<String> {
    let Value::List(values) = value else {
        panic!("expected key list");
    };
    values
        .into_iter()
        .map(|value| {
            let Value::String(key) = value else {
                panic!("expected key string");
            };
            key
        })
        .collect()
}

/// 独立预期覆盖重叠片段、空匹配、转义及 UTF-8 字节边界。
/// Independent expectations cover overlapping fragments, empty matches, escapes, and UTF-8 byte boundaries.
#[test]
fn keys_scan_and_subscribe_agree_on_byte_globs() {
    let stored = [
        "xyz",
        "xyyz",
        "xyQyz",
        "xyxyyz",
        "aba",
        "abaXaba",
        "雪",
        "雪a",
        "雪猫",
        "a雪b",
        "a*b",
        "a?b",
        r"a\b",
        "prefix",
        "prefix:tail",
    ];
    let cases: &[(&str, &[&str])] = &[
        ("", &[]),
        ("*xy*yz", &["xyyz", "xyQyz", "xyxyyz"]),
        ("**xy**yz", &["xyyz", "xyQyz", "xyxyyz"]),
        ("*aba*aba", &["abaXaba"]),
        ("雪*", &["雪", "雪a", "雪猫"]),
        ("雪?", &["雪a"]),
        ("?雪?", &["a雪b"]),
        ("???", &["xyz", "aba", "雪", "a*b", "a?b", r"a\b"]),
        (r"a\*b", &["a*b"]),
        (r"a\?b", &["a?b"]),
        (r"a\\b", &[r"a\b"]),
        ("prefix*", &["prefix", "prefix:tail"]),
        ("*", &stored),
    ];
    for &(pattern, expected) in cases {
        let events = Arc::new(EventStore::open(Default::default(), 1, 2).unwrap());
        let engine = Engine::with_events(
            Config {
                databases: 1,
                shard_count: 2,
                ..Default::default()
            },
            1,
            Arc::new(SystemClock),
            Some(events.clone()),
        )
        .unwrap();
        let subscription = events.subscribe(0, vec![pattern.into()], None).unwrap();
        for key in stored {
            engine
                .execute(0, "SET", &[text(key), Value::Null], 4096)
                .unwrap();
        }
        let expected: BTreeSet<String> = expected.iter().map(|key| (*key).into()).collect();
        assert_eq!(
            keys(engine.execute(0, "KEYS", &[text(pattern)], 4096).unwrap()),
            expected,
            "KEYS {pattern:?}"
        );

        let mut scan = ScanState::default();
        let mut cursor = 0;
        let mut scanned = BTreeSet::new();
        loop {
            let Value::Map(mut page) = engine
                .scan(
                    0,
                    &[
                        Value::UInt(cursor),
                        text("MATCH"),
                        text(pattern),
                        text("COUNT"),
                        Value::UInt(2),
                    ],
                    &mut scan,
                    4096,
                )
                .unwrap()
            else {
                panic!("expected scan page");
            };
            scanned.extend(keys(page.remove("keys").unwrap()));
            let Value::UInt(next) = page["cursor"] else {
                panic!("expected cursor");
            };
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(scanned, expected, "SCAN MATCH {pattern:?}");

        let mut delivered = BTreeSet::new();
        while let Some(event) = events.next(&subscription, Instant::now()).unwrap() {
            assert!(delivered.insert(event.key.clone()), "duplicate delivery");
            events.ack(&subscription, event.id).unwrap();
        }
        assert_eq!(delivered, expected, "SUBSCRIBE {pattern:?}");
    }
}

/// 三条入口均拒绝末尾转义；All three entry points reject a dangling escape.
#[test]
fn all_key_filters_reject_dangling_escape() {
    let events = Arc::new(EventStore::open(Default::default(), 1, 1).unwrap());
    let engine = Engine::new(
        Config {
            databases: 1,
            shard_count: 1,
            ..Default::default()
        },
        1,
    )
    .unwrap();
    let mut state = ScanState::default();
    for error in [
        engine
            .execute(0, "KEYS", &[text("trailing\\")], 4096)
            .unwrap_err(),
        engine
            .scan(
                0,
                &[Value::UInt(0), text("MATCH"), text("trailing\\")],
                &mut state,
                4096,
            )
            .unwrap_err(),
        events
            .subscribe(0, vec!["trailing\\".into()], None)
            .err()
            .unwrap(),
    ] {
        assert_eq!(error.code, "ERR_INVALID_ARGUMENT");
    }
}
