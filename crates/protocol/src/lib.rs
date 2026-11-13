//! Version 1 wire format. Socket reads are never assumed to align with frames.

mod codec;
mod transport;

pub use codec::{decode_frame, decode_value, encode_frame, encode_value, parse_header};
use linger_types::{DbError, ErrorCode, Value, ValueLimits};
pub use transport::{ReadError, read_frame, write_frame};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 18;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_batch_size: usize,
    pub max_decode_bytes: usize,
    pub value: ValueLimits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 128 * 1024 * 1024,
            max_request_bytes: 64 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
            max_batch_size: 1000,
            max_decode_bytes: 128 * 1024 * 1024,
            value: ValueLimits::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub request_id: u64,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Request(Request),
    BatchRequest(Vec<Request>),
    Response(Result<Value, DbError>),
    BatchResponse(Vec<Result<Value, DbError>>),
    Event(Value),
    Ack(Value),
    Heartbeat,
    Error(DbError),
}

#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub command: String,
    pub arguments: Vec<Value>,
    /// A structurally delimited but invalid business Value must not close TCP.
    pub value_error: Option<DbError>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("command", &self.command)
            .field("argument_count", &self.arguments.len())
            .finish_non_exhaustive()
    }
}

impl Request {
    pub fn new(command: impl AsRef<str>, arguments: Vec<Value>) -> Self {
        Self {
            command: command.as_ref().to_ascii_uppercase(),
            arguments,
            value_error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Header {
    pub kind: u8,
    pub batch: bool,
    pub request_id: u64,
    pub payload_len: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub error: DbError,
    pub fatal: bool,
    pub request_id: u64,
}

impl DecodeError {
    pub(crate) fn protocol(message: &'static str) -> Self {
        Self {
            error: DbError::new(ErrorCode::Protocol, message),
            fatal: true,
            request_id: 0,
        }
    }

    pub(crate) fn limit() -> Self {
        Self::protocol("Frame exceeds decoding budget")
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for DecodeError {}
