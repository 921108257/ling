use linger_protocol::*;
use linger_types::{DbError, ErrorCode, Float, Scalar, Value};
use std::collections::{BTreeMap, HashSet};

fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn value(name: &str) -> Value {
    match name {
        "null" => Value::Null,
        "bool_false" => Value::Bool(false),
        "bool_true" => Value::Bool(true),
        "int_min" => Value::Int(i64::MIN),
        "int_max" => Value::Int(i64::MAX),
        "uint_max" => Value::UInt(u64::MAX),
        "float_inf" => Value::Float(Float::new(f64::INFINITY).unwrap()),
        "float_neg_inf" => Value::Float(Float::new(f64::NEG_INFINITY).unwrap()),
        "float_zero" => Value::Float(Float::new(-0.0).unwrap()),
        "string_unicode" => Value::String("中".into()),
        "string_empty" => Value::String(String::new()),
        "bytes" => Value::Bytes(vec![0, 255, 128]),
        "bytes_empty" => Value::Bytes(vec![]),
        "list" => Value::List(vec![Value::Null, Value::UInt(u64::MAX)]),
        "map" => Value::Map(BTreeMap::from([("x".into(), Value::Bool(true))])),
        "set" => Value::Set(HashSet::from([Scalar::Bytes(vec![0, 255])])),
        "list_empty" => Value::List(vec![]),
        "map_empty" => Value::Map(BTreeMap::new()),
        "set_empty" => Value::Set(HashSet::new()),
        _ => panic!("Unknown fixture {name}"),
    }
}

fn event_id() -> Value {
    Value::Map(BTreeMap::from([
        (
            "stream_id".into(),
            Value::Bytes(1u128.to_be_bytes().to_vec()),
        ),
        ("db".into(), Value::UInt(0)),
        ("shard".into(), Value::UInt(0)),
        ("sequence".into(), Value::UInt(1)),
    ]))
}

fn frame(name: &str) -> Frame {
    let (id, body) = match name {
        "hello" => (
            1,
            Body::Request(Request::new("HELLO", vec![Value::UInt(1)])),
        ),
        "ping" => (2, Body::Request(Request::new("PING", vec![]))),
        "pong" => (2, Body::Response(Ok(Value::String("PONG".into())))),
        "error" => (2, Body::Error(DbError::new(ErrorCode::Protocol, "bad"))),
        "batch_request" => (
            3,
            Body::BatchRequest(vec![Request::new("GET", vec![Value::String("k".into())])]),
        ),
        "batch_response" => (3, Body::BatchResponse(vec![Ok(Value::Null)])),
        "heartbeat" => (0, Body::Heartbeat),
        "ack" => (4, Body::Ack(event_id())),
        "event" => (
            0,
            Body::Event(Value::Map(BTreeMap::from([
                ("event_id".into(), event_id()),
                ("event_type".into(), Value::String("KEY_CREATED".into())),
                ("key".into(), Value::String("k".into())),
                (
                    "generation".into(),
                    Value::Map(BTreeMap::from([
                        ("epoch".into(), Value::Bytes(2u128.to_be_bytes().to_vec())),
                        ("counter".into(), Value::UInt(1)),
                    ])),
                ),
                ("revision".into(), Value::UInt(1)),
                ("timestamp_ms".into(), Value::UInt(0)),
            ]))),
        ),
        _ => panic!("Unknown fixture {name}"),
    };
    Frame {
        request_id: id,
        body,
    }
}

#[test]
fn independent_value_golden_vectors() {
    for line in include_str!("../../../tests/fixtures/protocol/values.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
    {
        let (name, wire) = line.split_once('|').unwrap();
        let expected = value(name);
        let bytes = hex(wire);
        assert_eq!(
            decode_value(&bytes, Limits::default()).unwrap(),
            expected,
            "{name}"
        );
        assert_eq!(
            encode_value(&expected, Limits::default()).unwrap(),
            bytes,
            "{name}"
        );
    }
}

#[test]
fn independent_frame_golden_vectors() {
    for line in include_str!("../../../tests/fixtures/protocol/frames.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
    {
        let (name, wire) = line.split_once('|').unwrap();
        let expected = frame(name);
        let bytes = hex(wire);
        assert_eq!(
            decode_frame(&bytes, Limits::default()).unwrap(),
            expected,
            "{name}"
        );
        // Map order is not a wire guarantee; compare the re-encoded data semantically.
        let encoded = encode_frame(&expected, Limits::default()).unwrap();
        if !matches!(name, "event" | "ack") {
            assert_eq!(encoded, bytes, "{name}");
        }
        assert_eq!(decode_frame(&encoded, Limits::default()).unwrap(), expected);
    }
}

#[test]
fn invalid_value_vectors_distinguish_semantic_errors() {
    for line in include_str!("../../../tests/fixtures/protocol/invalid-values.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
    {
        let parts: Vec<_> = line.split('|').collect();
        let err = decode_value(&hex(parts[3]), Limits::default()).unwrap_err();
        assert_eq!(err.fatal, parts[1] == "true", "{}", parts[0]);
        assert_eq!(err.error.code, parts[2], "{}", parts[0]);
    }
}

#[test]
fn flags_versions_ids_and_lengths_are_checked_before_payload_allocation() {
    let wire = encode_frame(&frame("hello"), Limits::default()).unwrap();
    for (offset, bad) in [(0, 0), (2, 2), (3, 0), (4, 128), (5, 2), (13, 0)] {
        let mut bytes = wire.clone();
        bytes[offset] = bad;
        let err = decode_frame(&bytes, Limits::default()).unwrap_err();
        assert!(err.fatal);
        assert_eq!(
            err.error.code,
            if offset == 2 {
                "ERR_UNSUPPORTED_VERSION"
            } else {
                "ERR_PROTOCOL"
            }
        );
    }
    let mut bytes = wire.clone();
    bytes[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(parse_header(bytes[..18].try_into().unwrap(), Limits::default()).is_err());
    for end in 0..wire.len() {
        assert!(decode_frame(&wire[..end], Limits::default()).is_err());
    }
    let mut bytes = wire;
    bytes.push(0);
    assert!(decode_frame(&bytes, Limits::default()).is_err());
}

#[test]
fn invalid_business_values_are_isolated_and_entire_frame_is_checked() {
    let mut payload = hex("05 00000003 534554 07 00000002 05 00000001 6b 04 7ff8000000000001");
    let mut wire = hex("4c47 01 01 0000 0000000000000001 00000000");
    wire[14..18].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    wire.append(&mut payload);
    let decoded = decode_frame(&wire, Limits::default()).unwrap();
    let Body::Request(request) = decoded.body else {
        panic!()
    };
    assert_eq!(request.value_error.unwrap().code, "ERR_INVALID_VALUE");
    wire.push(255);
    let len = (wire.len() - 18) as u32;
    wire[14..18].copy_from_slice(&len.to_be_bytes());
    assert!(decode_frame(&wire, Limits::default()).unwrap_err().fatal);
}

#[test]
fn decoding_is_bounded_and_envelopes_do_not_count_as_business_depth() {
    let limits = Limits {
        max_decode_bytes: 1,
        ..Limits::default()
    };
    assert!(
        decode_value(&hex("07 00000001 00"), limits)
            .unwrap_err()
            .fatal
    );
    let limits = Limits {
        value: linger_types::ValueLimits {
            max_depth: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let f = Frame {
        request_id: 1,
        body: Body::Request(Request::new("SET", vec![Value::List(vec![Value::Null])])),
    };
    let bytes = encode_frame(&f, limits).unwrap();
    assert_eq!(decode_frame(&bytes, limits).unwrap(), f);
    let deep = hex("07 00000001 07 00000001 07 00000001 07 00000001 07 00000001 07 00000000");
    assert!(decode_value(&deep, limits).unwrap_err().fatal);
    let zeros = decode_value(
        &hex("09 00000002 04 8000000000000000 04 0000000000000000"),
        Limits::default(),
    )
    .unwrap();
    let Value::Set(members) = zeros else { panic!() };
    assert_eq!(members.len(), 1);
}

#[test]
fn small_map_and_set_tables_are_charged_before_allocation() {
    let limits = Limits {
        max_decode_bytes: 64,
        ..Limits::default()
    };
    for wire in ["08 00000001 05 00000001 78 00", "09 00000001 00"] {
        assert!(decode_value(&hex(wire), limits).unwrap_err().fatal);
    }
}

#[test]
fn batch_business_error_is_isolated_but_truncated_tail_rejects_all() {
    let payload = hex(
        "00000002 05 00000003 534554 07 00000002 05 00000001 6b 04 7ff8000000000001 05 00000003 474554 07 00000001 05 00000001 6b",
    );
    let mut wire = hex("4c47 01 01 0001 0000000000000001 00000000");
    wire[14..18].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    wire.extend(payload);
    let Body::BatchRequest(items) = decode_frame(&wire, Limits::default()).unwrap().body else {
        panic!()
    };
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].value_error.as_ref().unwrap().code,
        "ERR_INVALID_VALUE"
    );
    assert!(items[1].value_error.is_none());
    assert_eq!(items[1].command, "GET");
    wire.pop();
    let len = (wire.len() - 18) as u32;
    wire[14..18].copy_from_slice(&len.to_be_bytes());
    assert!(decode_frame(&wire, Limits::default()).unwrap_err().fatal);
}

#[tokio::test]
async fn fragmented_and_coalesced_frames_and_truncated_eof() {
    use tokio::io::AsyncWriteExt;
    let first = encode_frame(&frame("hello"), Limits::default()).unwrap();
    let second = encode_frame(&frame("ping"), Limits::default()).unwrap();
    for split in 1..first.len() {
        let (mut tx, mut rx) = tokio::io::duplex(256);
        let (a, b) = (first[..split].to_vec(), first[split..].to_vec());
        let second = second.clone();
        let writer = tokio::spawn(async move {
            tx.write_all(&a).await.unwrap();
            tokio::task::yield_now().await;
            tx.write_all(&b).await.unwrap();
            tx.write_all(&second).await.unwrap();
        });
        assert_eq!(
            read_frame(&mut rx, Limits::default())
                .await
                .unwrap()
                .unwrap(),
            frame("hello")
        );
        assert_eq!(
            read_frame(&mut rx, Limits::default())
                .await
                .unwrap()
                .unwrap(),
            frame("ping")
        );
        assert!(
            read_frame(&mut rx, Limits::default())
                .await
                .unwrap()
                .is_none()
        );
        writer.await.unwrap();
    }
    let (mut tx, mut rx) = tokio::io::duplex(256);
    tx.write_all(&first[..first.len() - 1]).await.unwrap();
    drop(tx);
    assert!(matches!(
        read_frame(&mut rx, Limits::default()).await,
        Err(ReadError::Decode(_))
    ));
}
