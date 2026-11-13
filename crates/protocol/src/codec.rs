use crate::*;
use linger_types::{Float, Scalar};
use std::collections::{BTreeMap, HashSet};
use std::mem::size_of;

pub fn parse_header(bytes: &[u8; HEADER_LEN], limits: Limits) -> Result<Header, DecodeError> {
    let id = u64::from_be_bytes(bytes[6..14].try_into().unwrap());
    let fail = |message| {
        let mut e = DecodeError::protocol(message);
        e.request_id = id;
        e
    };
    if &bytes[..2] != b"LG" {
        return Err(fail("Invalid frame magic"));
    }
    if bytes[2] != VERSION {
        return Err(DecodeError {
            error: DbError::new(
                ErrorCode::UnsupportedVersion,
                "Unsupported protocol version",
            ),
            fatal: true,
            request_id: id,
        });
    }
    let kind = bytes[3];
    if !(1..=6).contains(&kind) {
        return Err(fail("Unknown frame type"));
    }
    let flags = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
    if flags > 1 || (flags == 1 && !matches!(kind, 1 | 2)) {
        return Err(fail("Invalid frame flags"));
    }
    if (matches!(kind, 1 | 2 | 4) && id == 0) || (matches!(kind, 3 | 5) && id != 0) {
        return Err(fail("Invalid request ID"));
    }
    let len = u32::from_be_bytes(bytes[14..18].try_into().unwrap()) as usize;
    let cap = if matches!(kind, 1 | 4) {
        limits.max_request_bytes
    } else {
        limits.max_response_bytes
    };
    if len > cap
        || len
            .checked_add(HEADER_LEN)
            .is_none_or(|n| n > limits.max_frame_bytes)
    {
        return Err(fail("Frame exceeds byte limit"));
    }
    if kind == 5 && len != 0 {
        return Err(fail("Heartbeat must be empty"));
    }
    Ok(Header {
        kind,
        batch: flags == 1,
        request_id: id,
        payload_len: len,
    })
}

struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    budget: usize,
    limits: Limits,
    semantic: Option<DbError>,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], limits: Limits) -> Self {
        Self {
            bytes,
            at: 0,
            budget: limits.max_decode_bytes,
            limits,
            semantic: None,
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| DecodeError::protocol("Truncated Value"))?;
        let result = &self.bytes[self.at..end];
        self.at = end;
        Ok(result)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn count(&mut self) -> Result<usize, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn charge(&mut self, bytes: usize) -> Result<(), DecodeError> {
        self.budget = self
            .budget
            .checked_sub(bytes)
            .ok_or_else(DecodeError::limit)?;
        Ok(())
    }
    fn invalid(&mut self, code: ErrorCode, message: &'static str) {
        self.semantic
            .get_or_insert_with(|| DbError::new(code, message));
    }
    fn elements(
        &mut self,
        count: usize,
        min_wire: usize,
        memory: usize,
    ) -> Result<(), DecodeError> {
        if count > (self.bytes.len() - self.at) / min_wire {
            return Err(DecodeError::protocol("Impossible container count"));
        }
        self.charge(count.checked_mul(memory).ok_or_else(DecodeError::limit)?)
    }

    fn value(&mut self, parents: usize) -> Result<Value, DecodeError> {
        let tag = self.byte()?;
        let depth = parents + usize::from((7..=9).contains(&tag));
        if depth > self.limits.value.max_depth.saturating_add(4) {
            return Err(DecodeError::limit());
        }
        let value = match tag {
            0 => Value::Null,
            1 => match self.byte()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(DecodeError::protocol("Invalid Bool")),
            },
            2 => Value::Int(i64::from_be_bytes(self.take(8)?.try_into().unwrap())),
            3 => Value::UInt(u64::from_be_bytes(self.take(8)?.try_into().unwrap())),
            4 => match Float::new(f64::from_bits(u64::from_be_bytes(
                self.take(8)?.try_into().unwrap(),
            ))) {
                Ok(v) => Value::Float(v),
                Err(_) => {
                    self.invalid(ErrorCode::InvalidValue, "NaN is not a Value");
                    Value::Null
                }
            },
            5 | 6 => {
                let len = self.count()?;
                self.charge(len)?;
                let bytes = self.take(len)?;
                if tag == 5 {
                    Value::String(
                        std::str::from_utf8(bytes)
                            .map_err(|_| DecodeError::protocol("Invalid UTF-8"))?
                            .into(),
                    )
                } else {
                    Value::Bytes(bytes.to_vec())
                }
            }
            7 => {
                let count = self.count()?;
                self.elements(count, 1, size_of::<Value>())?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.value(depth)?);
                }
                Value::List(items)
            }
            8 => {
                let count = self.count()?;
                self.elements(
                    count,
                    6,
                    size_of::<(String, Value)>() + 6 * size_of::<usize>(),
                )?;
                let mut fields = BTreeMap::new();
                for _ in 0..count {
                    let Value::String(key) = self.value(depth)? else {
                        return Err(DecodeError::protocol("Map field name must be a String"));
                    };
                    if key.is_empty() {
                        self.invalid(ErrorCode::InvalidValue, "Empty Map field");
                    }
                    if key.len() > self.limits.value.max_key_bytes {
                        self.invalid(ErrorCode::LimitExceeded, "Map field exceeds byte limit");
                    }
                    let item = self.value(depth)?;
                    if fields.insert(key, item).is_some() {
                        self.invalid(ErrorCode::InvalidValue, "Duplicate Map field");
                    }
                }
                Value::Map(fields)
            }
            9 => {
                let count = self.count()?;
                self.elements(count, 1, 2 * size_of::<Scalar>())?;
                let mut members = HashSet::with_capacity(count);
                for _ in 0..count {
                    match Scalar::try_from(self.value(depth)?) {
                        Ok(item) => {
                            members.insert(item);
                        }
                        Err(_) => {
                            self.invalid(ErrorCode::InvalidValue, "Set members must be scalar")
                        }
                    }
                }
                Value::Set(members)
            }
            _ => return Err(DecodeError::protocol("Unknown Value tag")),
        };
        Ok(value)
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        match self.value(0)? {
            Value::String(v) => Ok(v),
            _ => Err(DecodeError::protocol("Expected String")),
        }
    }
    fn request(&mut self) -> Result<Request, DecodeError> {
        let command = self.string()?;
        if !command.is_ascii() {
            return Err(DecodeError::protocol("Command name must be ASCII"));
        }
        let Value::List(arguments) = self.value(0)? else {
            return Err(DecodeError::protocol("Expected argument List"));
        };
        // Protocol envelopes do not count against business Value depth.
        for argument in &arguments {
            if let Err(e) = argument.validate(self.limits.value) {
                self.semantic.get_or_insert(e);
            }
        }
        Ok(Request {
            command: command.to_ascii_uppercase(),
            arguments,
            value_error: self.semantic.take(),
        })
    }
    fn error(&mut self) -> Result<DbError, DecodeError> {
        Ok(DbError {
            code: self.string()?,
            message: self.string()?,
        })
    }
    fn response(&mut self) -> Result<Result<Value, DbError>, DecodeError> {
        match self.byte()? {
            0 => Ok(Ok(self.value(0)?)),
            1 => Ok(Err(self.error()?)),
            _ => Err(DecodeError::protocol("Invalid response status")),
        }
    }
    fn finish(&self) -> Result<(), DecodeError> {
        if self.at != self.bytes.len() {
            return Err(DecodeError::protocol("Trailing payload bytes"));
        }
        Ok(())
    }
}

pub fn decode_value(bytes: &[u8], limits: Limits) -> Result<Value, DecodeError> {
    if bytes.len() > limits.max_frame_bytes {
        return Err(DecodeError::limit());
    }
    let mut decoder = Decoder::new(bytes, limits);
    let value = decoder.value(0)?;
    decoder.finish()?;
    if let Some(error) = decoder.semantic {
        return Err(DecodeError {
            error,
            fatal: false,
            request_id: 0,
        });
    }
    value.validate(limits.value).map_err(|error| DecodeError {
        error,
        fatal: false,
        request_id: 0,
    })?;
    Ok(value)
}

pub fn decode_frame(bytes: &[u8], limits: Limits) -> Result<Frame, DecodeError> {
    let raw_header = bytes
        .get(..HEADER_LEN)
        .ok_or_else(|| DecodeError::protocol("Truncated frame header"))?;
    let header = parse_header(raw_header.try_into().unwrap(), limits)?;
    if bytes.len() != HEADER_LEN + header.payload_len {
        return Err(DecodeError::protocol("Incorrect payload length"));
    }
    let mut d = Decoder::new(&bytes[HEADER_LEN..], limits);
    let parse = |d: &mut Decoder<'_>| -> Result<Body, DecodeError> {
        Ok(match (header.kind, header.batch) {
            (1, false) => Body::Request(d.request()?),
            (2, false) => Body::Response(d.response()?),
            (kind @ (1 | 2), true) => {
                let count = d.count()?;
                if count == 0 || count > limits.max_batch_size {
                    return Err(DecodeError::limit());
                }
                d.elements(
                    count,
                    if kind == 1 { 10 } else { 2 },
                    size_of::<Request>().max(size_of::<Result<Value, DbError>>()),
                )?;
                if kind == 1 {
                    let mut items = Vec::with_capacity(count);
                    for _ in 0..count {
                        items.push(d.request()?);
                    }
                    Body::BatchRequest(items)
                } else {
                    let mut items = Vec::with_capacity(count);
                    for _ in 0..count {
                        items.push(d.response()?);
                    }
                    Body::BatchResponse(items)
                }
            }
            (3, false) => Body::Event(d.value(0)?),
            (4, false) => Body::Ack(d.value(0)?),
            (5, false) => Body::Heartbeat,
            (6, false) => Body::Error(d.error()?),
            _ => unreachable!(),
        })
    };
    let result = (|| {
        let body = parse(&mut d)?;
        d.finish()?;
        if d.semantic.is_some() {
            return Err(DecodeError::protocol(
                "Invalid Value in server or control frame",
            ));
        }
        Ok(Frame {
            request_id: header.request_id,
            body,
        })
    })();
    result.map_err(|mut error: DecodeError| {
        error.request_id = header.request_id;
        error
    })
}

struct Encoder {
    bytes: Vec<u8>,
    cap: usize,
}

impl Encoder {
    fn push(&mut self, bytes: &[u8]) -> Result<(), DbError> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.cap)
        {
            return Err(DbError::new(
                ErrorCode::LimitExceeded,
                "Encoded frame exceeds byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn count(&mut self, count: usize) -> Result<(), DbError> {
        self.push(
            &u32::try_from(count)
                .map_err(|_| DbError::new(ErrorCode::LimitExceeded, "Length exceeds u32"))?
                .to_be_bytes(),
        )
    }
    fn string(&mut self, text: &str) -> Result<(), DbError> {
        self.push(&[5])?;
        self.count(text.len())?;
        self.push(text.as_bytes())
    }
    fn scalar(&mut self, scalar: &Scalar) -> Result<(), DbError> {
        match scalar {
            Scalar::Null => self.push(&[0]),
            Scalar::Bool(v) => self.push(&[1, u8::from(*v)]),
            Scalar::Int(v) => {
                self.push(&[2])?;
                self.push(&v.to_be_bytes())
            }
            Scalar::UInt(v) => {
                self.push(&[3])?;
                self.push(&v.to_be_bytes())
            }
            Scalar::Float(v) => {
                self.push(&[4])?;
                self.push(&v.get().to_bits().to_be_bytes())
            }
            Scalar::String(v) => self.string(v),
            Scalar::Bytes(v) => {
                self.push(&[6])?;
                self.count(v.len())?;
                self.push(v)
            }
        }
    }
    fn value(&mut self, value: &Value, parents: usize, limits: Limits) -> Result<(), DbError> {
        let depth = parents
            + usize::from(matches!(
                value,
                Value::List(_) | Value::Map(_) | Value::Set(_)
            ));
        if depth > limits.value.max_depth.saturating_add(4) {
            return Err(DbError::new(
                ErrorCode::LimitExceeded,
                "Envelope exceeds depth limit",
            ));
        }
        match value {
            Value::Null => self.push(&[0]),
            Value::Bool(v) => self.push(&[1, u8::from(*v)]),
            Value::Int(v) => {
                self.push(&[2])?;
                self.push(&v.to_be_bytes())
            }
            Value::UInt(v) => {
                self.push(&[3])?;
                self.push(&v.to_be_bytes())
            }
            Value::Float(v) => {
                self.push(&[4])?;
                self.push(&v.get().to_bits().to_be_bytes())
            }
            Value::String(v) => self.string(v),
            Value::Bytes(v) => {
                self.push(&[6])?;
                self.count(v.len())?;
                self.push(v)
            }
            Value::List(items) => {
                self.push(&[7])?;
                self.count(items.len())?;
                for item in items {
                    self.value(item, depth, limits)?;
                }
                Ok(())
            }
            Value::Map(fields) => {
                self.push(&[8])?;
                self.count(fields.len())?;
                for (key, item) in fields {
                    if key.is_empty() {
                        return Err(DbError::new(ErrorCode::InvalidValue, "Empty Map field"));
                    }
                    if key.len() > limits.value.max_key_bytes {
                        return Err(DbError::new(
                            ErrorCode::LimitExceeded,
                            "Map field exceeds byte limit",
                        ));
                    }
                    self.string(key)?;
                    self.value(item, depth, limits)?;
                }
                Ok(())
            }
            Value::Set(items) => {
                self.push(&[9])?;
                self.count(items.len())?;
                for item in items {
                    self.scalar(item)?;
                }
                Ok(())
            }
        }
    }
    fn request(&mut self, request: &Request, limits: Limits) -> Result<(), DbError> {
        if let Some(error) = &request.value_error {
            return Err(error.clone());
        }
        if !request.command.is_ascii() {
            return Err(DbError::new(
                ErrorCode::InvalidArgument,
                "Command must be ASCII",
            ));
        }
        self.string(&request.command.to_ascii_uppercase())?;
        self.push(&[7])?;
        self.count(request.arguments.len())?;
        for argument in &request.arguments {
            argument.validate(limits.value)?;
            self.value(argument, 1, limits)?;
        }
        Ok(())
    }
    fn error(&mut self, error: &DbError) -> Result<(), DbError> {
        self.string(&error.code)?;
        self.string(&error.message)
    }
    fn response(
        &mut self,
        response: &Result<Value, DbError>,
        limits: Limits,
    ) -> Result<(), DbError> {
        match response {
            Ok(v) => {
                self.push(&[0])?;
                self.value(v, 0, limits)
            }
            Err(e) => {
                self.push(&[1])?;
                self.error(e)
            }
        }
    }
}

pub fn encode_value(value: &Value, limits: Limits) -> Result<Vec<u8>, DbError> {
    let size = value.validate(limits.value)?;
    let mut e = Encoder {
        bytes: Vec::with_capacity(size),
        cap: limits.value.max_bytes,
    };
    e.value(value, 0, limits)?;
    Ok(e.bytes)
}

pub fn encode_frame(frame: &Frame, limits: Limits) -> Result<Vec<u8>, DbError> {
    let (kind, batch) = match &frame.body {
        Body::Request(_) => (1, false),
        Body::BatchRequest(_) => (1, true),
        Body::Response(_) => (2, false),
        Body::BatchResponse(_) => (2, true),
        Body::Event(_) => (3, false),
        Body::Ack(_) => (4, false),
        Body::Heartbeat => (5, false),
        Body::Error(_) => (6, false),
    };
    let cap = if matches!(kind, 1 | 4) {
        limits.max_request_bytes
    } else {
        limits.max_response_bytes
    };
    let mut e = Encoder {
        bytes: Vec::new(),
        cap: limits.max_frame_bytes.min(cap.saturating_add(HEADER_LEN)),
    };
    e.push(&[0; HEADER_LEN])?;
    match &frame.body {
        Body::Request(v) => e.request(v, limits)?,
        Body::Response(v) => e.response(v, limits)?,
        Body::BatchRequest(items) => {
            if items.is_empty() || items.len() > limits.max_batch_size {
                return Err(DbError::new(ErrorCode::LimitExceeded, "Invalid Batch size"));
            }
            e.count(items.len())?;
            for item in items {
                e.request(item, limits)?;
            }
        }
        Body::BatchResponse(items) => {
            if items.is_empty() || items.len() > limits.max_batch_size {
                return Err(DbError::new(ErrorCode::LimitExceeded, "Invalid Batch size"));
            }
            e.count(items.len())?;
            for item in items {
                e.response(item, limits)?;
            }
        }
        Body::Event(v) | Body::Ack(v) => e.value(v, 0, limits)?,
        Body::Heartbeat => {}
        Body::Error(v) => e.error(v)?,
    }
    let len = u32::try_from(e.bytes.len() - HEADER_LEN)
        .map_err(|_| DbError::new(ErrorCode::LimitExceeded, "Payload exceeds u32"))?;
    e.bytes[..2].copy_from_slice(b"LG");
    e.bytes[2] = VERSION;
    e.bytes[3] = kind;
    e.bytes[4..6].copy_from_slice(&u16::from(batch).to_be_bytes());
    e.bytes[6..14].copy_from_slice(&frame.request_id.to_be_bytes());
    e.bytes[14..18].copy_from_slice(&len.to_be_bytes());
    parse_header(e.bytes[..HEADER_LEN].try_into().unwrap(), limits).map_err(|e| e.error)?;
    Ok(e.bytes)
}
