//! Shared data contracts; independent of transport and storage.

mod error;
mod value;

pub use error::{DbError, ErrorCode};
pub use value::{Float, Scalar, Value, ValueLimits};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation {
    pub epoch: u128,
    pub counter: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    pub stream_id: u128,
    pub db: u16,
    pub shard: u32,
    pub sequence: u64,
}
