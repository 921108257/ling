package io.linger.cache;

import java.io.*;
import java.math.BigInteger;
import java.net.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.locks.ReentrantLock;
import javax.net.ssl.*;

/** 原生独占 TCP/TLS 客户端，不自动重放请求。 Native exclusive TCP/TLS client without request replay. */
public final class Client implements AutoCloseable {
  /**
   * 固定初始化设置，超时单位毫秒，密码不进入诊断。 Fixed initialization with millisecond deadlines; credentials stay out
   * of diagnostics.
   */
  public static final class Options {
    /** 目标地址。 Target address. */
    public final String host;

    /** 可选认证，不进入诊断。 Optional auth, never printed. */
    public final String password;

    /** 必须与证书匹配的名称。 Name that must match the certificate. */
    public final String serverName;

    /** 目标非零端口。 Nonzero target port. */
    public final int port;

    /** 初始化数据库。 Initial database. */
    public final int database;

    /** 建连毫秒期限。 Connection deadline in milliseconds. */
    public final int connectTimeoutMs;

    /** 请求毫秒期限。 Request deadline in milliseconds. */
    public final int requestTimeoutMs;

    /** 使用 JSSE 的验证式上下文。 Verified JSSE context. */
    public final SSLContext tls;

    /** TLS 必须提供要验证的名称；null TLS 表示明文。 TLS requires a verified name; null TLS means plaintext. */
    public Options(
        String host,
        int port,
        int database,
        String password,
        int connectTimeoutMs,
        int requestTimeoutMs,
        SSLContext tls,
        String serverName) {
      if (port < 1
          || port > 65535
          || database < 0
          || database > 65535
          || connectTimeoutMs < 1
          || requestTimeoutMs < 1
          || (tls != null && (serverName == null || serverName.isEmpty())))
        throw new IllegalArgumentException("Invalid client configuration");
      this.host = host;
      this.port = port;
      this.database = database;
      this.password = password;
      this.connectTimeoutMs = connectTimeoutMs;
      this.requestTimeoutMs = requestTimeoutMs;
      this.tls = tls;
      this.serverName = serverName;
    }

    /** 默认 TCP 连接数据库零。 Defaults to TCP and database zero. */
    public Options(String host, int port) {
      this(host, port, 0, null, 5000, 30000, null, null);
    }

    /** 隐藏身份凭据。 Hides authentication credentials. */
    @Override
    public String toString() {
      return "Linger options (credentials hidden)";
    }
  }

  /**
   * 服务端代码与客户端失败分开，显式保留提交未知。 Separates server and client failures with explicit unknown outcomes.
   */
  public static final class Failure extends IOException {
    private static final long serialVersionUID = 1L;

    /** 稳定服务端码或客户端分类。 Stable server code or client category. */
    public final String code;

    /** 发送后无法确定是否提交。 Cannot confirm commit after sending. */
    public final boolean outcomeUnknown;

    /** 不携带参数与原始异常内容。 Contains neither arguments nor raw exception contents. */
    public Failure(String code, boolean unknown) {
      super(code + (unknown ? "; outcome unknown" : ""));
      this.code = code;
      this.outcomeUnknown = unknown || code.equals("ERR_COMMIT_UNKNOWN");
    }
  }

  /** 普通 Batch 子项。 Ordinary Batch item. */
  public record Request(String command, List<Value> arguments) {
    /** 入参列表不可变。 Argument list is immutable. */
    public Request {
      arguments = List.copyOf(arguments);
    }
  }

  /** 成功 Value 与错误分别表达，Null 仍是成功。 Result separates errors from Values; Null remains success. */
  public record Result(Value value, Failure error) {}

  private record Received(Codec.Frame frame, IOException error) {}

  private static final ScheduledThreadPoolExecutor TIMER = timer();
  private final Socket socket;
  private final Options options;
  private final ReentrantLock lock = new ReentrantLock();
  private final ArrayBlockingQueue<Received> incoming = new ArrayBlockingQueue<>(1);
  private final ArrayDeque<Value> queued = new ArrayDeque<>();
  private final Thread reader;
  private volatile boolean closed;
  private boolean subscribed;
  private long last;
  private byte[] stream;
  private int queuedBytes;
  private int database;

  private Client(Socket socket, Options options) {
    this.socket = socket;
    this.options = options;
    this.database = options.database;
    reader = new Thread(this::readLoop, "linger-reader");
    reader.setDaemon(true);
    reader.start();
  }

  /**
   * 依次完成 TLS、HELLO、AUTH、SELECT；任何失败都关闭套接字。 Completes TLS/HELLO/AUTH/SELECT in order and closes on
   * failure.
   */
  public static Client connect(Options options) throws IOException {
    return connect(options, socket -> {});
  }

  /**
   * 池在网络等待前登记套接字，关闭可及时取消建连。 Pools register sockets before network waits so close can cancel
   * connects.
   */
  @FunctionalInterface
  interface Register {
    void socket(Socket socket) throws IOException;
  }

  static Client connect(Options options, Register register) throws IOException {
    Socket socket = new Socket();
    Client client = null;
    try {
      register.socket(socket);
      socket.connect(new InetSocketAddress(options.host, options.port), options.connectTimeoutMs);
      socket.setTcpNoDelay(true);
      if (options.tls != null) {
        SSLSocket secure =
            (SSLSocket)
                options
                    .tls
                    .getSocketFactory()
                    .createSocket(socket, options.serverName, options.port, true);
        SSLParameters parameters = secure.getSSLParameters();
        parameters.setEndpointIdentificationAlgorithm("HTTPS");
        parameters.setProtocols(new String[] {"TLSv1.3", "TLSv1.2"});
        secure.setSSLParameters(parameters);
        secure.setSoTimeout(options.connectTimeoutMs);
        secure.startHandshake();
        secure.setSoTimeout(0);
        socket = secure;
      }
      client = new Client(socket, options);
      Value hello = client.execute("HELLO", Value.uint(1));
      if (hello.kind() != Value.Kind.MAP) throw Codec.protocol();
      Map<String, Value> m = hello.fields();
      if (!Value.uint(1).equals(m.get("protocol"))
          || !has(m, "server_version", Value.Kind.STRING)
          || !has(m, "events", Value.Kind.BOOL)
          || !has(m, "reliable_events", Value.Kind.BOOL)) throw Codec.protocol();
      if (Boolean.TRUE.equals(m.get("events").data())) {
        if (!has(m, "stream_id", Value.Kind.BYTES)
            || ((byte[]) m.get("stream_id").data()).length != 16) throw Codec.protocol();
        client.stream = (byte[]) m.get("stream_id").data();
      } else if (!Value.nil().equals(m.get("stream_id"))
          || Boolean.TRUE.equals(m.get("reliable_events").data())) throw Codec.protocol();
      if (options.password != null
          && !client
              .execute("AUTH", Value.string(options.password))
              .equals(new Value(Value.Kind.BOOL, true))) throw Codec.protocol();
      if (!client
          .execute("SELECT", Value.uint(options.database))
          .equals(new Value(Value.Kind.BOOL, true))) throw Codec.protocol();
      return client;
    } catch (IOException | RuntimeException e) {
      if (client != null) client.close();
      else socket.close();
      if (e instanceof Failure failure) throw failure;
      throw new Failure("ERR_CLIENT_CONNECT", false);
    }
  }

  /**
   * 请求串行执行且不复用 ID；超时或中断关闭连接。 Serial requests never reuse IDs; timeout or interruption closes the
   * connection.
   */
  public Value execute(String command, Value... arguments) throws IOException {
    String name = command.toUpperCase(Locale.ROOT);
    byte[] payload = Codec.request(name, List.of(arguments));
    acquire();
    try {
      if (subscribed && !List.of("PING", "QUIT", "UNSUBSCRIBE").contains(name))
        throw new Failure("ERR_CONNECTION_STATE", false);
      List<Result> result = exchange(1, 0, payload, 0);
      Result item = result.get(0);
      if (item.error != null) throw item.error;
      if (name.equals("SUBSCRIBE") && item.value.equals(new Value(Value.Kind.BOOL, true)))
        subscribed = true;
      if (name.equals("UNSUBSCRIBE") && item.value.equals(new Value(Value.Kind.BOOL, true))) {
        subscribed = false;
        queued.clear();
        queuedBytes = 0;
      }
      if (name.equals("SELECT")
          && arguments.length == 1
          && arguments[0].kind() == Value.Kind.UINT
          && item.value.equals(new Value(Value.Kind.BOOL, true)))
        database = ((BigInteger) arguments[0].data()).intValueExact();
      if (name.equals("QUIT")) close();
      return item.value;
    } finally {
      lock.unlock();
    }
  }

  /**
   * 普通批量按输入顺序返回；传输失败可能有部分已执行，绝不重放。 Batch results preserve input order; transport failure may follow
   * partial execution and never triggers replay.
   */
  public List<Result> batch(List<Request> requests) throws IOException {
    if (requests.isEmpty() || requests.size() > 1000)
      throw new Failure("ERR_CLIENT_ARGUMENT", false);
    List<byte[]> encoded = new ArrayList<>();
    long size = 4;
    for (Request request : requests) {
      byte[] part = Codec.request(request.command, request.arguments);
      size += part.length;
      if (size > Codec.MAX_BYTES) throw new Failure("ERR_CLIENT_LIMIT", false);
      encoded.add(part);
    }
    java.nio.ByteBuffer data = java.nio.ByteBuffer.allocate((int) size);
    data.putInt(requests.size());
    for (byte[] part : encoded) data.put(part);
    acquire();
    try {
      if (subscribed) throw new Failure("ERR_CONNECTION_STATE", false);
      return exchange(1, 1, data.array(), requests.size());
    } finally {
      lock.unlock();
    }
  }

  /** 当前流身份的副本，事件禁用时为空。 Copy of the stream identity, or null when events are disabled. */
  public byte[] streamId() {
    return stream == null ? null : stream.clone();
  }

  /**
   * 恢复前验证身份，变化时关闭且不创建替代组。 Checks identity before recovery; changes close without creating a
   * replacement group.
   */
  public void requireEventStream(byte[] expected) throws IOException {
    if (stream == null || !Arrays.equals(expected, stream)) {
      close();
      throw new Failure("ERR_STREAM_MISMATCH", false);
    }
  }

  /** 专用订阅，不与普通命令或池共享连接。 Dedicated subscription never shares an ordinary or pooled connection. */
  public Subscriber subscribe(List<String> patterns, String group) throws IOException {
    List<Value> args = new ArrayList<>();
    args.add(Value.list(patterns.stream().map(Value::string).toList()));
    if (group != null) {
      args.add(Value.string("GROUP"));
      args.add(Value.string(group));
    }
    Value result = execute("SUBSCRIBE", args.toArray(Value[]::new));
    if (!result.equals(new Value(Value.Kind.BOOL, true))) throw Codec.protocol();
    return new Subscriber(this);
  }

  /**
   * 并发安全的幂等关闭，阻塞写入和读任务均被唤醒。 Concurrent idempotent close wakes blocked writes and the reader task.
   */
  @Override
  public void close() {
    closed = true;
    try {
      socket.close();
    } catch (IOException ignored) {
      /* 已关闭语义优先。 Closed-state semantics take priority. */
    }
    reader.interrupt();
    // 直接唤醒等待响应的调用，不能只等待其完整超时。
    // Wake response waiters directly instead of waiting for their full timeout.
    incoming.offer(new Received(null, new IOException("Client closed")));
  }

  /** 关闭状态可用于连接池淘汰。 Closed state allows pools to discard sockets. */
  public boolean isClosed() {
    return closed;
  }

  /** 全局单个有界超时调度线程，取消项立即移除。 One shared timeout scheduler removes cancelled tasks immediately. */
  private static ScheduledThreadPoolExecutor timer() {
    ScheduledThreadPoolExecutor timer =
        new ScheduledThreadPoolExecutor(
            1,
            r -> {
              Thread t = new Thread(r, "linger-timeouts");
              t.setDaemon(true);
              return t;
            });
    timer.setRemoveOnCancelPolicy(true);
    return timer;
  }

  /**
   * 独立读取保存半帧进度，队列满时停止预读。 Independent reading preserves partial frames and stops prefetching when
   * full.
   */
  private void readLoop() {
    try {
      while (!closed) {
        Codec.Frame f = Codec.read(socket.getInputStream());
        incoming.put(new Received(f, null));
      }
    } catch (IOException e) {
      try {
        incoming.put(new Received(null, e));
      } catch (InterruptedException interrupted) {
        Thread.currentThread().interrupt();
      }
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    }
  }

  /**
   * 排队取消尚未发送；取得锁之后由 watchdog 限制完整请求。 Queue cancellation precedes sending; after locking a watchdog
   * bounds the whole exchange.
   */
  private void acquire() throws IOException {
    try {
      if (!lock.tryLock(options.requestTimeoutMs, TimeUnit.MILLISECONDS))
        throw new Failure("ERR_CLIENT_TIMEOUT", false);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new Failure("ERR_CLIENT_CANCELLED", false);
    }
    if (closed) {
      lock.unlock();
      throw new Failure("ERR_CLIENT_CLOSED", false);
    }
  }

  /**
   * 有界等待同时覆盖写出和响应匹配；中断关闭连接。 Bounded wait covers writes and response matching; interruption closes
   * the socket.
   */
  private List<Result> exchange(int kind, int flags, byte[] payload, int batch) throws IOException {
    acquire();
    boolean sent = false;
    ScheduledFuture<?> deadline =
        TIMER.schedule(this::close, options.requestTimeoutMs, TimeUnit.MILLISECONDS);
    try {
      if (last == -1L) throw new Failure("ERR_CLIENT_CLOSED", false);
      last++;
      sent = true;
      Codec.write(socket.getOutputStream(), new Codec.Frame(kind, flags, last, payload));
      long until = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(options.requestTimeoutMs);
      while (true) {
        Received r = incoming.poll(Math.max(1, until - System.nanoTime()), TimeUnit.NANOSECONDS);
        if (r == null || r.error != null) throw new Failure("ERR_CLIENT_DISCONNECTED", true);
        Codec.Frame f = r.frame;
        if (f.kind() == 5) continue;
        if (f.kind() == 3 && subscribed) {
          Value event = Codec.decodeValue(f.payload());
          validateEvent(event);
          if (queued.size() >= 1024
              || (long) queuedBytes + f.payload().length > Codec.MAX_BYTES / 2)
            throw new Failure("ERR_CLIENT_BUFFER_FULL", true);
          queued.add(event);
          queuedBytes += f.payload().length;
          continue;
        }
        if (f.kind() == 2 && f.id() != 0 && Long.compareUnsigned(f.id(), last) < 0) continue;
        if ((f.kind() != 2 && f.kind() != 6) || f.id() != last) throw Codec.protocol();
        Codec.Decoder d = new Codec.Decoder(f.payload());
        if (f.kind() == 6) {
          Failure error = serverError(d);
          close();
          throw error;
        }
        if (f.flags() == 1) {
          int count = d.count();
          if (batch == 0 || count != batch) throw Codec.protocol();
          List<Result> result = new ArrayList<>();
          for (int i = 0; i < count; i++) result.add(response(d));
          d.end();
          if (d.semantic) throw Codec.protocol();
          return result;
        }
        if (f.flags() != 0) throw Codec.protocol();
        Result item = response(d);
        d.end();
        if (d.semantic) throw Codec.protocol();
        if (batch > 0) {
          if (item.error != null) throw item.error;
          throw Codec.protocol();
        }
        return List.of(item);
      }
    } catch (InterruptedException e) {
      close();
      Thread.currentThread().interrupt();
      throw new Failure("ERR_CLIENT_CANCELLED", sent);
    } catch (Codec.DecodeException e) {
      close();
      throw new Failure("ERR_CLIENT_PROTOCOL", sent);
    } catch (IOException e) {
      if (e instanceof Failure failure && !failure.code.startsWith("ERR_CLIENT_")) throw failure;
      close();
      if (e instanceof Failure failure) throw failure;
      throw new Failure("ERR_CLIENT_DISCONNECTED", sent);
    } finally {
      deadline.cancel(false);
      lock.unlock();
    }
  }

  /** 仅按稳定码分支，不回显服务端诊断值。 Branches on stable codes without echoing diagnostic contents. */
  private static Failure serverError(Codec.Decoder d) throws IOException {
    Value code = d.value(0), message = d.value(0);
    if (code.kind() != Value.Kind.STRING || message.kind() != Value.Kind.STRING)
      throw Codec.protocol();
    return new Failure((String) code.data(), false);
  }

  private static Result response(Codec.Decoder d) throws IOException {
    d.need(1);
    int status = Byte.toUnsignedInt(d.data.get());
    if (status == 0) return new Result(d.value(0), null);
    if (status == 1) return new Result(null, serverError(d));
    throw Codec.protocol();
  }

  /**
   * 必需字段严格验证，拒绝宽整数截断与新流替代。 Required fields reject integer truncation and silent stream replacement.
   */
  private void validateEvent(Value event) throws IOException {
    if (event.kind() != Value.Kind.MAP) throw Codec.protocol();
    Map<String, Value> m = event.fields();
    if (m.size() != 6
        || !has(m, "event_type", Value.Kind.STRING)
        || !has(m, "key", Value.Kind.STRING)
        || ((String) m.get("key").data()).isEmpty()
        || !has(m, "event_id", Value.Kind.MAP)
        || !has(m, "generation", Value.Kind.MAP)
        || !has(m, "revision", Value.Kind.UINT)
        || !has(m, "timestamp_ms", Value.Kind.UINT)) throw Codec.protocol();
    if (!List.of(
            "KEY_CREATED",
            "KEY_UPDATED",
            "KEY_EXPIRED",
            "KEY_DELETED",
            "KEY_EVICTED",
            "KEY_TTL_CHANGED")
        .contains(m.get("event_type").data())) throw Codec.protocol();
    Map<String, Value> id = m.get("event_id").fields(), g = m.get("generation").fields();
    if (id.size() != 4
        || !has(id, "stream_id", Value.Kind.BYTES)
        || !Arrays.equals((byte[]) id.get("stream_id").data(), stream)
        || !Value.uint(database).equals(id.get("db"))
        || !has(id, "shard", Value.Kind.UINT)
        || ((BigInteger) id.get("shard").data()).bitLength() > 32
        || !positive(id, "sequence")
        || g.size() != 2
        || !has(g, "epoch", Value.Kind.BYTES)
        || ((byte[]) g.get("epoch").data()).length != 16
        || !positive(g, "counter")
        || !positive(m, "revision")) throw Codec.protocol();
  }

  private static boolean has(Map<String, Value> m, String key, Value.Kind kind) {
    return m.containsKey(key) && m.get(key).kind() == kind;
  }

  private static boolean positive(Map<String, Value> m, String key) {
    return has(m, key, Value.Kind.UINT) && ((BigInteger) m.get(key).data()).signum() > 0;
  }

  /**
   * 专用事件消费者；取消读取会关闭连接，命名组未确认位置保留。 Dedicated consumer; cancelled reads close while named-group
   * pending positions survive.
   */
  public static final class Subscriber implements AutoCloseable {
    private final Client client;

    private Subscriber(Client client) {
      this.client = client;
    }

    /** 阻塞直到事件或取消；空闲发送心跳。 Waits for an event or cancellation and sends idle heartbeats. */
    public Value next() throws IOException {
      client.acquire();
      try {
        if (!client.queued.isEmpty()) {
          Value v = client.queued.remove();
          client.queuedBytes -= Codec.encode(v, 12).length;
          return v;
        }
        while (true) {
          Received r =
              client.incoming.poll(
                  Math.max(1, Math.min(client.options.requestTimeoutMs / 2, 10000)),
                  TimeUnit.MILLISECONDS);
          if (r == null) {
            if (client.closed) throw new Failure("ERR_CLIENT_CLOSED", false);
            ScheduledFuture<?> limit =
                TIMER.schedule(
                    client::close, client.options.requestTimeoutMs, TimeUnit.MILLISECONDS);
            try {
              Codec.write(client.socket.getOutputStream(), new Codec.Frame(5, 0, 0, new byte[0]));
            } finally {
              limit.cancel(false);
            }
            continue;
          }
          if (r.error != null) throw r.error;
          Codec.Frame f = r.frame;
          if (f.kind() == 5) continue;
          if (f.kind() == 6) throw serverError(new Codec.Decoder(f.payload()));
          if (f.kind() != 3) throw Codec.protocol();
          Codec.Decoder d = new Codec.Decoder(f.payload());
          Value v = d.value(0);
          d.end();
          if (d.semantic) throw Codec.protocol();
          client.validateEvent(v);
          return v;
        }
      } catch (InterruptedException e) {
        client.close();
        Thread.currentThread().interrupt();
        throw new Failure("ERR_CLIENT_CANCELLED", false);
      } catch (IOException e) {
        client.close();
        throw e;
      } finally {
        client.lock.unlock();
      }
    }

    /** 显式确认复合 ID，不自动重试。 Explicitly acknowledges compound IDs without retries. */
    public void ack(Value id) throws IOException {
      Result result = client.exchange(4, 0, Codec.encode(id, 12), 0).get(0);
      if (result.error != null) throw result.error;
      if (!result.value.equals(new Value(Value.Kind.BOOL, true))) throw Codec.protocol();
    }

    /** 专用连接探活。 Dedicated connection probe. */
    public void ping() throws IOException {
      client.execute("PING");
    }

    /** 释放连接但不删除组。 Releases the connection without deleting the group. */
    @Override
    public void close() {
      client.close();
    }
  }
}
