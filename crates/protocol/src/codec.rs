//! 大端版本 1 编解码；分配前校验长度、元素数和逻辑预算。
//! Big-endian version 1 codec; lengths, counts, and logical budgets are checked before allocation.

use crate::*;
use ling_types::{Float, Scalar};
use std::collections::{BTreeMap, HashSet};
use std::mem::size_of;

#[cfg(test)]
thread_local! {
    /// 操作计数只存在于测试，观测命令规范化所扫描的字节数。
    /// Test-only operation counter observes bytes scanned while normalizing command names.
    static NORMALIZED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 校验魔数、版本、保留位、请求 ID 和方向限额，不读取或分配负载。
/// Checks magic, version, reserved bits, IDs, and directional caps without reading or allocating payloads.
pub fn parse_header(bytes: &[u8; HEADER_LEN], limits: Limits) -> Result<Header, DecodeError> {
    // 输入固定为 18 字节，以下 8/2/4 字节切片宽度由协议布局保证。
    // The fixed 18-byte header guarantees the following 8/2/4-byte slice widths.
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

/// 借用输入的有界解码器；语义错误延迟到帧结构完整后返回。
/// Bounded decoder borrowing input; semantic errors wait until the full frame structure is checked.
struct Decoder<'a> {
    /// 只借用当前帧负载，不读取下一帧。
    /// Borrows only the current payload, never reading into the next frame.
    bytes: &'a [u8],
    /// 下一个未消费字节的位置。
    /// Position of the next unconsumed byte.
    at: usize,
    /// 分配前扣减的剩余额度。
    /// Remaining budget debited before allocation.
    budget: usize,
    /// 与当前帧关联的固定资源边界。
    /// Resource boundaries fixed for the current frame.
    limits: Limits,
    /// 首个完整业务值错误，结构检查仍继续。
    /// First complete business-value error while structural validation continues.
    semantic: Option<DbError>,
}

impl<'a> Decoder<'a> {
    /// 为单个负载建立独立解码预算。
    /// Starts a separate decoding budget for one payload.
    fn new(bytes: &'a [u8], limits: Limits) -> Self {
        Self {
            bytes,
            at: 0,
            budget: limits.max_decode_bytes,
            limits,
            semantic: None,
        }
    }

    /// 检查偏移加法和剩余长度后移动游标。
    /// Checks offset arithmetic and remaining bytes before advancing.
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

    /// 读取一个标签或标记字节。
    /// Reads one tag or flag byte.
    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    /// 读取大端 u32 长度，不提前分配。
    /// Reads a big-endian u32 length without allocating.
    fn count(&mut self) -> Result<usize, DecodeError> {
        // take(4) 成功恰好返回四字节；截断已作为协议错误返回。
        // A successful take(4) returns exactly four bytes; truncation already returns a protocol error.
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    /// 在分配发生前扣减逻辑内存预算。
    /// Debits the logical memory budget before allocation.
    fn charge(&mut self, bytes: usize) -> Result<(), DecodeError> {
        self.budget = self
            .budget
            .checked_sub(bytes)
            .ok_or_else(DecodeError::limit)?;
        Ok(())
    }
    /// 只记录首个业务错误，继续检查结构边界。
    /// Records only the first business error while checking the remaining structure.
    fn invalid(&mut self, code: ErrorCode, message: &'static str) {
        self.semantic
            .get_or_insert_with(|| DbError::new(code, message));
    }
    /// 以最小编码长度排除虚假数量，并预扣容器开销。
    /// Rejects impossible counts using minimum wire length and reserves container overhead.
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

    /// 按冻结标签递归解析，禁止未知标签和非法 UTF-8。
    /// Recursively parses frozen tags, rejecting unknown tags and invalid UTF-8.
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
            // take(8) 对各数值分支统一保证宽度，失败先返回错误而不转换。
            // take(8) guarantees the numeric branch widths and returns truncation before conversion.
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
                    // 单字段 BTreeMap 也可能分配完整节点。
                    // A small BTreeMap may allocate a complete node for one field.
                    12 * size_of::<(String, Value)>() + 16 * size_of::<usize>(),
                )?;
                let mut fields = BTreeMap::new();
                for _ in 0..count {
                    let Value::String(key) = self.value(depth)? else {
                        return Err(DecodeError::protocol("Map field name must be a String"));
                    };
                    if key.is_empty() {
                        self.invalid(ErrorCode::InvalidValue, "Empty Map field");
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
                // 同时计入控制字节、负载因子和最小哈希表。
                // Account for control bytes, load factor and the minimum table.
                self.elements(count, 1, 2 * size_of::<Scalar>() + 2)?;
                if count != 0 {
                    self.charge(4 * size_of::<Scalar>() + 32)?;
                }
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

    /// 读取必须为 String 的结构字段。
    /// Reads a structural field that must be a String.
    fn string(&mut self) -> Result<String, DecodeError> {
        match self.value(0)? {
            Value::String(v) => Ok(v),
            _ => Err(DecodeError::protocol("Expected String")),
        }
    }
    /// 解析命令封套，单独验证签名中的实际业务值。
    /// Parses the command envelope and separately validates business-value arguments.
    fn request(&mut self) -> Result<Request, DecodeError> {
        let mut command = self.string()?;
        if !command.is_ascii() {
            return Err(DecodeError::protocol("Command name must be ASCII"));
        }
        // 名称仅规范化一次，不让命令长度与参数数量的乘积放大工作量。
        // Normalize once so command length and argument count do not multiply the work.
        command.make_ascii_uppercase();
        #[cfg(test)]
        NORMALIZED_BYTES.set(NORMALIZED_BYTES.get() + command.len());
        let Value::List(arguments) = self.value(0)? else {
            return Err(DecodeError::protocol("Expected argument List"));
        };
        // 协议封套不占用业务 Value 深度。
        // Protocol envelopes do not count against business Value depth.
        for (index, argument) in arguments.iter().enumerate() {
            if ling_types::is_value_argument(&command, index)
                && let Err(e) = argument.validate(self.limits.value)
            {
                self.semantic.get_or_insert(e);
            }
        }
        Ok(Request {
            command,
            arguments,
            value_error: self.semantic.take(),
        })
    }
    /// 保留服务端原始错误码和诊断文本。
    /// Preserves the original server error code and diagnostic message.
    fn error(&mut self) -> Result<DbError, DecodeError> {
        Ok(DbError {
            code: self.string()?,
            message: self.string()?,
        })
    }
    /// 按 status 区分成功 Null 与失败。
    /// Uses status to distinguish successful Null from failure.
    fn response(&mut self) -> Result<Result<Value, DbError>, DecodeError> {
        match self.byte()? {
            0 => Ok(Ok(self.value(0)?)),
            1 => Ok(Err(self.error()?)),
            _ => Err(DecodeError::protocol("Invalid response status")),
        }
    }
    /// 确保没有被忽略的尾随字节。
    /// Ensures no trailing bytes were ignored.
    fn finish(&self) -> Result<(), DecodeError> {
        if self.at != self.bytes.len() {
            return Err(DecodeError::protocol("Trailing payload bytes"));
        }
        Ok(())
    }
}

/// 解码独立业务值，拒绝尾随字节和任何语义错误。
/// Decodes a standalone business value, rejecting trailing bytes and semantic errors.
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

/// 解码完整帧；请求业务错误留在 Request，结构错误使整帧失败。
/// Decodes a complete frame; business errors stay in Request while structural errors reject the frame.
pub fn decode_frame(bytes: &[u8], limits: Limits) -> Result<Frame, DecodeError> {
    let raw_header = bytes
        .get(..HEADER_LEN)
        .ok_or_else(|| DecodeError::protocol("Truncated frame header"))?;
    // get(..HEADER_LEN) 成功只可能得到固定宽度的头部。
    // A successful get(..HEADER_LEN) can only return the exact header width.
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

/// 始终受总字节上限约束的输出缓冲。
/// Output buffer always bounded by its total byte cap.
struct Encoder {
    /// 只在预算允许时扩展的输出缓冲。
    /// Output buffer extended only when the budget permits.
    bytes: Vec<u8>,
    /// 当前编码操作的总字节上限。
    /// Total byte cap for this encoding operation.
    cap: usize,
}

impl Encoder {
    /// 检查缓冲增长后追加字节，失败不暴露半帧。
    /// Checks buffer growth before appending; failures expose no partial frame.
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
    /// 禁止长度截断并写入大端 u32。
    /// Rejects length truncation and writes a big-endian u32.
    fn count(&mut self, count: usize) -> Result<(), DbError> {
        self.push(
            &u32::try_from(count)
                .map_err(|_| DbError::new(ErrorCode::LimitExceeded, "Length exceeds u32"))?
                .to_be_bytes(),
        )
    }
    /// 保留 UTF-8 原字节，写入标签及长度前缀。
    /// Preserves UTF-8 bytes with a tag and length prefix.
    fn string(&mut self, text: &str) -> Result<(), DbError> {
        self.push(&[5])?;
        self.count(text.len())?;
        self.push(text.as_bytes())
    }
    /// 编码 Set 成员，保持数值与文本类型标签。
    /// Encodes Set members while preserving numeric and text type tags.
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
    /// 递归编码并限制封套深度；业务深度由入口独立验证。
    /// Encodes recursively with an envelope-depth cap; entry points validate business depth separately.
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
    /// 规范化命令并在写入业务参数前校验限额。
    /// Normalizes the command and checks limits before writing business arguments.
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
        let command = request.command.to_ascii_uppercase();
        self.string(&command)?;
        self.push(&[7])?;
        self.count(request.arguments.len())?;
        for (index, argument) in request.arguments.iter().enumerate() {
            if ling_types::is_value_argument(&command, index) {
                argument.validate(limits.value)?;
            }
            self.value(argument, 1, limits)?;
        }
        Ok(())
    }
    /// 编码稳定错误码和说明，不通过 JSON。
    /// Encodes the stable error code and message without JSON.
    fn error(&mut self, error: &DbError) -> Result<(), DbError> {
        self.string(&error.code)?;
        self.string(&error.message)
    }
    /// 显式写出成功或失败 status。
    /// Writes an explicit success or failure status.
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

/// 先测量完整业务值再分配精确大小的编码缓冲。
/// Measures the complete business value before allocating its wire buffer.
pub fn encode_value(value: &Value, limits: Limits) -> Result<Vec<u8>, DbError> {
    let size = value.validate(limits.value)?;
    let mut e = Encoder {
        bytes: Vec::with_capacity(size),
        cap: limits.value.max_bytes,
    };
    e.value(value, 0, limits)?;
    Ok(e.bytes)
}

/// 编码负载后填入冻结帧头，再校验 ID 与方向约束。
/// Encodes payload then fills the frozen header and validates ID and directional constraints.
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
    // 编码器首先写入完整固定头，之后只追加，头部宽度不会缩短。
    // The encoder begins with a complete fixed header and only appends, so its width cannot shrink.
    parse_header(e.bytes[..HEADER_LEN].try_into().unwrap(), limits).map_err(|e| e.error)?;
    Ok(e.bytes)
}

#[cfg(test)]
mod normalization_tests {
    use super::*;

    /// 命令名扫描次数与参数数量无关；长命令和大量参数不能产生相乘的规范化分配。
    /// Command scans are independent of argument count; long names and many arguments cannot multiply normalization allocations.
    #[test]
    fn command_normalization_is_once_per_request() {
        for count in [0, 1, 1000] {
            let request = Frame {
                request_id: 1,
                body: Body::Request(Request::new("x".repeat(8192), vec![Value::Null; count])),
            };
            let bytes = encode_frame(&request, Limits::default()).unwrap();
            NORMALIZED_BYTES.set(0);
            assert_eq!(decode_frame(&bytes, Limits::default()).unwrap(), request);
            assert_eq!(NORMALIZED_BYTES.get(), 8192);
        }
    }
}
