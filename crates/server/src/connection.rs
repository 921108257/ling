use crate::Config;
use linger_protocol::{Body, Frame, ReadError, Request, encode_frame, read_frame};
use linger_types::{DbError, ErrorCode, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::watch,
    time::{Instant, timeout, timeout_at},
};

pub(crate) async fn serve(
    mut stream: TcpStream,
    config: &Config,
    engine: &std::sync::Mutex<linger_engine::Engine>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut session = Session {
        hello: false,
        authenticated: !config.auth.enabled,
        db: config.database.default_db,
        last_id: 0,
    };
    loop {
        if *shutdown.borrow() {
            break;
        }
        let received = tokio::select! {
            _ = shutdown.changed() => break,
            frame = receive(&mut stream, config) => frame,
        };
        let frame = match received {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(ReadError::Io(_)) => break,
            Err(ReadError::Decode(error)) => {
                let _ = send(
                    &mut stream,
                    Frame {
                        request_id: error.request_id,
                        body: Body::Error(error.error),
                    },
                    config,
                )
                .await;
                break;
            }
        };
        if matches!(frame.body, Body::Heartbeat) {
            if !session.hello {
                let _ = send(
                    &mut stream,
                    fatal(
                        0,
                        ErrorCode::ConnectionState,
                        "HELLO must be the first frame",
                    ),
                    config,
                )
                .await;
                break;
            }
            continue;
        }
        if !matches!(
            frame.body,
            Body::Request(_) | Body::BatchRequest(_) | Body::Ack(_)
        ) || frame.request_id != session.last_id.saturating_add(1)
            || session.last_id == u64::MAX
        {
            let _ = send(
                &mut stream,
                fatal(
                    frame.request_id,
                    ErrorCode::Protocol,
                    "Unexpected frame or request ID",
                ),
                config,
            )
            .await;
            break;
        }
        session.last_id = frame.request_id;
        if !session.hello && !matches!(&frame.body, Body::Request(r) if r.command == "HELLO") {
            let _ = send(
                &mut stream,
                fatal(
                    frame.request_id,
                    ErrorCode::ConnectionState,
                    "HELLO must be the first request",
                ),
                config,
            )
            .await;
            break;
        }
        let (result, close, fatal_error) = match frame.body {
            Body::Request(request) => session.execute(request, config, engine),
            _ if !session.authenticated => (
                Err(DbError::new(
                    ErrorCode::Unauthenticated,
                    "Authentication required",
                )),
                false,
                false,
            ),
            Body::BatchRequest(_) => (
                Err(DbError::new(
                    ErrorCode::InvalidArgument,
                    "Batch execution is not supported in stage 2",
                )),
                false,
                false,
            ),
            Body::Ack(_) => (
                Err(DbError::new(
                    ErrorCode::ConnectionState,
                    "No subscription context",
                )),
                false,
                false,
            ),
            _ => unreachable!(),
        };
        let body = if fatal_error {
            Body::Error(result.unwrap_err())
        } else {
            Body::Response(result)
        };
        if send(
            &mut stream,
            Frame {
                request_id: frame.request_id,
                body,
            },
            config,
        )
        .await
        .is_err()
            || close
        {
            break;
        }
    }
    let _ = stream.shutdown().await;
}

fn fatal(id: u64, code: ErrorCode, message: &'static str) -> Frame {
    Frame {
        request_id: id,
        body: Body::Error(DbError::new(code, message)),
    }
}

// Serial execution is the stage-1 baseline and naturally enforces all barriers.
struct Session {
    hello: bool,
    authenticated: bool,
    db: u16,
    last_id: u64,
}
impl Session {
    fn execute(
        &mut self,
        request: Request,
        config: &Config,
        engine: &std::sync::Mutex<linger_engine::Engine>,
    ) -> (Result<Value, DbError>, bool, bool) {
        let name = request.command.as_str();
        if !self.authenticated && self.hello && !matches!(name, "AUTH" | "PING" | "QUIT") {
            return (
                Err(DbError::new(
                    ErrorCode::Unauthenticated,
                    "Authentication required",
                )),
                false,
                false,
            );
        }
        if name == "HELLO" && self.hello {
            return (
                Err(DbError::new(
                    ErrorCode::ConnectionState,
                    "HELLO already completed",
                )),
                false,
                false,
            );
        }
        if let Some(error) = request.value_error {
            return (Err(error), !self.hello, !self.hello);
        }
        let args = request.arguments.as_slice();
        let invalid = || {
            Err(DbError::new(
                ErrorCode::InvalidArgument,
                "Invalid command arguments",
            ))
        };
        let mut close = false;
        let mut fatal = false;
        let result = match name {
            "HELLO" => match args {
                [Value::UInt(1)] => {
                    self.hello = true;
                    Ok(Value::Map(BTreeMap::from([
                        ("protocol".into(), Value::UInt(1)),
                        (
                            "server_version".into(),
                            Value::String(env!("CARGO_PKG_VERSION").into()),
                        ),
                        ("events".into(), Value::Bool(false)),
                        ("reliable_events".into(), Value::Bool(false)),
                        ("stream_id".into(), Value::Null),
                    ])))
                }
                [Value::UInt(_)] => {
                    close = true;
                    fatal = true;
                    Err(DbError::new(
                        ErrorCode::UnsupportedVersion,
                        "Unsupported HELLO version",
                    ))
                }
                _ => {
                    close = true;
                    fatal = true;
                    invalid()
                }
            },
            "PING" => match args {
                [] => Ok(Value::String("PONG".into())),
                [Value::String(v)] => Ok(Value::String(v.clone())),
                _ => invalid(),
            },
            "AUTH" => match args {
                [Value::String(password)] => {
                    // A failed reauthentication never leaves an authenticated session.
                    self.authenticated = !config.auth.enabled || password == &config.auth.password;
                    if self.authenticated {
                        Ok(Value::Bool(true))
                    } else {
                        Err(DbError::new(ErrorCode::AuthFailed, "Authentication failed"))
                    }
                }
                _ => invalid(),
            },
            "SELECT" => match args {
                [Value::UInt(db)] if *db < u64::from(config.database.databases) => {
                    self.db = *db as u16;
                    Ok(Value::Bool(true))
                }
                [Value::UInt(_)] => Err(DbError::new(
                    ErrorCode::DbOutOfRange,
                    "Database out of range",
                )),
                _ => invalid(),
            },
            "QUIT" => {
                if args.is_empty() {
                    close = true;
                    Ok(Value::Bool(true))
                } else {
                    invalid()
                }
            }
            _ => match engine.lock() {
                Ok(mut engine) => {
                    engine.execute(self.db, name, args, config.server.max_response_bytes)
                }
                Err(_) => Err(DbError::new(ErrorCode::Busy, "Data engine unavailable")),
            },
        };
        (result, close, fatal)
    }
}

async fn receive(stream: &mut TcpStream, config: &Config) -> Result<Option<Frame>, ReadError> {
    let idle = config.server.idle_timeout_ms;
    // Peek only to separate idle time from the deadline for a partially received frame.
    let mut first = [0];
    let peek = if idle == 0 {
        stream.peek(&mut first).await
    } else {
        match timeout(Duration::from_millis(idle), stream.peek(&mut first)).await {
            Ok(value) => value,
            Err(_) => return Ok(None),
        }
    };
    match peek {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) => return Err(ReadError::Io(error)),
    }
    let read = read_frame(stream, config.limits());
    if config.server.request_timeout_ms == 0 {
        read.await
    } else {
        timeout(
            Duration::from_millis(config.server.request_timeout_ms),
            read,
        )
        .await
        .unwrap_or(Ok(None))
    }
}

async fn send(stream: &mut TcpStream, frame: Frame, config: &Config) -> Result<(), ()> {
    let limits = config.limits();
    let bytes = encode_frame(&frame, limits)
        .or_else(|error| {
            encode_frame(
                &Frame {
                    request_id: frame.request_id,
                    body: Body::Response(Err(error)),
                },
                limits,
            )
        })
        .map_err(|_| ())?;
    let deadline = Instant::now()
        + Duration::from_millis(if config.server.request_timeout_ms == 0 {
            config.server.shutdown_timeout_ms
        } else {
            config.server.request_timeout_ms
        });
    for chunk in bytes.chunks(config.server.write_buffer_bytes) {
        timeout_at(deadline, stream.write_all(chunk))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
    }
    Ok(())
}
