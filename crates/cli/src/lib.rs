//! CLI 类型字面量解析与展示；JSON 只用于 String 转义，不承载 Value。
//! CLI typed-literal parsing and rendering; JSON is used only for String escaping, never to carry Values.

use base64::{Engine, engine::general_purpose::STANDARD};
use ling_protocol::Request;
use ling_types::{Float, Scalar, Value, ValueLimits};
use std::collections::{BTreeMap, HashSet};

/// 单条输入的字节上限，解析前先检查。
/// Byte cap for one input, checked before parsing.
const MAX_LINE: usize = 64 * 1024 * 1024;

/// 不含原始输入的语法错误，避免 AUTH 密码被回显。
/// Syntax error without raw input, preventing AUTH password echoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError;
impl std::fmt::Display for ParseError {
    /// 固定错误说明，不包含用户输入。
    /// Uses a fixed diagnostic without user input.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("命令或值语法错误")
    }
}
impl std::error::Error for ParseError {}

/// 借用 UTF-8 输入并跟踪游标、递归深度和分配预算。
/// Borrows UTF-8 input and tracks cursor, nesting, and allocation budget.
struct Parser<'a> {
    /// 完整原始行，只借用不复制。
    /// Original complete line, borrowed without copying.
    source: &'a str,
    /// 始终位于 UTF-8 字符边界的字节游标。
    /// Byte cursor always on a UTF-8 character boundary.
    at: usize,
    /// 剩余的保守解析分配额度。
    /// Remaining conservative parsing-allocation budget.
    budget: usize,
}
impl<'a> Parser<'a> {
    /// 创建独立的单行解析预算。
    /// Creates an independent per-line parsing budget.
    fn new(source: &'a str) -> Self {
        Self {
            source,
            at: 0,
            budget: 128 * 1024 * 1024,
        }
    }
    /// 分配前扣除保守内存开销。
    /// Charges conservative memory costs before allocation.
    fn charge(&mut self, bytes: usize) -> Result<(), ParseError> {
        self.budget = self.budget.checked_sub(bytes).ok_or(ParseError)?;
        Ok(())
    }
    /// 只借用尚未消费的 UTF-8 尾部。
    /// Borrows the remaining unconsumed UTF-8 suffix.
    fn rest(&self) -> &'a str {
        &self.source[self.at..]
    }
    /// 跳过 Unicode 空白且保持字符边界。
    /// Skips Unicode whitespace while preserving character boundaries.
    fn space(&mut self) {
        self.at += self.rest().len() - self.rest().trim_start().len();
    }
    /// 跳过空白并尝试消费精确语法标记。
    /// Skips whitespace and attempts to consume an exact syntax token.
    fn eat(&mut self, token: &str) -> bool {
        self.space();
        if self.rest().starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }
    /// 要求下一个语法标记存在。
    /// Requires the next syntax token.
    fn expect(&mut self, token: &str) -> Result<(), ParseError> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(ParseError)
        }
    }
    /// 读取空白分隔裸词，不分配字符串。
    /// Reads a whitespace-delimited bare word without allocating.
    fn word(&mut self) -> Result<&'a str, ParseError> {
        self.space();
        let end = self
            .rest()
            .find(char::is_whitespace)
            .unwrap_or(self.rest().len());
        if end == 0 {
            return Err(ParseError);
        }
        let result = &self.rest()[..end];
        self.at += end;
        Ok(result)
    }
    /// 识别双引号边界，再由成熟 JSON 实现验证转义。
    /// Finds double-quote boundaries and delegates escape validation to JSON.
    fn string(&mut self) -> Result<String, ParseError> {
        self.space();
        if !self.rest().starts_with('"') {
            return Err(ParseError);
        }
        let start = self.at;
        self.at += 1;
        let mut escaped = false;
        for (index, ch) in self.source[self.at..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                self.at += index + 1;
                self.charge(self.at - start)?;
                return serde_json::from_str(&self.source[start..self.at]).map_err(|_| ParseError);
            }
        }
        Err(ParseError)
    }
    /// Key、选项等文本位置接受引号或裸词。
    /// Text positions such as keys and options accept quoted strings or bare words.
    fn text(&mut self) -> Result<String, ParseError> {
        self.space();
        if self.rest().starts_with('"') {
            self.string()
        } else {
            let word = self.word()?;
            self.charge(word.len())?;
            Ok(word.into())
        }
    }
    /// 读取数字或十六进制字面量的右括号前内容。
    /// Reads contents before the closing parenthesis of a numeric or hex literal.
    fn inside(&mut self) -> Result<&'a str, ParseError> {
        let end = self.rest().find(')').ok_or(ParseError)?;
        let inner = self.rest()[..end].trim();
        self.at += end + 1;
        Ok(inner)
    }
    /// 按显式标签解析，拒绝隐式类型推测和超深容器。
    /// Parses explicit tags, rejecting inferred types and excessive nesting.
    fn value(&mut self, parents: usize) -> Result<Value, ParseError> {
        self.space();
        if self.rest().starts_with('"') {
            return Ok(Value::String(self.string()?));
        }
        for (text, value) in [
            ("null", Value::Null),
            ("true", Value::Bool(true)),
            ("false", Value::Bool(false)),
        ] {
            if self.rest().starts_with(text) {
                self.at += text.len();
                return Ok(value);
            }
        }
        if self.eat("int(") {
            let n = self.inside()?;
            if !signed_decimal(n) {
                return Err(ParseError);
            }
            return Ok(Value::Int(n.parse().map_err(|_| ParseError)?));
        }
        if self.eat("uint(") {
            let n = self.inside()?;
            if !unsigned_decimal(n) {
                return Err(ParseError);
            }
            return Ok(Value::UInt(n.parse().map_err(|_| ParseError)?));
        }
        if self.eat("float(") {
            let n = self.inside()?;
            if !matches!(n, "inf" | "-inf")
                && (n.is_empty()
                    || !n
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)))
            {
                return Err(ParseError);
            }
            let number: f64 = n.parse().map_err(|_| ParseError)?;
            if number.is_infinite() && !matches!(n, "inf" | "-inf") {
                return Err(ParseError);
            }
            return Ok(Value::Float(Float::new(number).map_err(|_| ParseError)?));
        }
        if self.eat("bytes(") {
            self.space();
            let bytes = if self.eat("0x") {
                let raw = self.inside()?;
                if raw.len() % 2 != 0 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(ParseError);
                }
                self.charge(raw.len() / 2)?;
                (0..raw.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).map_err(|_| ParseError))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                self.expect("b64")?;
                let text = self.string()?;
                self.expect(")")?;
                self.charge(text.len())?;
                STANDARD.decode(text).map_err(|_| ParseError)?
            };
            return Ok(Value::Bytes(bytes));
        }
        if parents >= ValueLimits::default().max_depth {
            return Err(ParseError);
        }
        if self.eat("list(") {
            let values = self.sequence(parents + 1)?;
            self.expect(")")?;
            return Ok(Value::List(values));
        }
        if self.eat("set(") {
            let values = self.sequence(parents + 1)?;
            self.expect(")")?;
            self.charge(
                values
                    .len()
                    .checked_mul(2 * std::mem::size_of::<Scalar>() + 2)
                    .and_then(|n| n.checked_add(256))
                    .ok_or(ParseError)?,
            )?;
            let mut members = HashSet::new();
            for value in values {
                members.insert(Scalar::try_from(value).map_err(|_| ParseError)?);
            }
            return Ok(Value::Set(members));
        }
        if self.eat("map(") {
            self.expect("{")?;
            let mut fields = BTreeMap::new();
            if !self.eat("}") {
                loop {
                    self.charge(1024)?;
                    let key = self.string()?;
                    if key.is_empty() {
                        return Err(ParseError);
                    }
                    self.expect(":")?;
                    let value = self.value(parents + 1)?;
                    if fields.insert(key, value).is_some() {
                        return Err(ParseError);
                    }
                    if self.eat("}") {
                        break;
                    }
                    self.expect(",")?;
                }
            }
            self.expect(")")?;
            return Ok(Value::Map(fields));
        }
        Err(ParseError)
    }
    /// 解析列表式元素序列并逐项预扣分配预算。
    /// Parses list-like sequences and reserves allocation budget per item.
    fn sequence(&mut self, parents: usize) -> Result<Vec<Value>, ParseError> {
        self.expect("[")?;
        let mut items = Vec::new();
        if self.eat("]") {
            return Ok(items);
        }
        loop {
            self.charge(2 * std::mem::size_of::<Value>())?;
            items.push(self.value(parents)?);
            if self.eat("]") {
                break;
            }
            self.expect(",")?;
        }
        Ok(items)
    }
    /// 允许尾随空白，拒绝未消费内容。
    /// Allows trailing whitespace but rejects unconsumed contents.
    fn end(&mut self) -> Result<(), ParseError> {
        self.space();
        if self.rest().is_empty() {
            Ok(())
        } else {
            Err(ParseError)
        }
    }
}

/// 只接受非空十进制数字，不猜测进制。
/// Accepts only nonempty decimal digits without guessing a radix.
fn unsigned_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|c| c.is_ascii_digit())
}
/// 有符号十进制允许一个前导正负号。
/// Signed decimal permits a single leading plus or minus.
fn signed_decimal(text: &str) -> bool {
    unsigned_decimal(
        text.strip_prefix('-')
            .or_else(|| text.strip_prefix('+'))
            .unwrap_or(text),
    )
}

/// 解析单个完整类型字面量并验证业务深度和大小。
/// Parses one complete typed literal and validates business depth and size.
pub fn parse_value(source: &str) -> Result<Value, ParseError> {
    if source.len() > MAX_LINE {
        return Err(ParseError);
    }
    let mut parser = Parser::new(source);
    let value = parser.value(0)?;
    parser.end()?;
    value
        .validate(ValueLimits::default())
        .map_err(|_| ParseError)?;
    Ok(value)
}

/// 按命令签名区分业务 Value、固定整数和文本；未知名称仍交由服务端裁决。
/// Distinguishes Values, fixed integers, and text by signature; the server resolves unknown names.
pub fn parse_command(source: &str) -> Result<Request, ParseError> {
    if source.len() > MAX_LINE {
        return Err(ParseError);
    }
    let mut p = Parser::new(source);
    let command = p.word()?.to_ascii_uppercase();
    if !command.is_ascii() {
        return Err(ParseError);
    }
    let mut args = Vec::new();
    if command == "ACK" {
        let id = parse_event_id(&p.text()?)?;
        p.end()?;
        return Ok(Request::new(command, vec![id.to_value()]));
    }
    if command == "SUBSCRIBE" {
        let mut patterns = Vec::new();
        p.space();
        while !p.rest().is_empty() {
            p.charge(2 * std::mem::size_of::<Value>())?;
            let quoted = p.rest().starts_with('"');
            let token = p.text()?;
            if !quoted && token.eq_ignore_ascii_case("GROUP") {
                if patterns.is_empty() {
                    return Err(ParseError);
                }
                let group = p.text()?;
                p.end()?;
                return Ok(Request::new(
                    command,
                    vec![
                        Value::List(patterns),
                        Value::String("GROUP".into()),
                        Value::String(group),
                    ],
                ));
            }
            patterns.push(Value::String(token));
            p.space();
        }
        if patterns.is_empty() {
            return Err(ParseError);
        }
        return Ok(Request::new(command, vec![Value::List(patterns)]));
    }
    p.space();
    while !p.rest().is_empty() {
        p.charge(2 * std::mem::size_of::<Value>())?;
        let index = args.len();
        let unsigned = matches!(
            (command.as_str(), index),
            ("HELLO" | "SELECT" | "SCAN", 0)
                | ("LPOP" | "RPOP" | "SPOP" | "MOVE", 1)
                | ("SWAPDB", 0 | 1)
        );
        let unsigned = unsigned || (command == "COPY" && index >= 3
            && args.get(index - 1).is_some_and(|value| matches!(value, Value::String(option) if option.eq_ignore_ascii_case("DB"))))
            || (command == "SCAN" && index >= 2 && index % 2 == 0 && args.get(index - 1).is_some_and(|value| matches!(value, Value::String(option) if option.eq_ignore_ascii_case("COUNT"))));
        let signed = matches!(
            (command.as_str(), index),
            (
                "INCRBY"
                    | "DECRBY"
                    | "LINDEX"
                    | "LSET"
                    | "LREM"
                    | "SRANDMEMBER"
                    | "EXPIRE"
                    | "PEXPIRE"
                    | "EXPIREAT"
                    | "PEXPIREAT",
                1
            ) | ("LRANGE" | "LTRIM", 1 | 2)
        );
        let signed = signed || (command == "SET" && index >= 3 && args.get(index - 1).is_some_and(|value|
            matches!(value, Value::String(option) if matches!(option.to_ascii_uppercase().as_str(), "EX" | "PX" | "EXAT" | "PXAT"))));
        args.push(if ling_types::is_value_argument(&command, index) {
            p.value(0)?
        } else if unsigned {
            let n = p.word()?;
            if !unsigned_decimal(n) {
                return Err(ParseError);
            }
            Value::UInt(n.parse().map_err(|_| ParseError)?)
        } else if signed {
            let n = p.word()?;
            if !signed_decimal(n) {
                return Err(ParseError);
            }
            Value::Int(n.parse().map_err(|_| ParseError)?)
        } else {
            Value::String(p.text()?)
        });
        if !p.rest().is_empty() && !p.rest().starts_with(char::is_whitespace) {
            return Err(ParseError);
        }
        p.space();
    }
    Ok(Request::new(command, args))
}

/// 仅使用 JSON 字符串转义，保持 Unicode 与控制字符可逆。
/// Uses JSON string escaping only, preserving Unicode and control-character round trips.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("String serialization")
}

/// 输出可再次解析的显式类型文本，64 位整数不转浮点。
/// Renders parseable explicitly typed text without converting 64-bit integers to floats.
pub fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(v) => v.to_string(),
        Value::Int(v) => format!("int({v})"),
        Value::UInt(v) => format!("uint({v})"),
        Value::Float(v) => format!("float({})", v.get()),
        Value::String(v) => quoted(v),
        Value::Bytes(v) => format!(
            "bytes(0x{})",
            v.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        Value::List(v) => format!(
            "list([{}])",
            v.iter().map(display_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Map(v) => format!(
            "map({{{}}})",
            v.iter()
                .map(|(k, v)| format!("{}: {}", quoted(k), display_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Set(v) => {
            let mut items: Vec<_> = v
                .iter()
                .map(|v| display_value(&Value::from(v.clone())))
                .collect();
            items.sort();
            format!("set([{}])", items.join(", "))
        }
    }
}

/// 无损显示复合事件身份；Losslessly displays a compound event identity.
pub fn display_event_id(id: ling_types::EventId) -> String {
    format!(
        "{:032x}/{}/{}/{}",
        id.stream_id, id.db, id.shard, id.sequence
    )
}
/// 严格读取固定宽度 stream 与十进制坐标；Strictly parses a fixed-width stream and decimal coordinates.
pub fn parse_event_id(text: &str) -> Result<ling_types::EventId, ParseError> {
    let parts: Vec<_> = text.split('/').collect();
    if parts.len() != 4
        || parts[0].len() != 32
        || !parts[0]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || parts[1..].iter().any(|s| !unsigned_decimal(s))
    {
        return Err(ParseError);
    }
    let id = ling_types::EventId {
        stream_id: u128::from_str_radix(parts[0], 16).map_err(|_| ParseError)?,
        db: parts[1].parse().map_err(|_| ParseError)?,
        shard: parts[2].parse().map_err(|_| ParseError)?,
        sequence: parts[3].parse().map_err(|_| ParseError)?,
    };
    if id.sequence == 0 {
        return Err(ParseError);
    }
    Ok(id)
}
