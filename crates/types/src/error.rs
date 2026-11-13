use std::fmt;

macro_rules! error_codes {
    ($($variant:ident => $code:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ErrorCode { $($variant),+ }

        impl ErrorCode {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }
        }
    };
}

error_codes! {
    UnknownCommand => "ERR_UNKNOWN_COMMAND",
    InvalidArgument => "ERR_INVALID_ARGUMENT",
    KeyNotFound => "ERR_KEY_NOT_FOUND",
    FieldNotFound => "ERR_FIELD_NOT_FOUND",
    TypeMismatch => "ERR_TYPE_MISMATCH",
    InvalidValue => "ERR_INVALID_VALUE",
    Overflow => "ERR_OVERFLOW",
    IndexOutOfRange => "ERR_INDEX_OUT_OF_RANGE",
    LimitExceeded => "ERR_LIMIT_EXCEEDED",
    OutOfMemory => "ERR_OUT_OF_MEMORY",
    DbOutOfRange => "ERR_DB_OUT_OF_RANGE",
    InvalidCursor => "ERR_INVALID_CURSOR",
    Unauthenticated => "ERR_UNAUTHENTICATED",
    AuthFailed => "ERR_AUTH_FAILED",
    Protocol => "ERR_PROTOCOL",
    UnsupportedVersion => "ERR_UNSUPPORTED_VERSION",
    ConnectionState => "ERR_CONNECTION_STATE",
    Busy => "ERR_BUSY",
    ConfigInvalid => "ERR_CONFIG_INVALID",
    ConfigImmutable => "ERR_CONFIG_IMMUTABLE",
    EventsDisabled => "ERR_EVENTS_DISABLED",
    GroupMismatch => "ERR_GROUP_MISMATCH",
    InvalidAck => "ERR_INVALID_ACK",
    OffsetExpired => "ERR_OFFSET_EXPIRED",
    StreamMismatch => "ERR_STREAM_MISMATCH",
    JournalUnavailable => "ERR_JOURNAL_UNAVAILABLE",
    JournalFull => "ERR_JOURNAL_FULL",
    CommitUnknown => "ERR_COMMIT_UNKNOWN",
}

/// Keep the wire code even when a newer server adds a code unknown to this SDK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    pub code: String,
    pub message: String,
}

impl DbError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DbError {}
