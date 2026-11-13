use linger_types::{ErrorCode, Float, Scalar, Value, ValueLimits};
use std::collections::{BTreeMap, HashSet};

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

#[test]
fn error_codes_are_unique_and_stable() {
    let codes: HashSet<_> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    assert_eq!(codes.len(), ErrorCode::ALL.len());
    assert_eq!(ErrorCode::CommitUnknown.as_str(), "ERR_COMMIT_UNKNOWN");
}
