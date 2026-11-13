//! 稳定错误注册表与跨版本保留原始错误码的载体。
//! Stable error registry and errors preserving wire codes across versions.

use std::fmt;

/// 由同一注册表生成枚举和线上映射，避免二者漂移。
/// Generates the enum and wire mapping from one registry to prevent divergence.
macro_rules! error_codes {
    ($(#[doc = $doc:literal] $variant:ident => $code:literal),+ $(,)?) => {
        /// 服务端稳定错误码；业务逻辑应比较错误码而非说明文本。
        /// Stable server error codes; business logic compares codes rather than messages.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ErrorCode { $(#[doc = $doc] $variant),+ }

        impl ErrorCode {
            /// 完整注册表，用于唯一性及兼容性校验。
            /// Complete registry for uniqueness and compatibility checks.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// 返回冻结的线上标识。
            /// Returns the frozen wire identifier.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }
        }
    };
}

error_codes! {
    #[doc = "命令未知或尚未实现。 Unknown or unimplemented command."]
    UnknownCommand => "ERR_UNKNOWN_COMMAND",
    #[doc = "参数数量、类型或选项非法。 Invalid argument count, type, or option."]
    InvalidArgument => "ERR_INVALID_ARGUMENT",
    #[doc = "Key 不存在或逻辑过期。 Key missing or logically expired."]
    KeyNotFound => "ERR_KEY_NOT_FOUND",
    #[doc = "Map 字段不存在。 Map field missing."]
    FieldNotFound => "ERR_FIELD_NOT_FOUND",
    #[doc = "存储值类型不符合命令。 Stored type does not match the command."]
    TypeMismatch => "ERR_TYPE_MISMATCH",
    #[doc = "Value 内容非法。 Invalid Value contents."]
    InvalidValue => "ERR_INVALID_VALUE",
    #[doc = "数值或身份计数溢出。 Numeric or identity counter overflow."]
    Overflow => "ERR_OVERFLOW",
    #[doc = "单项 List 索引越界。 Single List index out of range."]
    IndexOutOfRange => "ERR_INDEX_OUT_OF_RANGE",
    #[doc = "请求、响应或值超限。 Request, response, or Value limit exceeded."]
    LimitExceeded => "ERR_LIMIT_EXCEEDED",
    #[doc = "缓存预算不足。 Insufficient cache budget."]
    OutOfMemory => "ERR_OUT_OF_MEMORY",
    #[doc = "数据库编号越界。 Database number out of range."]
    DbOutOfRange => "ERR_DB_OUT_OF_RANGE",
    #[doc = "遍历游标非法或失效。 Invalid or expired scan cursor."]
    InvalidCursor => "ERR_INVALID_CURSOR",
    #[doc = "需要先认证。 Authentication required."]
    Unauthenticated => "ERR_UNAUTHENTICATED",
    #[doc = "认证失败。 Authentication failed."]
    AuthFailed => "ERR_AUTH_FAILED",
    #[doc = "致命结构协议错误。 Fatal structural protocol error."]
    Protocol => "ERR_PROTOCOL",
    #[doc = "不支持的协议版本。 Unsupported protocol version."]
    UnsupportedVersion => "ERR_UNSUPPORTED_VERSION",
    #[doc = "连接状态不允许操作。 Operation disallowed in this connection state."]
    ConnectionState => "ERR_CONNECTION_STATE",
    #[doc = "服务过载或不可用。 Service overloaded or unavailable."]
    Busy => "ERR_BUSY",
    #[doc = "配置值或组合非法。 Invalid configuration value or combination."]
    ConfigInvalid => "ERR_CONFIG_INVALID",
    #[doc = "静态配置不可热改。 Static configuration cannot change at runtime."]
    ConfigImmutable => "ERR_CONFIG_IMMUTABLE",
    #[doc = "事件功能未启用。 Events disabled."]
    EventsDisabled => "ERR_EVENTS_DISABLED",
    #[doc = "消费组定义不匹配。 Consumer-group definition mismatch."]
    GroupMismatch => "ERR_GROUP_MISMATCH",
    #[doc = "确认不属于当前交付。 Acknowledgement does not belong to the current delivery."]
    InvalidAck => "ERR_INVALID_ACK",
    #[doc = "消费位置超出保留窗口。 Consumer offset outside the retention window."]
    OffsetExpired => "ERR_OFFSET_EXPIRED",
    #[doc = "持久流身份变化。 Persistent stream identity changed."]
    StreamMismatch => "ERR_STREAM_MISMATCH",
    #[doc = "Journal 在提交前不可写。 Journal unavailable before commit."]
    JournalUnavailable => "ERR_JOURNAL_UNAVAILABLE",
    #[doc = "Journal 容量不足。 Journal capacity exhausted."]
    JournalFull => "ERR_JOURNAL_FULL",
    #[doc = "提交结果未知，禁止自动重放。 Commit outcome unknown; do not replay automatically."]
    CommitUnknown => "ERR_COMMIT_UNKNOWN",
}

/// 即使新服务端添加 SDK 未知的错误，也保留原始线上错误码。
/// Keeps the wire code even when a newer server adds a code unknown to this SDK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    /// 稳定线上错误码，可能来自更新版本。
    /// Stable wire code, possibly from a newer version.
    pub code: String,
    /// 诊断说明，不应用作分支条件。
    /// Diagnostic message, unsuitable for branching.
    pub message: String,
}

impl DbError {
    /// 从注册表构造错误；调用方不应附带凭据或完整 Value。
    /// Constructs a registered error; callers must not include credentials or full Values.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for DbError {
    /// 仅展示稳定错误码和安全说明。
    /// Displays only the stable code and safe diagnostic message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DbError {}
