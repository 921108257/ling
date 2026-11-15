//! 单连接状态机、认证和选库屏障；网络等待不持有数据引擎锁。
//! Per-connection state, authentication, and database barriers; network waits never hold the engine lock.

use crate::Config;
use linger_protocol::{Body, Frame, ReadError, Request, encode_frame, read_frame};
use linger_types::{DbError, ErrorCode, Value};
use std::collections::BTreeMap;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::{TcpStream, tcp::OwnedReadHalf},
    sync::{Semaphore, watch},
    time::{Instant, timeout, timeout_at},
};

/// 按接收顺序处理请求，致命帧错误或关闭信号结束连接。
/// Processes requests in receive order and closes on fatal framing errors or shutdown.
pub(crate) async fn serve(
    stream: TcpStream,
    config: &Config,
    engine: &Arc<linger_engine::Engine>,
    slots: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (mut reader, mut stream) = stream.into_split();
    let (frames, mut incoming) = tokio::sync::mpsc::channel(1);
    let read_config = config.clone();
    let mut reading = tokio::task::JoinSet::new();
    // 独立有界读任务保存半帧状态；事件推送不会取消正在进行的帧读取。
    // A bounded reader preserves partial-frame state; event pushes never cancel frame reads.
    reading.spawn(async move {
        loop {
            let frame = receive(&mut reader, &read_config).await;
            let done = !matches!(frame, Ok(Some(_)));
            if frames.send(frame).await.is_err() || done {
                break;
            }
        }
    });
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut session = Session {
        hello: false,
        authenticated: !config.auth.enabled,
        db: config.database.default_db,
        last_id: 0,
        scan: linger_engine::ScanState::default(),
        subscription: None,
        events: engine.events().cloned(),
    };
    loop {
        if *shutdown.borrow() {
            break;
        }
        let received = tokio::select! {
            _ = shutdown.changed() => break,
            frame = incoming.recv() => match frame {Some(frame)=>frame,None=>break},
            _ = poll.tick(), if session.subscription.is_some() => {
                let sub=session.subscription.as_ref().unwrap().clone();
                let store=session.events.as_ref().unwrap().clone();
                match event_work(slots.clone(),config,move ||store.next(&sub.token,std::time::Instant::now())).await {
                    Ok(Some(event))=>{
                        if send(&mut stream,Frame{request_id:0,body:Body::Event(event.to_value())},config).await.is_err(){break;}
                    }
                    Ok(None)=>{}
                    Err(error) if error.code==ErrorCode::Busy.as_str()=>{}
                    Err(error)=>{
                        let _=send(&mut stream,Frame{request_id:0,body:Body::Error(error)},config).await;
                        break;
                    }
                }
                continue;
            }
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
            Body::Request(request) => {
                session
                    .execute(request, config, engine, slots.clone())
                    .await
            }
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
                    if session.subscription.is_some() {
                        ErrorCode::ConnectionState
                    } else {
                        ErrorCode::InvalidArgument
                    },
                    "Batch execution is not supported",
                )),
                false,
                false,
            ),
            Body::Ack(value) => {
                let result = match (
                    session.subscription.as_ref(),
                    session.events.as_ref(),
                    linger_types::EventId::from_value(&value),
                ) {
                    (Some(sub), Some(store), Ok(id)) => {
                        let (sub, store) = (sub.clone(), store.clone());
                        event_work(slots.clone(), config, move || {
                            store.ack(&sub.token, id).map(|()| Value::Bool(true))
                        })
                        .await
                    }
                    (_, _, Err(e)) => Err(e),
                    _ => Err(DbError::new(
                        ErrorCode::ConnectionState,
                        "No subscription context",
                    )),
                };
                (result, false, false)
            }
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

/// 与数据命令共享阻塞许可；开始后等待真实提交结果。
/// Shares blocking permits with data commands and awaits actual outcomes once started.
async fn event_work<T: Send + 'static>(
    slots: Arc<Semaphore>,
    config: &Config,
    work: impl FnOnce() -> Result<T, DbError> + Send + 'static,
) -> Result<T, DbError> {
    let waiting = slots.acquire_owned();
    let permit = if config.server.request_timeout_ms == 0 {
        waiting.await.ok()
    } else {
        timeout(
            Duration::from_millis(config.server.request_timeout_ms),
            waiting,
        )
        .await
        .ok()
        .and_then(Result::ok)
    };
    let Some(permit) = permit else {
        return Err(DbError::new(ErrorCode::Busy, "Event execution queue full"));
    };
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| {
        DbError::new(
            ErrorCode::CommitUnknown,
            "Event task failed; outcome unknown",
        )
    })?
}

/// 构造致命 ERROR 帧，与普通命令错误区分。
/// Builds a fatal ERROR frame, distinct from an ordinary command error.
fn fatal(id: u64, code: ErrorCode, message: &'static str) -> Frame {
    Frame {
        request_id: id,
        body: Body::Error(DbError::new(code, message)),
    }
}

// 连接内串行执行；跨连接操作由引擎协调相关分片的执行权。
// Connections execute serially; the engine coordinates ownership of relevant shards across connections.
/// 连接独占状态；串行执行明确保证 AUTH / SELECT / QUIT 屏障。
/// Connection-owned state; serial execution explicitly enforces AUTH, SELECT, and QUIT barriers.
struct Session {
    /// 是否完成唯一一次 HELLO。
    /// Whether the sole HELLO handshake completed.
    hello: bool,
    /// 当前认证状态；失败重认证会清除。
    /// Current authentication state, revoked by failed reauthentication.
    authenticated: bool,
    /// 当前连接选择的数据库。
    /// Database selected by this connection.
    db: u16,
    /// 最后接受的请求编号，避免重用。
    /// Last accepted request number, preventing reuse.
    last_id: u64,
    /// 当前连接唯一游标，断线即释放；Connection-owned cursor, released on disconnect.
    scan: linger_engine::ScanState,
    /// 专用连接拥有的消费身份；Consumption identity owned by this dedicated connection.
    subscription: Option<Arc<Consumer>>,
    /// 事件句柄保证取消时释放租约；Event handle releases leases on cancellation.
    events: Option<Arc<linger_events::EventStore>>,
}
/// 订阅结果自带释放责任，包括阻塞任务完成前连接已取消的情况。
/// Subscription results own cleanup, including connection cancellation before blocking tasks complete.
struct Consumer {
    /// 消费令牌；Consumption token.
    token: linger_events::Subscription,
    /// 保证释放时存储仍存活；Keeps storage alive until cleanup.
    store: Arc<linger_events::EventStore>,
}
impl Drop for Consumer {
    /// 最后一个在途操作释放后归还租约；Releases leases after the last in-flight operation.
    fn drop(&mut self) {
        self.store.disconnect(&self.token);
    }
}
impl Session {
    /// 先验证连接权限与参数，再在同步临界区执行数据命令。
    /// Validates connection permissions and arguments, then executes data commands in a synchronous critical section.
    async fn execute(
        &mut self,
        request: Request,
        config: &Config,
        engine: &Arc<linger_engine::Engine>,
        slots: Arc<Semaphore>,
    ) -> (Result<Value, DbError>, bool, bool) {
        let name = request.command.as_str();
        if self.subscription.is_some() && !matches!(name, "PING" | "QUIT" | "UNSUBSCRIBE") {
            return (
                Err(DbError::new(
                    ErrorCode::ConnectionState,
                    "Dedicated subscription connection",
                )),
                false,
                false,
            );
        }
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
                    let stream_id = match self.events.as_ref().filter(|s| s.enabled()) {
                        Some(store) => match store.stream_id() {
                            Ok(id) => Value::Bytes(id.to_be_bytes().to_vec()),
                            Err(e) => return (Err(e), true, true),
                        },
                        None => Value::Null,
                    };
                    self.hello = true;
                    Ok(Value::Map(BTreeMap::from([
                        ("protocol".into(), Value::UInt(1)),
                        (
                            "server_version".into(),
                            Value::String(env!("CARGO_PKG_VERSION").into()),
                        ),
                        (
                            "events".into(),
                            Value::Bool(self.events.as_ref().is_some_and(|s| s.enabled())),
                        ),
                        (
                            "reliable_events".into(),
                            Value::Bool(self.events.as_ref().is_some_and(|s| s.reliable())),
                        ),
                        ("stream_id".into(), stream_id),
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
            "SUBSCRIBE" => {
                let parsed = match args {
                    [Value::List(patterns)] => Some((patterns, None)),
                    [
                        Value::List(patterns),
                        Value::String(group),
                        Value::String(name),
                    ] if group == "GROUP" => Some((patterns, Some(name.clone()))),
                    _ => None,
                };
                if let Some((patterns, name)) = parsed {
                    let patterns = patterns
                        .iter()
                        .map(|p| match p {
                            Value::String(s) => Ok(s.clone()),
                            _ => Err(DbError::new(
                                ErrorCode::InvalidArgument,
                                "Invalid subscription pattern",
                            )),
                        })
                        .collect::<Result<Vec<_>, _>>();
                    match (patterns, self.events.clone()) {
                        (Ok(patterns), Some(store)) => {
                            let db = self.db;
                            match event_work(slots.clone(), config, move || {
                                store
                                    .subscribe(db, patterns, name.as_deref())
                                    .map(|token| Consumer { token, store })
                            })
                            .await
                            {
                                Ok(sub) => {
                                    self.subscription = Some(Arc::new(sub));
                                    Ok(Value::Bool(true))
                                }
                                Err(e) => Err(e),
                            }
                        }
                        (Err(e), _) => Err(e),
                        _ => Err(DbError::new(ErrorCode::EventsDisabled, "Events disabled")),
                    }
                } else {
                    invalid()
                }
            }
            "UNSUBSCRIBE" if args.is_empty() => {
                if let Some(sub) = self.subscription.take() {
                    drop(sub);
                    Ok(Value::Bool(true))
                } else {
                    Err(DbError::new(
                        ErrorCode::ConnectionState,
                        "No subscription context",
                    ))
                }
            }
            "PING" => match args {
                [] => Ok(Value::String("PONG".into())),
                [Value::String(v)] => Ok(Value::String(v.clone())),
                _ => invalid(),
            },
            "AUTH" => match args {
                [Value::String(password)] => {
                    // 重新认证失败后必须撤销已有认证状态。
                    // Failed reauthentication must revoke any prior authenticated state.
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
                    if self.db != *db as u16 {
                        self.scan = linger_engine::ScanState::default();
                    }
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
            _ => {
                let permit = if config.server.request_timeout_ms == 0 {
                    slots.acquire_owned().await.ok()
                } else {
                    timeout(
                        Duration::from_millis(config.server.request_timeout_ms),
                        slots.acquire_owned(),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                };
                let Some(permit) = permit else {
                    return (
                        Err(DbError::new(
                            ErrorCode::Busy,
                            "Execution queue deadline exceeded before starting",
                        )),
                        false,
                        false,
                    );
                };
                let engine = engine.clone();
                let db = self.db;
                let cap = config.server.max_response_bytes;
                let mut scan = std::mem::take(&mut self.scan);
                // 任务一旦开始就不以超时宣称回滚；客户端失去响应时结果可能未知。
                // Once started, timeout cannot claim rollback; losing a response may leave the client outcome unknown.
                match tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let result = if request.command == "SCAN" {
                        engine.scan(db, &request.arguments, &mut scan, cap)
                    } else {
                        engine.execute(db, &request.command, &request.arguments, cap)
                    };
                    (result, scan)
                })
                .await
                {
                    Ok((result, scan)) => {
                        self.scan = scan;
                        result
                    }
                    Err(_) => {
                        close = true;
                        Err(DbError::new(
                            ErrorCode::CommitUnknown,
                            "Engine task failed; request outcome unknown",
                        ))
                    }
                }
            }
        };
        (result, close, fatal)
    }
}

/// 分别限制帧间空闲和半帧接收，取消后连接不再复用。
/// Separately limits inter-frame idle time and partial reads; cancelled streams are never reused.
async fn receive(stream: &mut OwnedReadHalf, config: &Config) -> Result<Option<Frame>, ReadError> {
    let idle = config.server.idle_timeout_ms;
    // 只预读首字节，用于区分空闲时间与半帧接收期限。
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

/// 完整编码后按有界块写出，所有块共用一个截止时间。
/// Encodes fully then writes bounded chunks sharing one deadline.
async fn send(
    stream: &mut (impl AsyncWrite + Unpin),
    frame: Frame,
    config: &Config,
) -> Result<(), ()> {
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

/// 执行调度的边界测试，不依赖制造昂贵命令或不稳定的网络竞争。
/// Scheduling boundary tests without expensive commands or timing-sensitive network contention.
#[cfg(test)]
mod tests {
    use super::*;

    /// 排队超时必须明确尚未执行；许可释放后同一会话可以继续请求。
    /// Queue timeout must precede execution; the same session works after a permit becomes available.
    #[tokio::test]
    async fn saturated_queue_rejects_before_engine_mutation() {
        let mut config = Config::default();
        config.engine.shard_count = 2;
        config.server.request_timeout_ms = 1;
        let engine =
            Arc::new(linger_engine::Engine::new(config.engine_config().unwrap(), 1).unwrap());
        let slots = Arc::new(Semaphore::new(1));
        let occupied = slots.clone().acquire_owned().await.unwrap();
        let mut session = Session {
            hello: true,
            authenticated: true,
            db: 0,
            last_id: 1,
            scan: linger_engine::ScanState::default(),
            subscription: None,
            events: None,
        };
        let request = Request::new("SET", vec![Value::String("queued".into()), Value::Null]);
        let (result, close, fatal) = session
            .execute(request.clone(), &config, &engine, slots.clone())
            .await;
        assert_eq!(result.unwrap_err().code, "ERR_BUSY");
        assert!(!close && !fatal);
        assert_eq!(
            engine.execute(0, "DBSIZE", &[], 1024).unwrap(),
            Value::UInt(0)
        );
        drop(occupied);
        let (result, close, fatal) = session.execute(request, &config, &engine, slots).await;
        assert_eq!(result.unwrap(), Value::Bool(true));
        assert!(!close && !fatal);
        assert_eq!(
            engine
                .execute(0, "GET", &[Value::String("queued".into())], 1024)
                .unwrap(),
            Value::Null
        );
    }
}
