//! 版本 1 二进制协议；不假设套接字读取与帧边界对齐。
//! Version 1 binary protocol; socket reads need not align with frame boundaries.

mod codec;
mod transport;

pub use codec::{decode_frame, decode_value, encode_frame, encode_value, parse_header};
use linger_types::{DbError, ErrorCode, Value, ValueLimits};
pub use transport::{ReadError, read_frame, write_frame};

/// 已冻结的线上协议版本。
/// Frozen wire-protocol version.
pub const VERSION: u8 = 1;
/// 固定帧头长度，包含魔数、版本、类型、标记、ID 和负载长度。
/// Fixed header length covering magic, version, kind, flags, ID, and payload length.
pub const HEADER_LEN: usize = 18;

/// 独立限制帧、业务值与解码分配，防止小输入放大内存。
/// Separate caps for frames, business values, and decoded allocations prevent memory amplification.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 包含帧头的总字节上限。
    /// Total byte cap including the header.
    pub max_frame_bytes: usize,
    /// 请求负载上限，不含帧头。
    /// Request payload cap excluding the header.
    pub max_request_bytes: usize,
    /// 响应负载上限，不含帧头。
    /// Response payload cap excluding the header.
    pub max_response_bytes: usize,
    /// 单个 Batch 的子项数量上限。
    /// Maximum items in one Batch.
    pub max_batch_size: usize,
    /// 单帧解码分配的保守逻辑预算。
    /// Conservative allocation budget for decoding one frame.
    pub max_decode_bytes: usize,
    /// 业务值限额，封套不消耗业务容器深度。
    /// Business-value limits; envelopes do not consume business depth.
    pub value: ValueLimits,
}

impl Default for Limits {
    /// 默认 128 MiB 帧与解码预算、64 MiB 方向负载。
    /// Defaults to 128 MiB frame and decode budgets and 64 MiB directional payloads.
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

/// 携带请求匹配 ID 的完整协议帧。
/// Complete protocol frame carrying its request-matching ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// 请求匹配编号；推送和心跳固定为零。
    /// Matching request number; zero for pushes and heartbeats.
    pub request_id: u64,
    /// 与帧头种类一致的类型化负载。
    /// Typed payload matching the header kind.
    pub body: Body,
}

/// 帧负载种类；存在编解码不代表服务端开放对应能力。
/// Frame payload variants; codec support does not imply server support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// 普通单命令请求。
    /// Ordinary single-command request.
    Request(Request),
    /// 普通批量请求，不具有事务语义。
    /// Ordinary batch request without transaction semantics.
    BatchRequest(Vec<Request>),
    /// 单命令成功值或稳定错误。
    /// Single-command value or stable error.
    Response(Result<Value, DbError>),
    /// 与批量输入顺序对应的独立结果。
    /// Independent results in batch-input order.
    BatchResponse(Vec<Result<Value, DbError>>),
    /// 事件推送负载。
    /// Event push payload.
    Event(Value),
    /// 事件确认负载。
    /// Event acknowledgement payload.
    Ack(Value),
    /// 无负载的连接心跳。
    /// Connection heartbeat with no payload.
    Heartbeat,
    /// 致命连接或协议错误。
    /// Fatal connection or protocol error.
    Error(DbError),
}

/// 规范命令与类型化参数；Debug 隐藏参数以保护认证和业务内容。
/// Canonical command with typed arguments; Debug hides arguments to protect credentials and contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// 大小写不敏感的 ASCII 命令名。
    /// Case-insensitive ASCII command name.
    pub command: String,
    /// 保留类型标签和整数精度的参数序列。
    /// Arguments preserving type tags and integer precision.
    pub arguments: Vec<Value>,
    /// 结构完整的非法业务值只使命令失败，不关闭 TCP。
    /// A structurally delimited but invalid business Value must not close TCP.
    pub value_error: Option<DbError>,
}

impl std::fmt::Debug for Request {
    /// 隐藏参数和值，只显示命令名与参数数量。
    /// Hides arguments and values, displaying only the command name and argument count.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("command", &self.command)
            .field("argument_count", &self.arguments.len())
            .finish_non_exhaustive()
    }
}

impl Request {
    /// 将 ASCII 命令规范为大写；实际编码时验证名称和参数。
    /// Normalizes ASCII commands to uppercase; encoding validates the name and arguments.
    pub fn new(command: impl AsRef<str>, arguments: Vec<Value>) -> Self {
        Self {
            command: command.as_ref().to_ascii_uppercase(),
            arguments,
            value_error: None,
        }
    }
}

/// 已经过版本、标记和长度校验的帧头。
/// Frame header validated for version, flags, and lengths.
#[derive(Clone, Debug)]
pub struct Header {
    /// 版本 1 注册的帧类型编号。
    /// Frame kind registered by version 1.
    pub kind: u8,
    /// 仅请求或响应可带 Batch 标记。
    /// Only requests and responses may carry the Batch flag.
    pub batch: bool,
    /// 帧头中的请求匹配编号。
    /// Request-matching number in the header.
    pub request_id: u64,
    /// 通过总帧和方向预算校验的负载字节数。
    /// Payload bytes checked against total-frame and directional caps.
    pub payload_len: usize,
}

/// 解码失败及是否必须关闭连接的判定。
/// Decoding failure and whether the connection must close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// 保留稳定错误码的失败说明。
    /// Failure description preserving the stable code.
    pub error: DbError,
    /// 为 true 时调用方必须关闭连接。
    /// When true, callers must close the connection.
    pub fatal: bool,
    /// 可识别的请求编号；无法读取时为零。
    /// Identifiable request number, or zero if unavailable.
    pub request_id: u64,
}

impl DecodeError {
    /// 构造致命结构错误，后续由外层补充可恢复的请求 ID。
    /// Builds a fatal structural error; outer layers attach any recoverable request ID.
    pub(crate) fn protocol(message: &'static str) -> Self {
        Self {
            error: DbError::new(ErrorCode::Protocol, message),
            fatal: true,
            request_id: 0,
        }
    }

    /// 结构解码预算耗尽，无法安全继续读取该帧。
    /// Structural decoding budget exhausted; the frame cannot be safely continued.
    pub(crate) fn limit() -> Self {
        Self::protocol("Frame exceeds decoding budget")
    }
}

impl std::fmt::Display for DecodeError {
    /// 直接保留已脱敏的协议错误说明。
    /// Retains the sanitized protocol error diagnostic.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for DecodeError {}
