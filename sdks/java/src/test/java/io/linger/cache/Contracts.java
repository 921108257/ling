package io.linger.cache;

import java.io.*;
import java.net.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicReference;
import javax.net.ssl.SSLContext;

/**
 * 独立黄金、故障及真实服务验收；无需测试框架依赖。 Independent fixtures, failure, and live-server contracts without
 * test-framework dependencies.
 */
public final class Contracts {
  private Contracts() {}

  /** 失败即非零退出，真实服务由仓库驱动器提供。 Failures exit nonzero; the repository harness supplies live servers. */
  public static void main(String[] args) throws Exception {
    Path fixtures = Path.of(args[0]);
    protocol(fixtures);
    unknownOutcome();
    if (System.getenv("LINGER_TEST_ADDR") != null) live(fixtures);
    System.out.println("Java contracts passed");
  }

  /** 每个 SDK 使用同一份冻结用例。 Each SDK uses the same frozen fixture files. */
  private static List<String[]> lines(Path fixtures, String name) throws IOException {
    List<String[]> result = new ArrayList<>();
    for (String line : Files.readAllLines(fixtures.resolve(name))) {
      if (line.isBlank() || line.startsWith("#")) continue;
      result.add(Arrays.stream(line.split("\\|", -1)).map(String::trim).toArray(String[]::new));
    }
    return result;
  }

  private static byte[] unhex(String text) {
    return HexFormat.of().parseHex(text.replace(" ", ""));
  }

  private static void check(boolean condition, String name) {
    if (!condition) throw new AssertionError(name);
  }

  /**
   * 十种类型、畸形结构及独立帧，绝不从被测编码器产生期望值。 Ten tags, malformed structures, and independent frames never
   * derive expectations from the tested encoder.
   */
  private static void protocol(Path fixtures) throws Exception {
    for (String[] row : lines(fixtures, "values.tsv")) {
      byte[] expected = unhex(row[1]);
      Value value = Codec.decodeValue(expected);
      check(Arrays.equals(Codec.encodeValue(value), expected), row[0]);
    }
    for (String[] row : lines(fixtures, "invalid-values.tsv")) {
      try {
        Codec.decodeValue(unhex(row[3]));
        throw new AssertionError(row[0]);
      } catch (Codec.DecodeException e) {
        check(e.code.equals(row[2]) && e.fatal == Boolean.parseBoolean(row[1]), row[0]);
      }
    }
    for (String[] row : lines(fixtures, "frames.tsv")) {
      byte[] expected = unhex(row[1]);
      Codec.Frame frame = Codec.read(new ByteArrayInputStream(expected));
      ByteArrayOutputStream out = new ByteArrayOutputStream();
      Codec.write(out, frame);
      check(Arrays.equals(out.toByteArray(), expected), row[0]);
      Codec.Decoder d = new Codec.Decoder(frame.payload());
      if (frame.flags() == 1) d.count();
      switch (frame.kind()) {
        case 1:
          d.value(0);
          d.value(0);
          break;
        case 2:
          d.need(1);
          int status = Byte.toUnsignedInt(d.data.get());
          d.value(0);
          if (status == 1) d.value(0);
          break;
        case 3, 4:
          d.value(0);
          break;
        case 6:
          d.value(0);
          d.value(0);
          break;
        default:
          break;
      }
      d.end();
      check(!d.semantic, row[0]);
    }
    try {
      new Value(Value.Kind.FLOAT, Double.NaN);
      throw new AssertionError("NaN");
    } catch (IllegalArgumentException expected) {
      /* NaN 被拒绝。 NaN is rejected. */
    }
    check(
        new Value(Value.Kind.FLOAT, -0.0).equals(new Value(Value.Kind.FLOAT, 0.0)),
        "zero equality");
    Value set =
        new Value(
            Value.Kind.SET,
            List.of(
                new Value(Value.Kind.BYTES, new byte[] {0}),
                new Value(Value.Kind.BYTES, new byte[] {0})));
    check(((List<?>) set.data()).size() == 1, "byte set equality");
  }

  /** 地址和 TLS 身份只由测试驱动器注入。 Address and TLS identity are injected only by the harness. */
  private static Client.Options options() throws Exception {
    String[] address = System.getenv("LINGER_TEST_ADDR").split(":");
    String ca = System.getenv("LINGER_TEST_CA"), identity = System.getenv("LINGER_TEST_PKCS12");
    SSLContext tls =
        ca == null
            ? null
            : Tls.context(
                Path.of(ca),
                identity == null ? null : Path.of(identity),
                "test-only".toCharArray());
    return new Client.Options(
        address[0], Integer.parseInt(address[1]), 0, null, 1000, 1000, tls, "localhost");
  }

  /**
   * 共享命令与十类型对真实 Rust 服务端往返，包含订阅和池关闭。 Shared commands and ten tags round-trip through the real Rust
   * server, including subscriptions and pool close.
   */
  private static void live(Path fixtures) throws Exception {
    Client.Options options = options();
    try (Client client = Client.connect(options)) {
      for (String[] row : lines(fixtures, "commands.tsv")) {
        Value args = Codec.decodeValue(unhex(row[2]));
        List<?> list = (List<?>) args.data();
        Value[] values = list.toArray(Value[]::new);
        if (row[3].startsWith("ERR_")) {
          try {
            client.execute(row[1], values);
            throw new AssertionError(row[0]);
          } catch (Client.Failure error) {
            check(error.code.equals(row[3]), row[0]);
          }
        } else
          check(client.execute(row[1], values).equals(Codec.decodeValue(unhex(row[3]))), row[0]);
      }
      // 三语言共用 INFO 字段类型，动态计数不经 JSON。 Shared INFO types retain dynamic counters without JSON.
      Value info = client.execute("INFO");
      for (String[] row : lines(fixtures, "info-fields.tsv")) {
        Value value = info.fields().get(row[0]).fields().get(row[1]);
        boolean valid =
            switch (row[2]) {
              case "UInt" -> value.kind() == Value.Kind.UINT;
              case "Bool" -> value.kind() == Value.Kind.BOOL;
              case "Float" -> value.kind() == Value.Kind.FLOAT;
              case "UIntOrNull" -> value.kind() == Value.Kind.UINT || value.kind() == Value.Kind.NULL;
              case "FloatOrNull" -> value.kind() == Value.Kind.FLOAT || value.kind() == Value.Kind.NULL;
              case "Histogram" -> value.kind() == Value.Kind.MAP
                  && value.fields().get("count").kind() == Value.Kind.UINT
                  && value.fields().get("sum").kind() == Value.Kind.UINT
                  && ((List<?>) value.fields().get("bounds").data()).size() == 6
                  && ((List<?>) value.fields().get("buckets").data()).size() == 7;
              default -> false;
            };
        check(valid, "INFO " + row[0] + "." + row[1]);
      }
      for (String[] row : lines(fixtures, "values.tsv")) {
        Value value = Codec.decodeValue(unhex(row[1])), key = Value.string("java:" + row[0]);
        client.execute("SET", key, value);
        check(client.execute("GET", key).equals(value), "live " + row[0]);
      }
      Value key = Value.string("java:batch");
      List<Client.Result> results =
          client.batch(
              List.of(
                  new Client.Request("SET", List.of(key, Value.nil())),
                  new Client.Request("GET", List.of(key)),
                  new Client.Request("GET", List.of(Value.string("absent")))));
      check(
          results.size() == 3
              && results.get(1).value().equals(Value.nil())
              && results.get(2).error() != null,
          "batch results");
      Client subscription = Client.connect(options);
      byte[] stream = subscription.streamId();
      // 切库后的订阅保留当前数据库；Subscriptions retain the selected database.
      client.execute("SELECT", Value.uint(1));
      subscription.execute("SELECT", Value.uint(1));
      Client.Subscriber subscriber = subscription.subscribe(List.of("java:event"), "java-workers");
      client.execute("SET", Value.string("java:event"), Value.nil());
      Value event = subscriber.next(), id = event.fields().get("event_id");
      check(id.fields().get("db").equals(Value.uint(1)), "subscription database");
      subscriber.close();
      subscription = Client.connect(options);
      subscription.requireEventStream(stream);
      subscription.execute("SELECT", Value.uint(1));
      subscriber = subscription.subscribe(List.of("java:event"), "java-workers");
      check(subscriber.next().fields().get("event_id").equals(id), "stable redelivery ID");
      subscriber.ack(id);
      subscriber.ping();
      AtomicReference<Throwable> outcome = new AtomicReference<>();
      Client.Subscriber waiting = subscriber;
      Thread thread =
          new Thread(
              () -> {
                try {
                  waiting.next();
                  outcome.set(new AssertionError("cancelled next succeeded"));
                } catch (IOException e) {
                  outcome.set(e);
                }
              });
      thread.start();
      Thread.sleep(20);
      thread.interrupt();
      thread.join(1000);
      check(
          !thread.isAlive() && outcome.get() instanceof Client.Failure,
          "subscription interruption");
      subscriber.close();
      try (Pool pool = new Pool(options, 2)) {
        check(pool.execute("GET", key).equals(Value.nil()), "pool get");
        try {
          pool.execute("SELECT", Value.uint(1));
          throw new AssertionError("pooled select");
        } catch (Client.Failure expected) {
          check(!expected.outcomeUnknown, "pool reject before send");
        }
        try (Pool.Lease lease = pool.acquire()) {
          lease.execute("SCAN", Value.uint(0));
        }
      }
      Pool stopped = new Pool(options, 1);
      try (Pool.Lease lease = stopped.acquire()) {
        stopped.close();
        try {
          lease.execute("GET", key);
          throw new AssertionError("closed pool lease");
        } catch (Client.Failure expected) {
          check(!expected.outcomeUnknown, "closed lease rejects before send");
        }
      } finally {
        stopped.close();
      }
    }
  }

  /**
   * 服务读到写入后断线，客户端必须暴露结果未知。 Disconnect after the server receives a write must expose an unknown
   * outcome.
   */
  private static void unknownOutcome() throws Exception {
    try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
      AtomicReference<Throwable> failure = new AtomicReference<>();
      Thread server =
          new Thread(
              () -> {
                try (Socket socket = listener.accept()) {
                  for (int i = 0; i < 2; i++) {
                    Codec.Frame f = Codec.read(socket.getInputStream());
                    Value value = new Value(Value.Kind.BOOL, true);
                    if (i == 0)
                      value =
                          new Value(
                              Value.Kind.MAP,
                              Map.of(
                                  "protocol",
                                  Value.uint(1),
                                  "server_version",
                                  Value.string("test"),
                                  "events",
                                  new Value(Value.Kind.BOOL, false),
                                  "reliable_events",
                                  new Value(Value.Kind.BOOL, false),
                                  "stream_id",
                                  Value.nil()));
                    byte[] encoded = Codec.encodeValue(value),
                        payload = new byte[encoded.length + 1];
                    System.arraycopy(encoded, 0, payload, 1, encoded.length);
                    Codec.write(socket.getOutputStream(), new Codec.Frame(2, 0, f.id(), payload));
                  }
                  Codec.read(socket.getInputStream());
                } catch (Throwable e) {
                  failure.set(e);
                }
              });
      server.start();
      try (Client client =
          Client.connect(new Client.Options("127.0.0.1", listener.getLocalPort()))) {
        try {
          client.execute("INCR", Value.string("counter"));
          throw new AssertionError("unknown result");
        } catch (Client.Failure error) {
          check(error.outcomeUnknown, "unknown outcome");
        }
      }
      server.join(1000);
      check(!server.isAlive() && failure.get() == null, "fake server completion");
    }
  }
}
