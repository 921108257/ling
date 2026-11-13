//! CLI text grammar and rendering. JSON is used only for String escaping.

use base64::{Engine, engine::general_purpose::STANDARD};
use linger_protocol::Request;
use linger_types::{Float, Scalar, Value, ValueLimits};
use std::collections::{BTreeMap, HashSet};

const MAX_LINE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError;
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("命令或值语法错误")
    }
}
impl std::error::Error for ParseError {}

struct Parser<'a> {
    source: &'a str,
    at: usize,
    budget: usize,
}
impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            at: 0,
            budget: 128 * 1024 * 1024,
        }
    }
    fn charge(&mut self, bytes: usize) -> Result<(), ParseError> {
        self.budget = self.budget.checked_sub(bytes).ok_or(ParseError)?;
        Ok(())
    }
    fn rest(&self) -> &'a str {
        &self.source[self.at..]
    }
    fn space(&mut self) {
        self.at += self.rest().len() - self.rest().trim_start().len();
    }
    fn eat(&mut self, token: &str) -> bool {
        self.space();
        if self.rest().starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, token: &str) -> Result<(), ParseError> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(ParseError)
        }
    }
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
    fn inside(&mut self) -> Result<&'a str, ParseError> {
        let end = self.rest().find(')').ok_or(ParseError)?;
        let inner = self.rest()[..end].trim();
        self.at += end + 1;
        Ok(inner)
    }
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
    fn end(&mut self) -> Result<(), ParseError> {
        self.space();
        if self.rest().is_empty() {
            Ok(())
        } else {
            Err(ParseError)
        }
    }
}

fn unsigned_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|c| c.is_ascii_digit())
}
fn signed_decimal(text: &str) -> bool {
    unsigned_decimal(
        text.strip_prefix('-')
            .or_else(|| text.strip_prefix('+'))
            .unwrap_or(text),
    )
}

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

/// Only stage-1 signatures are interpreted here. Other names reach the server
/// as text arguments so its stable unknown-command error remains authoritative.
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
    p.space();
    while !p.rest().is_empty() {
        p.charge(2 * std::mem::size_of::<Value>())?;
        args.push(if matches!(command.as_str(), "HELLO" | "SELECT") {
            let n = p.word()?;
            if !unsigned_decimal(n) {
                return Err(ParseError);
            }
            Value::UInt(n.parse().map_err(|_| ParseError)?)
        } else {
            Value::String(p.text()?)
        });
        p.space();
    }
    Ok(Request::new(command, args))
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("String serialization")
}

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
