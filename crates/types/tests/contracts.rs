//! 类型契约：精度、相等、哈希、深度与稳定错误码。
//! Type contracts: precision, equality, hashing, depth, and stable error codes.

use linger_types::{ErrorCode, Float, Scalar, Value, ValueLimits};
use std::collections::{BTreeMap, HashSet};

/// 类型敏感的相等与哈希一致，拒绝 NaN 并归一化正负零。
/// Type-sensitive equality agrees with hashing, rejects NaN, and canonicalizes signed zero.
#[test]
fn scalar_equality_and_hash_preserve_types() {
    let members = HashSet::from([
        Scalar::Null,
        Scalar::Bool(true),
        Scalar::Int(1),
        Scalar::UInt(1),
        Scalar::Float(Float::new(1.0).unwrap()),
        Scalar::String("a".into()),
        Scalar::Bytes(b"a".to_vec()),
        Scalar::Float(Float::new(-0.0).unwrap()),
        Scalar::Float(Float::new(0.0).unwrap()),
    ]);
    assert_eq!(members.len(), 8);
    assert_eq!(Float::new(-0.0).unwrap().get().to_bits(), 0);
    assert!(Float::new(f64::NAN).is_err());
    assert_ne!(Float::new(f64::INFINITY), Float::new(f64::NEG_INFINITY));
    assert_ne!(Value::Null, Value::Bool(false));
    assert_ne!(
        Ok(Value::Null),
        Err(linger_types::DbError::new(
            ErrorCode::KeyNotFound,
            "missing"
        ))
    );
}

/// 容器保留顺序或集合语义，非法名称、大小和深度均拒绝。
/// Containers retain ordered or set semantics and reject invalid names, sizes, and depths.
#[test]
fn container_equality_and_validation() {
    let a = Value::Map(BTreeMap::from([
        ("x".into(), Value::Int(i64::MIN)),
        ("y".into(), Value::UInt(u64::MAX)),
    ]));
    let b = Value::Map(BTreeMap::from([
        ("y".into(), Value::UInt(u64::MAX)),
        ("x".into(), Value::Int(i64::MIN)),
    ]));
    assert_eq!(a, b);
    assert_eq!(a.validate(ValueLimits::default()).unwrap(), 35);
    let value = Value::List(vec![Value::Set(HashSet::from([Scalar::Null]))]);
    assert!(
        value
            .validate(ValueLimits {
                max_depth: 1,
                ..ValueLimits::default()
            })
            .is_err()
    );
    assert!(
        value
            .validate(ValueLimits {
                max_depth: 2,
                ..ValueLimits::default()
            })
            .is_ok()
    );
    assert!(
        Value::Map(BTreeMap::from([("".into(), Value::Null)]))
            .validate(ValueLimits::default())
            .is_err()
    );
    assert!(Scalar::try_from(Value::List(vec![])).is_err());
    assert_eq!(
        Value::String("中".into())
            .validate(ValueLimits::default())
            .unwrap(),
        8
    );
    assert!(
        Value::String("中".into())
            .validate(ValueLimits {
                max_bytes: 7,
                ..ValueLimits::default()
            })
            .is_err()
    );
    assert!(ValueLimits::default().validate_key("").is_err());
}

/// 空容器仍消耗一层业务深度。
/// Empty containers still consume one business nesting level.
#[test]
fn empty_containers_count_as_a_level() {
    for value in [
        Value::List(vec![]),
        Value::Map(BTreeMap::new()),
        Value::Set(HashSet::new()),
    ] {
        assert!(
            value
                .validate(ValueLimits {
                    max_depth: 0,
                    ..ValueLimits::default()
                })
                .is_err()
        );
        assert_eq!(value.validate(ValueLimits::default()).unwrap(), 5);
    }
}

/// 注册表错误码唯一且使用固定线上前缀。
/// Registered error codes are unique and use the fixed wire prefix.
#[test]
fn error_codes_are_unique_and_stable() {
    let codes: HashSet<_> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    assert_eq!(codes.len(), ErrorCode::ALL.len());
    assert_eq!(ErrorCode::CommitUnknown.as_str(), "ERR_COMMIT_UNKNOWN");
}
