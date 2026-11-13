//! 强类型值、规范化浮点和完整编码大小校验。
//! Strongly typed values, canonical floats, and complete wire-size validation.

use crate::{DbError, ErrorCode};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

/// 拒绝 NaN；正负零统一为正零位模式，使相等和哈希一致。
/// Rejects NaN; signed zeros use positive-zero bits so equality and hashing agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Float(u64);

impl Float {
    /// 构造允许无穷大的规范浮点；NaN 返回 ERR_INVALID_VALUE。
    /// Constructs a canonical float allowing infinity; NaN returns ERR_INVALID_VALUE.
    pub fn new(value: f64) -> Result<Self, DbError> {
        if value.is_nan() {
            return Err(DbError::new(ErrorCode::InvalidValue, "NaN is not a Value"));
        }
        Ok(Self(if value == 0.0 { 0 } else { value.to_bits() }))
    }

    /// 取回规范化后的 IEEE 754 数值。
    /// Returns the canonical IEEE 754 number.
    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl Hash for Float {
    /// 哈希规范位模式，保持正负零和相等关系一致。
    /// Hashes canonical bits so signed-zero equality and hashing agree.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// 集合成员允许的七种标量；数值类型不隐式提升。
/// Seven scalar types allowed in sets; numeric types are never promoted.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
    /// 显式空值。
    /// Explicit null value.
    Null,
    /// 布尔值。
    /// Boolean value.
    Bool(bool),
    /// 有符号 64 位整数。
    /// Signed 64-bit integer.
    Int(i64),
    /// 无符号 64 位整数。
    /// Unsigned 64-bit integer.
    UInt(u64),
    /// 拒绝 NaN 的双精度浮点。
    /// Double-precision float excluding NaN.
    Float(Float),
    /// UTF-8 文本，与 Bytes 不互转。
    /// UTF-8 text, never coerced to Bytes.
    String(String),
    /// 任意字节，与 String 不互转。
    /// Arbitrary bytes, never coerced to String.
    Bytes(Vec<u8>),
}

/// 十种独立 Value；Null 是成功值，区别于 Key 缺失。
/// Ten distinct Values; Null is a successful value, separate from a missing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// 显式空值。
    /// Explicit null value.
    Null,
    /// 布尔值。
    /// Boolean value.
    Bool(bool),
    /// 有符号 64 位整数。
    /// Signed 64-bit integer.
    Int(i64),
    /// 无符号 64 位整数。
    /// Unsigned 64-bit integer.
    UInt(u64),
    /// 拒绝 NaN 的双精度浮点。
    /// Double-precision float excluding NaN.
    Float(Float),
    /// UTF-8 文本，与 Bytes 不互转。
    /// UTF-8 text, never coerced to Bytes.
    String(String),
    /// 任意字节，与 String 不互转。
    /// Arbitrary bytes, never coerced to String.
    Bytes(Vec<u8>),
    /// 有序、可递归的值序列。
    /// Ordered, recursively nestable values.
    List(Vec<Value>),
    /// 非空 UTF-8 字段名映射到任意值。
    /// Maps nonempty UTF-8 field names to arbitrary values.
    Map(BTreeMap<String, Value>),
    /// 按类型敏感相等去重的标量集合。
    /// Scalar set deduplicated by type-sensitive equality.
    Set(HashSet<Scalar>),
}

impl From<Scalar> for Value {
    /// 无损提升标量，不转换原始类型。
    /// Promotes a scalar losslessly without converting its type.
    fn from(scalar: Scalar) -> Self {
        match scalar {
            Scalar::Null => Self::Null,
            Scalar::Bool(v) => Self::Bool(v),
            Scalar::Int(v) => Self::Int(v),
            Scalar::UInt(v) => Self::UInt(v),
            Scalar::Float(v) => Self::Float(v),
            Scalar::String(v) => Self::String(v),
            Scalar::Bytes(v) => Self::Bytes(v),
        }
    }
}

impl TryFrom<Value> for Scalar {
    type Error = DbError;

    /// 仅标量可成为 Set 成员，容器返回 ERR_INVALID_VALUE。
    /// Only scalars become Set members; containers return ERR_INVALID_VALUE.
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(v),
            Value::Int(v) => Self::Int(v),
            Value::UInt(v) => Self::UInt(v),
            Value::Float(v) => Self::Float(v),
            Value::String(v) => Self::String(v),
            Value::Bytes(v) => Self::Bytes(v),
            _ => {
                return Err(DbError::new(
                    ErrorCode::InvalidValue,
                    "Set members must be scalar",
                ));
            }
        })
    }
}

/// 单个业务值的编码、递归和名称边界。
/// Wire-size, recursion, and name boundaries for one business value.
#[derive(Clone, Copy, Debug)]
pub struct ValueLimits {
    /// 完整编码字节数上限，包含子值。
    /// Maximum complete wire bytes including children.
    pub max_bytes: usize,
    /// 容器最大层数；标量不增加层数。
    /// Maximum container levels; scalars add no level.
    pub max_depth: usize,
    /// Key 与 Map 字段名的 UTF-8 字节上限。
    /// UTF-8 byte cap for keys and Map field names.
    pub max_key_bytes: usize,
}

impl Default for ValueLimits {
    /// 默认单值 64 MiB、容器 8 层、名称 1024 字节。
    /// Defaults to 64 MiB per value, eight container levels, and 1024-byte names.
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_depth: 8,
            max_key_bytes: 1024,
        }
    }
}

impl ValueLimits {
    /// 拒绝空名称和超过 UTF-8 字节限额的 Key / 字段。
    /// Rejects empty names and keys or fields exceeding the UTF-8 byte limit.
    pub fn validate_key(&self, key: &str) -> Result<(), DbError> {
        if key.is_empty() {
            return Err(DbError::new(
                ErrorCode::InvalidArgument,
                "Key must not be empty",
            ));
        }
        if key.len() > self.max_key_bytes {
            return Err(limit_error());
        }
        Ok(())
    }
}

/// 构造稳定限额错误，不携带用户内容。
/// Builds a stable limit error without user contents.
fn limit_error() -> DbError {
    DbError::new(
        ErrorCode::LimitExceeded,
        "Value exceeds size or depth limit",
    )
}

/// 先检查加法和总上限，再累计编码长度。
/// Checks addition and the total cap before charging wire bytes.
fn add_bytes(bytes: &mut usize, count: usize, limits: ValueLimits) -> Result<(), DbError> {
    *bytes = bytes
        .checked_add(count)
        .filter(|v| *v <= limits.max_bytes)
        .ok_or_else(limit_error)?;
    Ok(())
}

/// 检查 u32 长度边界，并计入标签和长度前缀。
/// Checks the u32 length boundary and includes tag and length-prefix bytes.
fn blob_size(len: usize, bytes: &mut usize, limits: ValueLimits) -> Result<(), DbError> {
    u32::try_from(len).map_err(|_| limit_error())?;
    add_bytes(bytes, 5, limits)?;
    add_bytes(bytes, len, limits)
}

impl Scalar {
    /// 累计标量的规范编码长度。
    /// Charges the canonical wire length of a scalar.
    fn measure(&self, limits: ValueLimits, bytes: &mut usize) -> Result<(), DbError> {
        match self {
            Self::Null => add_bytes(bytes, 1, limits),
            Self::Bool(_) => add_bytes(bytes, 2, limits),
            Self::Int(_) | Self::UInt(_) | Self::Float(_) => add_bytes(bytes, 9, limits),
            Self::String(v) => blob_size(v.len(), bytes, limits),
            Self::Bytes(v) => blob_size(v.len(), bytes, limits),
        }
    }
}

impl Value {
    /// 返回命令 TYPE 使用的稳定小写类型名。
    /// Returns the stable lowercase type name used by TYPE.
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::UInt(_) => "uint",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::List(_) => "list",
            Self::Map(_) => "map",
            Self::Set(_) => "set",
        }
    }

    /// 校验完整业务值，返回含标签与长度字段的编码字节数。
    /// Validates the entire business value and returns its wire length including tags and lengths.
    pub fn validate(&self, limits: ValueLimits) -> Result<usize, DbError> {
        let mut bytes = 0;
        self.measure(0, limits, &mut bytes)?;
        Ok(bytes)
    }

    /// 递归遍历容器，所有长度使用检查算术。
    /// Walks containers recursively with checked length arithmetic.
    fn measure(
        &self,
        parents: usize,
        limits: ValueLimits,
        bytes: &mut usize,
    ) -> Result<(), DbError> {
        let depth =
            parents + usize::from(matches!(self, Self::List(_) | Self::Map(_) | Self::Set(_)));
        if depth > limits.max_depth {
            return Err(limit_error());
        }
        match self {
            Self::Null => add_bytes(bytes, 1, limits)?,
            Self::Bool(_) => add_bytes(bytes, 2, limits)?,
            Self::Int(_) | Self::UInt(_) | Self::Float(_) => add_bytes(bytes, 9, limits)?,
            Self::String(v) => blob_size(v.len(), bytes, limits)?,
            Self::Bytes(v) => blob_size(v.len(), bytes, limits)?,
            Self::List(v) => {
                u32::try_from(v.len()).map_err(|_| limit_error())?;
                add_bytes(bytes, 5, limits)?;
                for item in v {
                    item.measure(depth, limits, bytes)?;
                }
            }
            Self::Map(v) => {
                u32::try_from(v.len()).map_err(|_| limit_error())?;
                add_bytes(bytes, 5, limits)?;
                for (key, item) in v {
                    if key.is_empty() {
                        return Err(DbError::new(
                            ErrorCode::InvalidValue,
                            "Map field must not be empty",
                        ));
                    }
                    if key.len() > limits.max_key_bytes {
                        return Err(limit_error());
                    }
                    blob_size(key.len(), bytes, limits)?;
                    item.measure(depth, limits, bytes)?;
                }
            }
            Self::Set(v) => {
                u32::try_from(v.len()).map_err(|_| limit_error())?;
                add_bytes(bytes, 5, limits)?;
                for item in v {
                    item.measure(limits, bytes)?;
                }
            }
        }
        Ok(())
    }
}
