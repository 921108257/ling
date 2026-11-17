//! 消耗普通连接建立专用订阅；不自动 ACK、重连或重放。
//! Consumes an ordinary connection into a dedicated subscription without automatic ACK, reconnect, or replay.

use super::*;
use std::collections::VecDeque;
use tokio::{io::WriteHalf, sync::mpsc, task::JoinSet};

/// 有界读任务保留半帧状态；析构中止任务并关闭读端。
/// A bounded reader preserves partial frames and aborts with the read half on drop.
struct Connection {
    /// 独占写端；Exclusive writer.
    writer: WriteHalf<ling_network::Stream>,
    /// 最多预读一帧；At most one prefetched frame.
    incoming: mpsc::Receiver<Result<Frame, Error>>,
    /// Drop 自动取消读取；Drop automatically cancels reading.
    _reader: JoinSet<()>,
}
/// 专用订阅连接；取消等待将关闭连接，命名组可由调用方显式重新连接。
/// Dedicated subscription; cancelling a wait closes the connection and named groups may reconnect explicitly.
pub struct Subscriber {
    /// 请求取消时移出并释放；Taken and released on request cancellation.
    connection: Option<Connection>,
    /// 已验证的连接预算；Validated connection budgets.
    options: Options,
    /// ACK 和控制命令共用递增请求编号；ACKs and controls share increasing request IDs.
    last_id: u64,
    /// 固定数据库；Fixed database.
    db: u16,
    /// HELLO 确认的流身份；Stream identity confirmed by HELLO.
    stream_id: u128,
    /// ACK 响应之前到达的有界事件；Bounded events arriving before ACK responses.
    queued: VecDeque<Event>,
    /// 队列保守计费；Conservative queue charge.
    bytes: usize,
}
impl Client {
    /// 从当前数据库尾部订阅；命名组沿用已持久化进度。
    /// Subscribes at the current database tail; named groups resume persisted progress.
    pub async fn subscribe(
        mut self,
        patterns: Vec<String>,
        group: Option<&str>,
    ) -> Result<Subscriber, Error> {
        let db = self
            .database
            .ok_or_else(|| Error::local(LocalError::Protocol, false))?;
        let Some(Value::Bytes(stream)) = self.hello.get("stream_id") else {
            return Err(Error::Server(DbError::new(
                ErrorCode::EventsDisabled,
                "Events disabled",
            )));
        };
        let stream_id = u128::from_be_bytes(
            stream
                .as_slice()
                .try_into()
                .map_err(|_| Error::local(LocalError::Protocol, false))?,
        );
        let mut args = vec![Value::List(
            patterns.into_iter().map(Value::String).collect(),
        )];
        if let Some(group) = group {
            args.extend([Value::String("GROUP".into()), Value::String(group.into())]);
        }
        if self.execute("SUBSCRIBE", args).await? != Value::Bool(true) {
            return Err(Error::local(LocalError::Protocol, false));
        }
        let stream = self
            .stream
            .take()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        let (mut reader, writer) = tokio::io::split(stream);
        let (sender, incoming) = mpsc::channel(1);
        let limits = self.options.limits;
        let mut task = JoinSet::new();
        task.spawn(async move {
            loop {
                let frame = match read_frame(&mut reader, limits).await {
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) | Err(ReadError::Io(_)) => {
                        Err(Error::local(LocalError::Disconnected, false))
                    }
                    Err(ReadError::Decode(_)) => Err(Error::local(LocalError::Protocol, false)),
                };
                let done = frame.is_err();
                if sender.send(frame).await.is_err() || done {
                    break;
                }
            }
        });
        Ok(Subscriber {
            connection: Some(Connection {
                writer,
                incoming,
                _reader: task,
            }),
            options: self.options,
            last_id: self.last_id,
            db,
            stream_id,
            queued: VecDeque::new(),
            bytes: 0,
        })
    }
}
impl Subscriber {
    /// 固定数据库，不提供 SELECT；Fixed database without SELECT.
    pub fn database(&self) -> u16 {
        self.db
    }
    /// 关闭或取消后不自动重连；No automatic reconnect after close or cancellation.
    pub fn is_closed(&self) -> bool {
        self.connection.is_none()
    }
    /// 读取下一事件，空闲时发送心跳；业务处理成功后由调用方 ACK。
    /// Reads the next event, sending idle heartbeats; callers ACK after successful business processing.
    pub async fn next_event(&mut self) -> Result<Event, Error> {
        if self.is_closed() {
            return Err(Error::local(LocalError::Closed, false));
        }
        if let Some(event) = self.queued.pop_front() {
            self.bytes -= charge(&event);
            return Ok(event);
        }
        let mut connection = self
            .connection
            .take()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        let heartbeat = self.options.request_timeout.min(Duration::from_secs(30)) / 2;
        let mut tick = tokio::time::interval(heartbeat.max(Duration::from_millis(1)));
        loop {
            tokio::select! {
                frame=connection.incoming.recv()=>{
                    let frame=frame.ok_or_else(||Error::local(LocalError::Disconnected,false))??;
                    match frame.body {
                        Body::Event(value) if frame.request_id==0=>{
                            let event=self.validate(&value)?;
                            self.connection=Some(connection);return Ok(event);
                        }
                        Body::Heartbeat=>{}
                        Body::Response(_) | Body::BatchResponse(_)
                            if frame.request_id != 0 && frame.request_id <= self.last_id=>{}
                        Body::Error(e) if frame.request_id <= self.last_id=>return Err(Error::Server(e)),
                        _=>return Err(Error::local(LocalError::Protocol,false))
                    }
                }
                _=tick.tick()=>{
                    let bytes=encode_frame(&Frame{request_id:0,body:Body::Heartbeat},self.options.limits)
                        .map_err(|_|Error::local(LocalError::InvalidRequest,false))?;
                    timeout(self.options.request_timeout,connection.writer.write_all(&bytes)).await
                        .map_err(|_|Error::local(LocalError::Timeout,false))?
                        .map_err(|_|Error::local(LocalError::Disconnected,false))?;
                }
            }
        }
    }
    /// 显式确认，发送后超时保留提交未知且不自动重放。
    /// Explicit acknowledgement; timeouts after sending remain unknown and are never replayed.
    pub async fn ack(&mut self, id: EventId) -> Result<(), Error> {
        self.exchange(Body::Ack(id.to_value())).await
    }
    /// 专用连接请求响应探活，保持 ACK 之外的编号连续。
    /// Probes the dedicated connection while preserving request numbering shared with ACKs.
    pub async fn ping(&mut self) -> Result<(), Error> {
        self.exchange(Body::Request(Request::new("PING", vec![])))
            .await
    }
    /// 停止订阅并释放连接；不删除命名组。
    /// Stops subscription and releases the connection without deleting the named group.
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.is_closed() {
            self.queued.clear();
            self.bytes = 0;
            return Ok(());
        }
        let result = self
            .exchange(Body::Request(Request::new("UNSUBSCRIBE", vec![])))
            .await;
        self.connection.take();
        self.queued.clear();
        self.bytes = 0;
        result
    }
    /// ACK 响应前的事件入有界队列；绝不隐式丢弃或确认。
    /// Events preceding an ACK response enter a bounded queue and are never implicitly dropped or acknowledged.
    async fn exchange(&mut self, body: Body) -> Result<(), Error> {
        let Some(id) = self.last_id.checked_add(1) else {
            self.connection.take();
            return Err(Error::local(LocalError::Closed, false));
        };
        let ping = matches!(&body,Body::Request(r) if r.command=="PING");
        let bytes = encode_frame(
            &Frame {
                request_id: id,
                body,
            },
            self.options.limits,
        )
        .map_err(|_| Error::local(LocalError::InvalidRequest, false))?;
        let mut connection = self
            .connection
            .take()
            .ok_or_else(|| Error::local(LocalError::Closed, false))?;
        self.last_id = id;
        let request_timeout = self.options.request_timeout;
        let exchange = async {
            connection
                .writer
                .write_all(&bytes)
                .await
                .map_err(|_| Error::local(LocalError::Disconnected, true))?;
            loop {
                let frame = connection
                    .incoming
                    .recv()
                    .await
                    .ok_or_else(|| Error::local(LocalError::Disconnected, true))?
                    .map_err(|e| match e {
                        Error::Local { kind, .. } => Error::local(kind, true),
                        e => e,
                    })?;
                match frame.body {
                    Body::Event(value) if frame.request_id == 0 => {
                        let event = self
                            .validate(&value)
                            .map_err(|_| Error::local(LocalError::Protocol, true))?;
                        let cost = charge(&event);
                        if self.bytes.saturating_add(cost)
                            > self.options.limits.max_decode_bytes / 2
                            || self.queued.len() >= 1024
                        {
                            return Err(Error::local(LocalError::BufferFull, true));
                        }
                        self.bytes += cost;
                        self.queued.push_back(event);
                    }
                    Body::Heartbeat => {}
                    Body::Response(_) | Body::BatchResponse(_)
                        if frame.request_id != 0 && frame.request_id < id => {}
                    Body::Response(result) if frame.request_id == id => return Ok(result),
                    Body::Error(e) if frame.request_id <= id => return Err(Error::Server(e)),
                    _ => return Err(Error::local(LocalError::Protocol, true)),
                }
            }
        };
        let result = timeout(request_timeout, exchange)
            .await
            .map_err(|_| Error::local(LocalError::Timeout, true))??;
        self.connection = Some(connection);
        match result {
            Ok(Value::Bool(true)) if !ping => Ok(()),
            Ok(Value::String(s)) if ping && s == "PONG" => Ok(()),
            Err(e) => Err(Error::Server(e)),
            _ => {
                self.connection.take();
                Err(Error::local(LocalError::Protocol, false))
            }
        }
    }
    /// 验证流身份与固定数据库；Validates stream identity and the fixed database.
    fn validate(&self, value: &Value) -> Result<Event, Error> {
        let event =
            Event::from_value(value).map_err(|_| Error::local(LocalError::Protocol, false))?;
        if event.id.db != self.db || event.id.stream_id != self.stream_id {
            return Err(Error::local(LocalError::Protocol, false));
        }
        Ok(event)
    }
}
/// 包括容器与字符串的保守额度；Conservative allowance for containers and strings.
fn charge(e: &Event) -> usize {
    512 + e.key.len() * 2
}
