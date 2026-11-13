//! Stage-1 async client. One caller owns a connection and its database context.

use linger_protocol::{Body, Frame, Limits, ReadError, Request, encode_frame, read_frame};
pub use linger_types::{DbError, ErrorCode, Float, Scalar, Value};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

#[derive(Clone, Debug)]
pub struct Options {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub limits: Limits,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            limits: Limits::default(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Server(DbError),
    Local {
        kind: LocalError,
        outcome_unknown: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalError {
    Connect,
    Timeout,
    Disconnected,
    Protocol,
    InvalidRequest,
    Closed,
}

impl Error {
    /// Sending a request is not proof that it was executed, or that it was not.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Server(error) => error.code == ErrorCode::CommitUnknown.as_str(),
            Self::Local {
                outcome_unknown, ..
            } => *outcome_unknown,
        }
    }
    fn local(kind: LocalError, outcome_unknown: bool) -> Self {
        Self::Local {
            kind,
            outcome_unknown,
        }
    }
}
impl std::fmt::Display for Error {
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

pub struct Client {
    stream: Option<TcpStream>,
    options: Options,
    last_id: u64,
    database: Option<u16>,
    hello: BTreeMap<String, Value>,
}

impl Client {
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
        if let Some(password) = password {
            if client
                .execute("AUTH", vec![Value::String(password.into())])
                .await?
                != Value::Bool(true)
            {
                return Err(Error::local(LocalError::Protocol, false));
            }
        }
        client.select(database).await?;
        Ok(client)
    }

    pub fn capabilities(&self) -> &BTreeMap<String, Value> {
        &self.hello
    }
    pub fn database(&self) -> Option<u16> {
        self.database
    }
    pub fn is_closed(&self) -> bool {
        self.stream.is_none()
    }

    /// No automatic replay. Dropping this future after polling closes the stream:
    /// it is taken out of self before the first network await.
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
        let bytes = encode_frame(
            &Frame {
                request_id: id,
                body: Body::Request(request),
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
