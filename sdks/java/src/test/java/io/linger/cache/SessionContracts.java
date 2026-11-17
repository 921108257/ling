package io.linger.cache;

import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketException;
import java.net.SocketTimeoutException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicReference;

/** 三语言共享的受控会话契约，不依赖真实服务地址。 Shared controlled sessions need no live-server address. */
public final class SessionContracts {
  private SessionContracts() {}

  /** 每个夹具输出独立结果，任何失败使进程非零。 Reports each fixture and exits nonzero on any failure. */
  public static void main(String[] args) throws Exception {
    Path fixtures = Path.of(args[0]);
    List<String[]> rows = rows(fixtures.resolve("sessions.tsv"));
    byte[] event = frameFixture(fixtures, "event"), heartbeat = frameFixture(fixtures, "heartbeat");
    int failures = 0;
    for (String[] row : rows) {
      try {
        run(row, event, heartbeat);
        System.out.println("LING_CONTRACT\tsessions\t" + row[0] + "\tpassed");
      } catch (Exception | AssertionError error) {
        failures++;
        System.out.println("LING_CONTRACT\tsessions\t" + row[0] + "\tfailed");
        error.printStackTrace(System.err);
      }
    }
    if (failures != 0) throw new AssertionError("Session failures: " + failures);
  }

  private static List<String[]> rows(Path path) throws IOException {
    return Files.readAllLines(path).stream().map(String::trim)
        .filter(line -> !line.isEmpty() && !line.startsWith("#"))
        .map(line -> line.split("\\|", -1)).toList();
  }

  private static byte[] frameFixture(Path fixtures, String name) throws IOException {
    return HexFormat.of().parseHex(rows(fixtures.resolve("frames.tsv")).stream()
        .filter(row -> row[0].equals(name)).findFirst().orElseThrow()[1].replace(" ", ""));
  }

  private static void check(boolean condition, String description) {
    if (!condition) throw new AssertionError(description);
  }

  private static void send(Socket socket, int kind, int flags, long id, byte[] payload) throws IOException {
    Codec.write(socket.getOutputStream(), new Codec.Frame(kind, flags, id, payload));
  }

  private static byte[] response(Value value) throws IOException {
    byte[] encoded = Codec.encodeValue(value), result = new byte[encoded.length + 1];
    System.arraycopy(encoded, 0, result, 1, encoded.length);
    return result;
  }

  /** 同时检查固定库，重连不得沿用未经初始化的上下文。 Checks fixed database after every connection. */
  private static void initialize(Socket socket, int database) throws IOException {
    socket.setSoTimeout(5000);
    Codec.Frame hello = Codec.read(socket.getInputStream());
    check(hello.id() == 1 && Arrays.equals(hello.payload(), Codec.request("HELLO", List.of(Value.uint(1)))), "HELLO order");
    byte[] stream = new byte[16];
    stream[15] = 1;
    send(socket, 2, 0, 1, response(new Value(Value.Kind.MAP, Map.of(
        "protocol", Value.uint(1), "server_version", Value.string("fixture"),
        "events", new Value(Value.Kind.BOOL, true), "reliable_events", new Value(Value.Kind.BOOL, false),
        "stream_id", new Value(Value.Kind.BYTES, stream)))));
    Codec.Frame selected = Codec.read(socket.getInputStream());
    check(selected.id() == 2 && Arrays.equals(selected.payload(), Codec.request("SELECT", List.of(Value.uint(database)))), "fixed database");
    send(socket, 2, 0, 2, response(new Value(Value.Kind.BOOL, true)));
  }

  /** EOF 和重置表示关闭，超时或新请求均失败。 EOF and reset are closure; timeout or a new request fails. */
  private static void closed(Socket socket) throws IOException {
    try {
      for (int i = 0; i < 4; i++) {
        if (Codec.read(socket.getInputStream()).kind() != 5) throw new AssertionError("Unexpected repeated request");
      }
      throw new AssertionError("Unexpected repeated request");
    } catch (SocketTimeoutException error) {
      throw new AssertionError("Peer did not close", error);
    } catch (EOFException | SocketException expected) {
      // 已观察到关闭。 Closure was observed.
    }
  }

  private static Codec.Frame success(String mode) throws IOException {
    if (mode.equals("subscription")) return new Codec.Frame(2, 0, 4, response(new Value(Value.Kind.BOOL, true)));
    byte[] payload = response(Value.uint(1));
    if (!mode.equals("batch")) return new Codec.Frame(2, 0, 4, payload);
    byte[] batch = new byte[payload.length + 4];
    batch[3] = 1;
    System.arraycopy(payload, 0, batch, 4, payload.length);
    return new Codec.Frame(2, 1, 4, batch);
  }

  private static int peer(Socket socket, String mode, String action, byte[] event, byte[] heartbeat) throws IOException {
    boolean idle = mode.equals("idle-subscription"), subscribed = mode.endsWith("subscription");
    initialize(socket, 0);
    Codec.Frame prepare = Codec.read(socket.getInputStream());
    check(prepare.id() == 3, "preparation ID");
    List<Value> prepareArguments = subscribed
        ? List.of(Value.list(List.of(Value.string("*"))), Value.string("GROUP"), Value.string("sessions"))
        : List.of();
    check(prepare.kind() == 1 && prepare.flags() == 0 && Arrays.equals(prepare.payload(),
        Codec.request(subscribed ? "SUBSCRIBE" : "PING", prepareArguments)), "preparation command");
    send(socket, 2, 0, 3, response(subscribed ? new Value(Value.Kind.BOOL, true) : Value.string("PONG")));
    if (action.equals("closed")) { closed(socket); return 0; }
    int count = idle ? 0 : 1;
    if (!idle) {
      Codec.Frame target = Codec.read(socket.getInputStream());
      check(target.id() == 4, "target ID");
      check(target.kind() == (mode.equals("subscription") ? 4 : 1), "target kind");
    }
    switch (action) {
      case "disconnect": return 1;
      case "truncated":
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        Codec.write(bytes, success(mode));
        socket.getOutputStream().write(Arrays.copyOf(bytes.toByteArray(), 19));
        return 1;
      case "old-response": send(socket, 2, 0, 3, response(new Value(Value.Kind.BOOL, true))); break;
      case "old-batch": send(socket, 2, 1, 3, new byte[] {0, 0, 0, 1, 0, 1, 1}); break;
      case "heartbeat": socket.getOutputStream().write(heartbeat); break;
      case "event":
        socket.getOutputStream().write(event);
        if (!mode.equals("subscription")) { closed(socket); return 1; }
        socket.getOutputStream().write(heartbeat); break;
      case "zero-response", "future-response":
        send(socket, 2, 0, action.equals("zero-response") ? 0 : 5, response(Value.uint(1)));
        closed(socket); return count;
      case "zero-error", "future-error":
        ByteArrayOutputStream payload = new ByteArrayOutputStream();
        payload.write(Codec.encodeValue(Value.string("ERR_PROTOCOL")));
        payload.write(Codec.encodeValue(Value.string("")));
        send(socket, 6, 0, action.equals("zero-error") ? 0 : 5, payload.toByteArray());
        closed(socket); return count;
      default: throw new AssertionError("Unknown peer action");
    }
    if (idle) socket.getOutputStream().write(event);
    else Codec.write(socket.getOutputStream(), success(mode));
    closed(socket);
    return count;
  }

  private static Client.Options options(int port, int db) {
    return new Client.Options("127.0.0.1", port, db, null, 2000, 2000, null, null);
  }

  /** 故障场景显式关闭，不依赖作用域清理。 Explicit close is the action under test. */
  private static void closeClient(Client client) { client.close(); }

  private static void closeListener(ServerSocket listener) throws IOException { listener.close(); }

  private static void verify(Client.Failure error, String expected, boolean unknown) {
    if (expected.startsWith("ok")) { check(error == null, "Expected success: " + error); return; }
    check(error != null && error.outcomeUnknown == unknown, "Unknown outcome: " + error);
    String code = switch (expected) {
      case "protocol" -> "ERR_CLIENT_PROTOCOL";
      case "disconnected" -> "ERR_CLIENT_DISCONNECTED";
      case "closed" -> "ERR_CLIENT_CLOSED";
      case "connect" -> "ERR_CLIENT_CONNECT";
      case "incomplete" -> error.code;
      default -> expected;
    };
    if (expected.equals("incomplete")) check(code.equals("ERR_CLIENT_PROTOCOL") || code.equals("ERR_CLIENT_DISCONNECTED"), "Incomplete response classification");
    check(error.code.equals(code), "Expected " + expected + ": " + error);
  }

  private static void run(String[] row, byte[] event, byte[] heartbeat) throws Exception {
    if (row[1].equals("pool")) { pool(row); return; }
    try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
      int port = listener.getLocalPort();
      if (row[2].equals("refused")) {
        closeListener(listener);
        Client.Failure failure = null;
        try (Client ignored = Client.connect(options(port, 0))) { check(ignored.isClosed(), "Connection unexpectedly succeeded"); }
        catch (Client.Failure error) { failure = error; }
        verify(failure, row[3], false);
        return;
      }
      AtomicReference<Throwable> peerFailure = new AtomicReference<>();
      int[] count = {0};
      Thread thread = new Thread(() -> {
        try {
          try (Socket socket = listener.accept()) { count[0] = peer(socket, row[1], row[2], event, heartbeat); }
          if (row[2].equals("disconnect") || row[2].equals("truncated")) {
            // 保留监听器观察隐式重连。 Keep listening to observe implicit reconnects.
            listener.setSoTimeout(150);
            try (Socket repeated = listener.accept()) { throw new AssertionError("Implicit reconnect: " + repeated.isConnected()); }
            catch (SocketTimeoutException expected) { /* 没有重放。 No replay. */ }
          }
        }
        catch (Throwable error) { peerFailure.set(error); }
      });
      thread.start();
      Client.Failure failure = null;
      try (Client client = Client.connect(options(port, 0))) {
        try {
          if (row[1].endsWith("subscription")) {
            Client.Subscriber subscriber = client.subscribe(List.of("*"), "sessions");
            Codec.Frame frame = Codec.read(new java.io.ByteArrayInputStream(event));
            Value value = Codec.decodeValue(frame.payload());
            if (row[1].equals("idle-subscription")) check(subscriber.next().equals(value), "Idle event differs");
            else {
              subscriber.ack(value.fields().get("event_id"));
              if (row[3].equals("ok-event")) check(subscriber.next().equals(value), "Queued event differs");
            }
          } else {
            client.execute("PING");
            if (row[2].equals("closed")) closeClient(client);
            if (row[1].equals("batch")) {
              List<Client.Result> values = client.batch(List.of(new Client.Request("INCR", List.of(Value.string("counter")))));
              check(values.size() == 1 && values.get(0).error() == null && values.get(0).value().equals(Value.uint(1)), "Batch value");
            } else check(client.execute("INCR", Value.string("counter")).equals(Value.uint(1)), "Response value");
          }
        } catch (Client.Failure error) { failure = error; }
        verify(failure, row[3], Boolean.parseBoolean(row[4]));
        check(client.isClosed() == Boolean.parseBoolean(row[5]), "Closed state differs");
      } finally {
        thread.join(6000);
        check(!thread.isAlive() && peerFailure.get() == null, "Peer failed: " + peerFailure.get());
      }
      check(count[0] == Integer.parseInt(row[6]), "Request count");
    }
  }

  /** 重建连接重新 SELECT，失效写入不自动重放。 Rebuilding selects the database again without replaying writes. */
  private static void pool(String[] row) throws Exception {
    try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
      AtomicReference<Throwable> failure = new AtomicReference<>();
      int[] count = {0};
      boolean reset = row[2].equals("pool-reset");
      Thread peer = new Thread(() -> {
        try {
          for (int i = 0; i < (reset ? 2 : 1); i++) {
            try (Socket socket = listener.accept()) {
              initialize(socket, 7);
              Codec.Frame target = Codec.read(socket.getInputStream());
              check(target.id() == 3 && Arrays.equals(target.payload(), Codec.request("INCR", List.of(Value.string("counter")))), "Pool target");
              count[0]++;
              if (reset && i == 0) continue;
              send(socket, 2, 0, 3, response(Value.uint(1)));
              closed(socket);
            }
          }
        } catch (Throwable error) { failure.set(error); }
      });
      peer.start();
      try (Pool pool = new Pool(options(listener.getLocalPort(), 7), 1)) {
        if (reset) {
          try { pool.execute("INCR", Value.string("counter")); throw new AssertionError("Disconnected write succeeded"); }
          catch (Client.Failure error) { check(error.outcomeUnknown, "Write outcome must be unknown"); }
        } else {
          try (Pool.Lease lease = pool.acquire()) {
            try { lease.execute("SELECT", Value.uint(0)); throw new AssertionError("Pool allowed SELECT"); }
            catch (Client.Failure error) { check(!error.outcomeUnknown, "SELECT was sent"); }
          }
        }
        check(pool.execute("INCR", Value.string("counter")).equals(Value.uint(1)), "Rebuilt pool value");
      } finally {
        peer.join(6000);
        check(!peer.isAlive() && failure.get() == null, "Pool peer: " + failure.get());
      }
      check(count[0] == Integer.parseInt(row[6]), "Pool request count");
    }
  }
}
