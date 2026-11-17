//! 异步 Rust 客户端；调用方独占连接和数据库上下文，不自动重放请求。
//! Async Rust client; callers own the connection and database context, with no automatic request replay.

use ling_protocol::{Body, Frame, Limits, ReadError, encode_frame, read_frame};
pub use ling_types::{DbError, ErrorCode, Event, EventId, EventKind, Float, Scalar, Value};
mod pool;
mod subscriber;
pub use ling_network::ClientTls;
pub use ling_protocol::Request;
pub use pool::{Pool, PoolOptions, PooledClient};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
pub use subscriber::Subscriber;
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

/// 连接、请求时限和编码预算；连接时拒绝零值或无法表示的时限。
/// Connection and request deadlines with codec budgets; zero or unrepresentable deadlines fail at connect.
#[derive(Clone, Debug)]
pub struct Options {
    /// TCP 建连的总时限。
    /// Overall TCP connection deadline.
    pub connect_timeout: Duration,
    /// 一次请求发送与匹配响应的总时限。
    /// Overall deadline for sending a request and receiving its matching response.
    pub request_timeout: Duration,
    /// 客户端独立执行的协议资源上限。
    /// Protocol resource caps enforced independently by the client.
    pub limits: Limits,
    /// 可选 TLS；默认仅 TCP，启用后始终验证服务端身份。
    /// Optional TLS; defaults to TCP and always verifies server identity when enabled.
    pub tls: Option<ClientTls>,
}
impl Default for Options {
    /// 默认建连 5 秒、请求 30 秒，使用标准协议预算。
    /// Defaults to a five-second connect and thirty-second request deadline with standard codec budgets.
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            limits: Limits::default(),
            tls: None,
        }
    }
}

/// 区分服务端业务失败与客户端传输失败，保留结果未知语义。
/// Separates server failures from client transport failures while retaining unknown-outcome semantics.
#[derive(Debug)]
pub enum Error {
    /// 服务端返回的稳定错误。
    /// Stable error returned by the server.
    Server(DbError),
    /// 客户端连接、协议或生命周期错误。
    /// Client connection, protocol, or lifecycle failure.
    Local {
        /// 本地错误分类。
        /// Local failure category.
        kind: LocalError,
        /// 为真时无法确认已发送请求的执行结果。
        /// True when the outcome of a sent request cannot be confirmed.
        outcome_unknown: bool,
    },
}

/// 不包含凭据和原始 Value 的本地错误分类。
/// Local error categories containing neither credentials nor raw Values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalError {
    /// TCP 建连或初始化套接字失败。
    /// TCP connection or socket initialization failed.
    Connect,
    /// 建立连接或等待请求超过时限。
    /// Connection establishment or request wait timed out.
    Timeout,
    /// 请求期间流关闭或发生 I/O 错误。
    /// Stream closed or I/O failed during a request.
    Disconnected,
    /// 服务端帧或能力不满足协议契约。
    /// Server frame or capabilities violated the protocol contract.
    Protocol,
    /// 请求在发送前无法编码或配置非法。
    /// Request could not be encoded before sending, or configuration was invalid.
    InvalidRequest,
    /// 连接已经关闭或请求编号耗尽。
    /// Connection closed or request numbers exhausted.
    Closed,
    /// 订阅等待 ACK 时的有界事件队列耗尽；Subscription queue exhausted while awaiting an ACK response.
    BufferFull,
}

impl Error {
    /// 发送不代表已执行或未执行；调用方必须显式处理结果未知。
    /// Sending proves neither execution nor nonexecution; callers must handle unknown outcomes.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Server(error) => error.code == ErrorCode::CommitUnknown.as_str(),
            Self::Local {
                outcome_unknown, ..
            } => *outcome_unknown,
        }
    }
    /// 构造本地错误并明确记录执行结果是否未知。
    /// Constructs a local error with an explicit unknown-outcome flag.
    fn local(kind: LocalError, outcome_unknown: bool) -> Self {
        Self::Local {
            kind,
            outcome_unknown,
        }
    }
}
impl std::fmt::Display for Error {
    /// 保留错误分类及结果未知状态，不回显请求内容。
    /// Preserves failure category and unknown outcome without echoing request contents.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(error) => error.fmt(f),
            Self::Local {
                kind,
                outcome_unknown,
            } => write!(
                f,
                "Client {kind:?}{}",
                if *outcome_unknown {
                    "; request outcome unknown"
                } else {
                    ""
                }
            ),
        }
    }
}
impl std::error::Error for Error {}

/// 单调用方连接；通过可变借用保证屏障，不共享可变 SELECT 状态。
/// Single-caller connection; mutable borrowing enforces barriers without shared SELECT state.
pub struct Client {
    /// 请求等待时移出，取消即关闭。
    /// Taken during request waits so cancellation closes it.
    stream: Option<ling_network::Stream>,
    /// 创建时固定的连接预算。
    /// Connection budgets fixed at creation.
    options: Options,
    /// 单调请求编号，不复用已发送 ID。
    /// Monotonic request number; sent IDs are never reused.
    last_id: u64,
    /// 最后一次成功 SELECT 的结果。
    /// Result of the last successful SELECT.
    database: Option<u16>,
    /// 校验通过的能力映射。
    /// Validated capability map.
    hello: BTreeMap<String, Value>,
}

impl Client {
    /// 连接并依次执行 HELLO、可选 AUTH 和 SELECT；任一步失败均释放连接。
    /// Connects then performs HELLO, optional AUTH, and SELECT; any failure releases the connection.
    pub async fn connect(
        address: SocketAddr,
        password: Option<&str>,
        database: u16,
        options: Options,
    ) -> Result<Self, Error> {
        if options.connect_timeout.is_zero() || options.request_timeout.is_zero() {
            return Err(Error::local(LocalError::InvalidRequest, false));
        }
        if [options.connect_timeout, options.request_timeout]
            .iter()
            .any(|duration| std::time::Instant::now().checked_add(*duration).is_none())
        {
            return Err(Error::local(LocalError::InvalidRequest, false));
        }
        let stream = timeout(options.connect_timeout, TcpStream::connect(address))
            .await
            .map_err(|_| Error::local(LocalError::Timeout, false))?
            .map_err(|_| Error::local(LocalError::Connect, false))?;
        stream
            .set_nodelay(true)
            .map_err(|_| Error::local(LocalError::Connect, false))?;
        let stream: ling_network::Stream = if let Some(tls) = &options.tls {
            let (connector, name) = tls.connector().map_err(Error::Server)?;
            Box::new(
                timeout(options.connect_timeout, connector.connect(name, stream))
                    .await
                    .map_err(|_| Error::local(LocalError::Timeout, false))?
                    .map_err(|_| Error::local(LocalError::Connect, false))?,
            )
        } else {
            Box::new(stream)
        };
        let mut client = Self {
            stream: Some(stream),
            options,
            last_id: 0,
            database: None,
            hello: BTreeMap::new(),
        };
        let hello = client.execute("HELLO", vec![Value::UInt(1)]).await?;
        let Value::Map(hello) = hello else {
            return Err(Error::local(LocalError::Protocol, false));
        };
        if hello.get("protocol") != Some(&Value::UInt(1))
            || !matches!(hello.get("server_version"), Some(Value::String(_)))
            || !matches!(hello.get("events"), Some(Value::Bool(_)))
            || !matches!(hello.get("reliable_events"), Some(Value::Bool(_)))
            || !matches!(
                hello.get("stream_id"),
                Some(Value::Null) | Some(Value::Bytes(_))
            )
        {
            return Err(Error::local(LocalError::Protocol, false));
        }
        let events = hello.get("events") == Some(&Value::Bool(true));
        let reliable = hello.get("reliable_events") == Some(&Value::Bool(true));
        let valid_stream = match (events, hello.get("stream_id")) {
            (false, Some(Value::Null)) => true,
            (true, Some(Value::Bytes(bytes))) => bytes.len() == 16,
            _ => false,
        };
        if (reliable && !events) || !valid_stream {
            return Err(Error::local(LocalError::Protocol, false));
        }
        client.hello = hello;
        if let Some(password) = password
            && client
                .execute("AUTH", vec![Value::String(password.into())])
                .await?
                != Value::Bool(true)
        {
            return Err(Error::local(LocalError::Protocol, false));
        }
        client.select(database).await?;
        Ok(client)
    }

    /// 在恢复订阅前显式校验旧流；变化时关闭连接，不创建新组。
    /// Explicitly checks a saved stream before resubscribing; changes close the connection without creating a group.
    pub fn require_event_stream(&mut self, expected: u128) -> Result<(), Error> {
        let expected = Value::Bytes(expected.to_be_bytes().to_vec());
        if self.hello.get("stream_id") != Some(&expected) {
            self.stream.take();
            return Err(Error::Server(DbError::new(
                ErrorCode::StreamMismatch,
                "Event stream changed",
            )));
        }
        Ok(())
    }

    /// 读取握手确认的服务端能力，不推断未公布的功能。
    /// Reads capabilities confirmed by the handshake without inferring unadvertised features.
    pub fn capabilities(&self) -> &BTreeMap<String, Value> {
        &self.hello
    }
    /// 返回最后确认成功选择的数据库编号。
    /// Returns the last database number confirmed by a successful SELECT.
    pub fn database(&self) -> Option<u16> {
        self.database
    }
    /// 连接关闭后不会隐式重连。
    /// Closed connections never reconnect implicitly.
    pub fn is_closed(&self) -> bool {
        self.stream.is_none()
    }

    /// 执行任意类型化命令，不自动重放；超时、断线或取消后释放连接。
    /// Executes any typed command without replay; timeout, disconnect, or cancellation releases the socket.
    ///
    /// 第一次网络等待前移出套接字，避免取消后迟到响应污染下一请求。
    /// Takes the socket before the first network await so cancellation cannot misroute a late response.
    pub async fn execute(&mut self, command: &str, arguments: Vec<Value>) -> Result<Value, Error> {
        let request = Request::new(command, arguments);
        let is_quit = request.command == "QUIT";
        let selected_db = if request.command == "SELECT" {
            match request.arguments.as_slice() {
                [Value::UInt(db)] => u16::try_from(*db).ok(),
                _ => None,
            }
        } else {
            None
        };
        let Some(id) = self.last_id.checked_add(1) else {
            self.stream.take();
            return Err(Error::local(LocalError::Closed, false));
        };
        let body = if request.command == "ACK" {
            let [id] = request.arguments.as_slice() else {
                return Err(Error::local(LocalError::InvalidRequest, false));
            };
            EventId::from_value(id).map_err(|_| Error::local(LocalError::InvalidRequest, false))?;
            Body::Ack(id.clone())
        } else {
            Body::Request(request)
        };
        let bytes = encode_frame(
            &Frame {
                request_id: id,
                body,
            },
            self.options.limits,
        )
        .map_err(|_| Error::local(LocalError::InvalidRequest, false))?;
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        self.last_id = id;
        let exchange = async {
            stream
                .write_all(&bytes)
                .await
                .map_err(|_| Error::local(LocalError::Disconnected, true))?;
            loop {
                let frame = read_frame(&mut stream, self.options.limits)
                    .await
                    .map_err(|e| match e {
                        ReadError::Io(_) => Error::local(LocalError::Disconnected, true),
                        ReadError::Decode(_) => Error::local(LocalError::Protocol, true),
                    })?
                    .ok_or_else(|| Error::local(LocalError::Disconnected, true))?;
                match frame.body {
                    Body::Heartbeat => continue,
                    Body::Response(_) if frame.request_id < id => continue,
                    Body::Response(result) if frame.request_id == id => return Ok(result),
                    Body::Error(error) => return Err(Error::Server(error)),
                    _ => return Err(Error::local(LocalError::Protocol, true)),
                }
            }
        };
        match timeout(self.options.request_timeout, exchange).await {
            Ok(Ok(result)) => {
                if !is_quit || result.is_err() {
                    self.stream = Some(stream);
                }
                if let Some(db) = selected_db.filter(|_| result == Ok(Value::Bool(true))) {
                    self.database = Some(db);
                }
                result.map_err(Error::Server)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::local(LocalError::Timeout, true)),
        }
    }

    /// 普通批量请求，结果按输入顺序返回；整体无事务性，传输失败可能有部分执行。
    /// Ordinary batch with input-ordered results; it is nontransactional and transport failure may follow partial execution.
    pub async fn batch(
        &mut self,
        requests: Vec<Request>,
    ) -> Result<Vec<Result<Value, DbError>>, Error> {
        let count = requests.len();
        let id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        let bytes = encode_frame(
            &Frame {
                request_id: id,
                body: Body::BatchRequest(requests),
            },
            self.options.limits,
        )
        .map_err(|_| Error::local(LocalError::InvalidRequest, false))?;
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        self.last_id = id;
        let mut fatal = false;
        let exchange = async {
            stream
                .write_all(&bytes)
                .await
                .map_err(|_| Error::local(LocalError::Disconnected, true))?;
            loop {
                let frame = read_frame(&mut stream, self.options.limits)
                    .await
                    .map_err(|_| Error::local(LocalError::Disconnected, true))?
                    .ok_or_else(|| Error::local(LocalError::Disconnected, true))?;
                match frame.body {
                    Body::Heartbeat => {}
                    Body::BatchResponse(results)
                        if frame.request_id == id && results.len() == count =>
                    {
                        return Ok(results);
                    }
                    Body::Response(Err(e)) if frame.request_id == id => {
                        return Err(Error::Server(e));
                    }
                    Body::Error(e) => {
                        fatal = true;
                        return Err(Error::Server(e));
                    }
                    _ => return Err(Error::local(LocalError::Protocol, true)),
                }
            }
        };
        match timeout(self.options.request_timeout, exchange).await {
            Ok(Ok(results)) => {
                self.stream = Some(stream);
                Ok(results)
            }
            Ok(Err(Error::Server(e))) if !fatal => {
                self.stream = Some(stream);
                Err(Error::Server(e))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::local(LocalError::Timeout, true)),
        }
    }

    /// 发送可选 String 回显，返回服务端的类型化结果。
    /// Sends an optional String echo and returns the typed server result.
    pub async fn ping(&mut self, message: Option<&str>) -> Result<Value, Error> {
        self.execute(
            "PING",
            message
                .into_iter()
                .map(|s| Value::String(s.into()))
                .collect(),
        )
        .await
    }
    /// 等待 SELECT 成功后更新上下文，失败保留原库号。
    /// Updates context only after SELECT succeeds; failure retains the prior database.
    pub async fn select(&mut self, database: u16) -> Result<(), Error> {
        if self
            .execute("SELECT", vec![Value::UInt(u64::from(database))])
            .await?
            != Value::Bool(true)
        {
            self.stream.take();
            return Err(Error::local(LocalError::Protocol, false));
        }
        Ok(())
    }
    /// 发送 QUIT 并释放套接字；重复关闭为成功无操作。
    /// Sends QUIT and releases the socket; repeated close is a successful no-op.
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        let result = self.execute("QUIT", vec![]).await;
        self.stream.take();
        match result {
            Ok(Value::Bool(true)) => Ok(()),
            Ok(_) => Err(Error::local(LocalError::Protocol, false)),
            Err(error) => Err(error),
        }
    }
}
