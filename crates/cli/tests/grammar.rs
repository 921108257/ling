//! CLI 文本契约：类型标签、完整整数精度和明确参数边界。
//! CLI text contracts: type tags, full integer precision, and explicit argument boundaries.

use linger_cli::{display_value, parse_command, parse_value};
use linger_types::{Float, Scalar, Value};
use std::collections::{BTreeMap, HashSet};

/// 十种 Value 的展示可逆，保留整数、Bytes 和 Float 区别。
/// Rendering round trips all ten Values while distinguishing integers, Bytes, and Float.
#[test]
fn typed_values_display_without_losing_precision_or_type() {
    let values = [
        Value::Null,
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::UInt(u64::MAX),
        Value::Float(Float::new(f64::INFINITY).unwrap()),
        Value::String("中\n\"文".into()),
        Value::Bytes(vec![0, 255]),
        Value::List(vec![Value::Null]),
        Value::Map(BTreeMap::from([("x".into(), Value::UInt(u64::MAX))])),
        Value::Set(HashSet::from([Scalar::Bytes(vec![0, 255]), Scalar::Int(1)])),
    ];
    for value in values {
        assert_eq!(parse_value(&display_value(&value)).unwrap(), value);
    }
    assert_eq!(
        parse_value("bytes(b64\"AP8=\")").unwrap(),
        Value::Bytes(vec![0, 255])
    );
    assert_eq!(
        display_value(&Value::Float(Float::new(-0.0).unwrap())),
        "float(0)"
    );
    assert_ne!(
        display_value(&Value::Int(1)),
        display_value(&Value::UInt(1))
    );
}

/// 拒绝隐式数字、无效转义、重复字段和过深递归。
/// Rejects implicit numbers, invalid escapes, duplicate fields, and excessive recursion.
#[test]
fn rejects_invalid_or_ambiguous_literals() {
    for literal in [
        "1",
        "int(0xff)",
        "uint(-1)",
        "uint(18446744073709551616)",
        "float(NaN)",
        "float(1e999)",
        "float(INF)",
        "bytes(0x1)",
        "bytes(b64\"AP8\")",
        "\"\\ud800\"",
        "map({\"x\": null, \"x\": true})",
        "map({\"\": null})",
        "set([list([])])",
        "true false",
        "list([null,])",
    ] {
        assert!(parse_value(literal).is_err(), "{literal}");
    }
    let deep = format!("{}null{}", "list([".repeat(9), "])".repeat(9));
    assert!(parse_value(&deep).is_err());
}

/// 连接命令按签名解析，错误与 Debug 不回显密码。
/// Connection commands follow signatures and errors or Debug never echo passwords.
#[test]
fn command_signatures_and_secret_safe_errors() {
    let request = parse_command(" pInG \"hello world\" ").unwrap();
    assert_eq!(request.command, "PING");
    assert_eq!(request.arguments, vec![Value::String("hello world".into())]);
    assert_eq!(
        parse_command("SELECT 65535").unwrap().arguments,
        vec![Value::UInt(65535)]
    );
    assert_eq!(
        parse_command("AUTH secret").unwrap().arguments,
        vec![Value::String("secret".into())]
    );
    assert_eq!(
        parse_command("AUTH \"secret").unwrap_err().to_string(),
        "命令或值语法错误"
    );
    assert!(parse_command("SELECT -1").is_err());
    assert!(!format!("{:?}", parse_command("AUTH secret").unwrap()).contains("secret"));
}

/// 业务值保留显式类型，索引和计数依签名解析。
/// Business values retain explicit types while indices and counts follow their signatures.
#[test]
fn data_command_signatures_and_argument_boundaries() {
    assert_eq!(
        parse_command("MSET first null second uint(18446744073709551615)")
            .unwrap()
            .arguments,
        vec![
            Value::String("first".into()),
            Value::Null,
            Value::String("second".into()),
            Value::UInt(u64::MAX)
        ]
    );
    assert_eq!(
        parse_command("LRANGE items -2 -1").unwrap().arguments,
        vec![
            Value::String("items".into()),
            Value::Int(-2),
            Value::Int(-1)
        ]
    );
    assert_eq!(
        parse_command("LPOP items 2").unwrap().arguments,
        vec![Value::String("items".into()), Value::UInt(2)]
    );
    assert_eq!(
        parse_command("LSET items 0 map({\"nested\": true})")
            .unwrap()
            .arguments,
        vec![
            Value::String("items".into()),
            Value::Int(0),
            Value::Map(BTreeMap::from([("nested".into(), Value::Bool(true))]))
        ]
    );
    for source in [
        "SET key 1",
        "SET key nulljunk",
        "SET key int(1)XX",
        "FSET key field trueoops",
        "LPOP key -1",
        "SRANDMEMBER key 9223372036854775808",
    ] {
        assert!(parse_command(source).is_err(), "{source}");
    }
}
