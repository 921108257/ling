use crate::{DbError, ErrorCode};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

/// NaN cannot be constructed; both signed zeros use the positive-zero bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Float(u64);

impl Float {
    pub fn new(value: f64) -> Result<Self, DbError> {
        if value.is_nan() {
            return Err(DbError::new(ErrorCode::InvalidValue, "NaN is not a Value"));
        }
        Ok(Self(if value == 0.0 { 0 } else { value.to_bits() }))
    }

    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl Hash for Float {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(Float),
    String(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(Float),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Set(HashSet<Scalar>),
}

impl From<Scalar> for Value {
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

#[derive(Clone, Copy, Debug)]
pub struct ValueLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_key_bytes: usize,
}

impl Default for ValueLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_depth: 8,
            max_key_bytes: 1024,
        }
    }
}

impl ValueLimits {
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

fn limit_error() -> DbError {
    DbError::new(
        ErrorCode::LimitExceeded,
        "Value exceeds size or depth limit",
    )
}

fn add_bytes(bytes: &mut usize, count: usize, limits: ValueLimits) -> Result<(), DbError> {
    *bytes = bytes
        .checked_add(count)
        .filter(|v| *v <= limits.max_bytes)
        .ok_or_else(limit_error)?;
    Ok(())
}

fn blob_size(len: usize, bytes: &mut usize, limits: ValueLimits) -> Result<(), DbError> {
    u32::try_from(len).map_err(|_| limit_error())?;
    add_bytes(bytes, 5, limits)?;
    add_bytes(bytes, len, limits)
}

impl Scalar {
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

    /// Validate the entire business Value, returning its complete wire length.
    pub fn validate(&self, limits: ValueLimits) -> Result<usize, DbError> {
        let mut bytes = 0;
        self.measure(0, limits, &mut bytes)?;
        Ok(bytes)
    }

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
