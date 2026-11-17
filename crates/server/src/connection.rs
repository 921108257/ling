//! 单连接状态机、认证和选库屏障；网络等待不持有数据引擎锁。
//! Per-connection state, authentication, and database barriers; network waits never hold the engine lock.

use crate::Config;
use ling_protocol::{Body, Frame, ReadError, Request, encode_frame, read_frame};
use ling_types::{DbError, ErrorCode, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Semaphore, watch},
    time::{Instant, timeout, timeout_at},
};

/// 按接收顺序处理请求，致命帧错误或关闭信号结束连接。
/// Processes requests in receive order and closes on fatal framing errors or shutdown.
pub(crate) async fn serve(
    stream: ling_network::Stream,
    config: &Config,
    engine: &Arc<ling_engine::Engine>,
    slots: Arc<Semaphore>,
    mut shutdown: watch::Receiver<bool>,
    runtime: Arc<crate::operations::Runtime>,
) {
    runtime
        .clients
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _connection = ConnectionCount(runtime.clone());
    // 测试专用故障点，只存在于单元测试构建；发行构建不含。
    // Test-only failpoint present only in unit-test builds and absent from release builds.
    #[cfg(test)]
    {
        if crate::failure_policy_tests::PANIC_NEXT_CONNECTION
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("failpoint: connection task panic");
        }
    }
    let (mut reader, mut stream) = tokio::io::split(stream);
    let (frames, mut incoming) = tokio::sync::mpsc::channel(1);
    let read_config = config.clone();
    let mut reading = tokio::task::JoinSet::new();
    // 独立有界读任务保存半帧状态；事件推送不会取消正在进行的帧读取。
    // A bounded reader preserves partial-frame state; event pushes never cancel frame reads.
    reading.spawn(async move {
        let mut first_frame = true;
        loop {
            let frame = receive(&mut reader, &read_config, first_frame).await;
            first_frame = false;
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
        auth_failures: 0,
        db: config.database.default_db,
        last_id: 0,
        scan: ling_engine::ScanState::default(),
        subscription: None,
        events: engine.events().cloned(),
        runtime: Some(runtime.clone()),
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
        let started = std::time::Instant::now();
        let shutdown_requested = matches!(&frame.body, Body::Request(r) if r.command == "SHUTDOWN" && r.arguments.is_empty() && session.authenticated && session.subscription.is_none());
        if let Body::BatchRequest(items) = &frame.body {
            let body = if !session.authenticated {
                Body::Response(Err(DbError::new(
                    ErrorCode::Unauthenticated,
                    "Authentication required",
                )))
            } else if session.subscription.is_some() {
                Body::Response(Err(DbError::new(
                    ErrorCode::ConnectionState,
                    "Dedicated subscription connection",
                )))
            } else {
                let items = items.clone();
                let engine = engine.clone();
                let config = config.clone();
                let db = session.db;
                match event_work(slots.clone(), &config.clone(), move || {
                    crate::batch::execute(&engine, &config, db, items)
                })
                .await
                {
                    Ok(results) => Body::BatchResponse(results),
                    Err(error) => Body::Response(Err(error)),
                }
            };
            runtime.record(started, matches!(&body, Body::Response(Err(_))));
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
            {
                break;
            }
            continue;
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
                    ling_types::EventId::from_value(&value),
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
        let accepted_shutdown = shutdown_requested && result == Ok(Value::Bool(true));
        runtime.record(started, result.is_err());
        let body = if fatal_error {
            Body::Error(result.unwrap_err())
        } else {
            Body::Response(result)
        };
        let sent = send(
            &mut stream,
            Frame {
                request_id: frame.request_id,
                body,
            },
            config,
        )
        .await
        .is_err();
        if accepted_shutdown {
            runtime.stop.send_replace(true);
        }
        if sent || close {
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
    /// 运行控制面；Runtime control plane.
    runtime: Option<Arc<crate::operations::Runtime>>,
    /// 是否完成唯一一次 HELLO。
    /// Whether the sole HELLO handshake completed.
    hello: bool,
    /// 当前认证状态；失败重认证会清除。
    /// Current authentication state, revoked by failed reauthentication.
    authenticated: bool,
    /// 连续错误口令次数，成功认证重置；Consecutive wrong passwords, reset by successful authentication.
    auth_failures: u8,
    /// 当前连接选择的数据库。
    /// Database selected by this connection.
    db: u16,
    /// 最后接受的请求编号，避免重用。
    /// Last accepted request number, preventing reuse.
    last_id: u64,
    /// 当前连接唯一游标，断线即释放；Connection-owned cursor, released on disconnect.
    scan: ling_engine::ScanState,
    /// 专用连接拥有的消费身份；Consumption identity owned by this dedicated connection.
    subscription: Option<Arc<Consumer>>,
    /// 事件句柄保证取消时释放租约；Event handle releases leases on cancellation.
    events: Option<Arc<ling_events::EventStore>>,
}
/// 订阅结果自带释放责任，包括阻塞任务完成前连接已取消的情况。
/// Subscription results own cleanup, including connection cancellation before blocking tasks complete.
struct Consumer {
    /// 消费令牌；Consumption token.
    token: ling_events::Subscription,
    /// 保证释放时存储仍存活；Keeps storage alive until cleanup.
    store: Arc<ling_events::EventStore>,
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
        engine: &Arc<ling_engine::Engine>,
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
            "INFO" | "CONFIG" => {
                let Some(runtime) = self.runtime.clone() else {
                    return (invalid(), false, false);
                };
                let engine = engine.clone();
                let args = args.to_vec();
                let info = name == "INFO";
                event_work(slots.clone(), config, move || {
                    if info {
                        runtime.info(&engine, &args)
                    } else {
                        runtime.configure(&args)
                    }
                })
                .await
            }
            "SHUTDOWN" if args.is_empty() => Ok(Value::Bool(true)),
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
                    self.authenticated =
                        !config.auth.enabled || password_matches(password, &config.auth.password);
                    if self.authenticated {
                        self.auth_failures = 0;
                        Ok(Value::Bool(true))
                    } else {
                        self.auth_failures = self.auth_failures.saturating_add(1);
                        close = self.auth_failures >= MAX_AUTH_FAILURES;
                        if let Some(runtime) = &self.runtime {
                            runtime.auth_failed();
                        }
                        // 等待不持执行许可；关闭只影响当前连接，失败响应字节保持不变。
                        // Waiting holds no execution permit; only this peer closes, with unchanged failure bytes.
                        tokio::time::sleep(auth_failure_delay(self.auth_failures, config)).await;
                        Err(DbError::new(ErrorCode::AuthFailed, "Authentication failed"))
                    }
                }
                _ => invalid(),
            },
            "SELECT" => match args {
                [Value::UInt(db)] if *db < u64::from(config.database.databases) => {
                    if self.db != *db as u16 {
                        self.scan = ling_engine::ScanState::default();
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

/// 连接累计五次失败后关闭，避免持有无限次猜测的会话。
/// Closes a connection after five consecutive failures to bound guesses per session.
const MAX_AUTH_FAILURES: u8 = 5;

/// 固定长度摘要交给成熟库比较，不对口令本身做提前终止的比较。
/// Compares fixed-length digests through an established library without early-exit password comparison.
fn password_matches(password: &str, expected: &str) -> bool {
    let supplied = Sha256::digest(password.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    bool::from(supplied.ct_eq(&expected))
}

/// 100 ms 起、翻倍至 1600 ms；启用的最小请求/空闲期限再限制为其一半。
/// Starts at 100 ms, doubles up to 1600 ms, and is capped at half the shortest enabled request/idle timeout.
fn auth_failure_delay(failures: u8, config: &Config) -> Duration {
    let delay = Duration::from_millis(100 << failures.saturating_sub(1).min(4));
    [
        config.server.request_timeout_ms,
        config.server.idle_timeout_ms,
    ]
    .into_iter()
    .filter(|millis| *millis != 0)
    .map(|millis| Duration::from_millis(millis) / 2)
    .fold(delay, Duration::min)
}

/// 首帧共用请求期限；后续分别限制帧间空闲和半帧，取消后不复用。
/// The first frame shares a request deadline; later idle and partial reads have separate limits, without reuse after cancellation.
async fn receive(
    stream: &mut (impl AsyncRead + Unpin),
    config: &Config,
    first_frame: bool,
) -> Result<Option<Frame>, ReadError> {
    if first_frame {
        // 首字节前就开始计时，慢速发送头部也不能延长同一个期限。
        // Start before the first byte so a slowly sent header cannot extend the deadline.
        let millis = if config.server.request_timeout_ms != 0 {
            config.server.request_timeout_ms
        } else {
            config.server.idle_timeout_ms
        };
        let read = read_frame(stream, config.limits());
        return if millis == 0 {
            read.await
        } else {
            timeout(Duration::from_millis(millis), read)
                .await
                .unwrap_or(Ok(None))
        };
    }
    let mut first = [0];
    let start = async { stream.read(&mut first).await };
    let count = if config.server.idle_timeout_ms == 0 {
        start.await
    } else {
        match timeout(Duration::from_millis(config.server.idle_timeout_ms), start).await {
            Ok(result) => result,
            Err(_) => return Ok(None),
        }
    }
    .map_err(ReadError::Io)?;
    if count == 0 {
        return Ok(None);
    }
    // 首字节回接至同一解码器，TCP/TLS 均不依赖 peek。
    // Chain the first byte into the same decoder; neither TCP nor TLS depends on peek.
    let mut complete = std::io::Cursor::new(first).chain(stream);
    let read = read_frame(&mut complete, config.limits());
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

/// 连接计数与任务生命周期一致，即使取消也会归还。
/// Connection counts follow task lifetime and return even on cancellation.
struct ConnectionCount(Arc<crate::operations::Runtime>);
impl Drop for ConnectionCount {
    /// 中止任务也更新连接数；Aborted tasks also update connection counts.
    fn drop(&mut self) {
        self.0
            .clients
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 执行调度的边界测试，不依赖制造昂贵命令或不稳定的网络竞争。
/// Scheduling boundary tests without expensive commands or timing-sensitive network contention.
#[cfg(test)]
mod tests {
    use super::*;

    /// 用虚拟时钟核对首帧请求期限、禁用后的 idle 回退及 HELLO 后 idle。
    /// A virtual clock checks the first-frame request deadline, disabled-request idle fallback, and post-HELLO idle.
    #[tokio::test(start_paused = true)]
    async fn first_frame_deadline_and_idle_fallback() {
        for (first, request_ms, idle_ms, expected_ms) in [
            (true, 100, 10, 100),
            (true, 0, 20, 20),
            (false, 100, 20, 20),
        ] {
            let mut config = Config::default();
            config.server.request_timeout_ms = request_ms;
            config.server.idle_timeout_ms = idle_ms;
            let (_writer, mut reader) = tokio::io::duplex(32);
            let start = Instant::now();
            assert!(
                receive(&mut reader, &config, first)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(start.elapsed(), Duration::from_millis(expected_ms));
        }
        for (request_ms, idle_ms, expected_ms) in [(100, 10, 100), (0, 20, 20)] {
            let mut config = Config::default();
            config.server.request_timeout_ms = request_ms;
            config.server.idle_timeout_ms = idle_ms;
            let (mut writer, mut reader) = tokio::io::duplex(32);
            writer.write_all(b"L").await.unwrap();
            let start = Instant::now();
            assert!(receive(&mut reader, &config, true).await.unwrap().is_none());
            assert_eq!(start.elapsed(), Duration::from_millis(expected_ms));
        }
    }

    /// 固定长度摘要覆盖相同、不同长度、空值以及 Unicode 口令。
    /// Fixed-length digest comparison covers equality, different lengths, empty values, and Unicode passwords.
    #[test]
    fn password_comparison_boundaries() {
        for (left, right, expected) in [
            ("secret", "secret", true),
            ("secret", "secreu", false),
            ("secret", "secret-long", false),
            ("", "secret", false),
            ("", "", true),
            ("密钥", "密钥", true),
            ("密钥", "密码", false),
        ] {
            assert_eq!(password_matches(left, right), expected);
        }
    }

    /// 延迟递增但严格低于启用的超时；用纯函数检验，不真实等待。
    /// Delays increase but stay below enabled timeouts, tested without wall-clock sleeps.
    #[test]
    fn authentication_delay_is_bounded() {
        let mut config = Config::default();
        for (failures, millis) in [(1, 100), (2, 200), (3, 400), (4, 800), (5, 1600)] {
            assert_eq!(
                auth_failure_delay(failures, &config),
                Duration::from_millis(millis)
            );
        }
        config.server.request_timeout_ms = 1;
        assert_eq!(auth_failure_delay(5, &config), Duration::from_micros(500));
        config.server.request_timeout_ms = 0;
        config.server.idle_timeout_ms = 20;
        assert_eq!(auth_failure_delay(5, &config), Duration::from_millis(10));
        config.server.idle_timeout_ms = 0;
        assert_eq!(auth_failure_delay(5, &config), Duration::from_millis(1600));
    }

    /// 测试会话不接入事件，避免把认证测试绑定到消费存储。
    /// Test sessions omit event state so authentication tests stay independent of consumer storage.
    fn session(config: &Config) -> Session {
        Session {
            hello: true,
            authenticated: !config.auth.enabled,
            auth_failures: 0,
            db: 0,
            last_id: 1,
            scan: ling_engine::ScanState::default(),
            subscription: None,
            events: None,
            runtime: None,
        }
    }

    /// 虚拟时钟证明延迟期间不取得执行许可，其他会话的 PING 可以先完成。
    /// A virtual clock proves delayed authentication holds no execution permit and another session can PING first.
    #[tokio::test(start_paused = true)]
    async fn authentication_delay_isolated_and_success_resets_failures() {
        let mut config = Config::default();
        config.auth.enabled = true;
        config.auth.password = "secret".into();
        let engine =
            Arc::new(ling_engine::Engine::new(config.engine_config().unwrap(), 1).unwrap());
        let slots = Arc::new(Semaphore::new(0));
        let mut bad = session(&config);
        bad.authenticated = true;
        let mut other = session(&config);
        let started = Instant::now();
        {
            let delayed = bad.execute(
                Request::new("AUTH", vec![Value::String("wrong".into())]),
                &config,
                &engine,
                slots.clone(),
            );
            tokio::pin!(delayed);
            tokio::select! {
                biased;
                _ = &mut delayed => panic!("Authentication failure was not delayed"),
                (result, _, _) = other.execute(Request::new("PING", vec![]), &config, &engine, slots.clone()) => {
                    assert_eq!(result.unwrap(), Value::String("PONG".into()));
                    assert_eq!(Instant::now(), started);
                }
            }
            let (result, close, fatal) = delayed.await;
            assert_eq!(result.unwrap_err().code, ErrorCode::AuthFailed.as_str());
            assert!(!close && !fatal);
        }
        assert!(!bad.authenticated);
        assert_eq!(bad.auth_failures, 1);
        let (result, close, fatal) = bad
            .execute(
                Request::new("AUTH", vec![Value::String("secret".into())]),
                &config,
                &engine,
                slots,
            )
            .await;
        assert_eq!(result.unwrap(), Value::Bool(true));
        assert!(!close && !fatal);
        assert!(bad.authenticated);
        assert_eq!(bad.auth_failures, 0);
    }

    /// 排队超时必须明确尚未执行；许可释放后同一会话可以继续请求。
    /// Queue timeout must precede execution; the same session works after a permit becomes available.
    #[tokio::test]
    async fn saturated_queue_rejects_before_engine_mutation() {
        let mut config = Config::default();
        config.engine.shard_count = 2;
        config.server.request_timeout_ms = 1;
        let engine =
            Arc::new(ling_engine::Engine::new(config.engine_config().unwrap(), 1).unwrap());
        let slots = Arc::new(Semaphore::new(1));
        let occupied = slots.clone().acquire_owned().await.unwrap();
        let mut session = Session {
            hello: true,
            authenticated: true,
            auth_failures: 0,
            db: 0,
            last_id: 1,
            scan: ling_engine::ScanState::default(),
            subscription: None,
            events: None,
            runtime: None,
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
