package io.linger.cache;

import java.io.*;
import java.math.BigInteger;
import java.nio.*;
import java.nio.charset.*;
import java.util.*;

/**
 * 版本一编解码，独立执行分配、UTF-8 和结构上限。 Version-one codec with independent allocation, UTF-8, and structural
 * limits.
 */
public final class Codec {
  /** 默认最大单向负载。 Default maximum directional payload. */
  public static final int MAX_BYTES = 64 * 1024 * 1024;

  private Codec() {}

  /** 协议错误保留致命判定，不携带输入内容。 Decode errors retain fatality without input contents. */
  public static final class DecodeException extends IOException {
    private static final long serialVersionUID = 1L;

    /** 稳定错误码。 Stable error code. */
    public final String code;

    /** 为真必须关闭连接。 True requires closing the connection. */
    public final boolean fatal;

    DecodeException(String code, boolean fatal) {
      super(code);
      this.code = code;
      this.fatal = fatal;
    }
  }

  /** 原始帧，负载在构造与读取时复制。 Raw frame with defensive payload copies. */
  public record Frame(int kind, int flags, long id, byte[] payload) {
    /** 防止调用方在发送期间修改。 Prevents mutation during transmission. */
    public Frame {
      payload = payload.clone();
    }

    /** 读取副本。 Returns a copy. */
    @Override
    public byte[] payload() {
      return payload.clone();
    }
  }

  /** 编码单 Value，在分配前验证大小和业务深度。 Encodes a Value after size and business-depth checks. */
  public static byte[] encodeValue(Value value) {
    return encode(value, 8);
  }

  static byte[] encode(Value value, int limit) {
    int size = measure(value, 0, limit);
    ByteBuffer out = ByteBuffer.allocate(size);
    write(out, value);
    return out.array();
  }

  /** 解码单 Value，拒绝尾随内容。 Decodes a single Value and rejects trailing contents. */
  public static Value decodeValue(byte[] bytes) throws IOException {
    Decoder d = new Decoder(bytes);
    Value v = d.value(0);
    d.end();
    if (d.semantic) throw semantic();
    measure(v, 0, 8);
    return v;
  }

  /** 完整帧读取先检查头和长度，随后有限分配。 Reads and validates the header before bounded payload allocation. */
  public static Frame read(InputStream in) throws IOException {
    byte[] h = exact(in, 18);
    ByteBuffer b = ByteBuffer.wrap(h);
    int magic = Short.toUnsignedInt(b.getShort());
    int version = Byte.toUnsignedInt(b.get());
    int kind = Byte.toUnsignedInt(b.get());
    int flags = Short.toUnsignedInt(b.getShort());
    long id = b.getLong();
    long n = Integer.toUnsignedLong(b.getInt());
    if (magic != 0x4c47
        || version != 1
        || kind < 1
        || kind > 6
        || flags > 1
        || (flags == 1 && kind != 1 && kind != 2)
        || n > MAX_BYTES
        || (kind == 5 && (id != 0 || n != 0))
        || (kind == 3 && id != 0)) throw protocol();
    return new Frame(kind, flags, id, exact(in, (int) n));
  }

  /** 发送前完整校验，写入失败不表示未执行。 Validates before writing; write failure does not prove nonexecution. */
  public static void write(OutputStream out, Frame frame) throws IOException {
    byte[] p = frame.payload;
    ByteBuffer h = ByteBuffer.allocate(18);
    h.putShort((short) 0x4c47)
        .put((byte) 1)
        .put((byte) frame.kind)
        .putShort((short) frame.flags)
        .putLong(frame.id)
        .putInt(p.length);
    if (p.length > MAX_BYTES) throw protocol();
    out.write(h.array());
    out.write(p);
    out.flush();
  }

  /**
   * 请求封套允许额外四层，不能偷偷改变业务 Value 类型。 Request envelopes permit four extra levels without changing
   * business tags.
   */
  static byte[] request(String command, List<Value> args) {
    String canonical = command.toUpperCase(Locale.ROOT);
    if (!canonical.matches("[A-Z][A-Z0-9_]*"))
      throw new IllegalArgumentException("Invalid command");
    byte[] name = encode(Value.string(canonical), 12), values = encode(Value.list(args), 12);
    if ((long) name.length + values.length > MAX_BYTES)
      throw new IllegalArgumentException("Request exceeds budget");
    byte[] result = Arrays.copyOf(name, name.length + values.length);
    System.arraycopy(values, 0, result, name.length, values.length);
    return result;
  }

  /** 严格 UTF-8 编码，拒绝未配对代理。 Strict UTF-8 encoding rejects unpaired surrogates. */
  static byte[] utf8(String text) {
    try {
      ByteBuffer b =
          StandardCharsets.UTF_8
              .newEncoder()
              .onMalformedInput(CodingErrorAction.REPORT)
              .encode(java.nio.CharBuffer.wrap(text));
      byte[] out = new byte[b.remaining()];
      b.get(out);
      return out;
    } catch (CharacterCodingException e) {
      throw new IllegalArgumentException("ERR_INVALID_VALUE");
    }
  }

  /** 长度在所有分配前累计。 Accumulates lengths before allocation. */
  @SuppressWarnings("unchecked")
  private static int measure(Value v, int depth, int limit) {
    if (depth > limit) throw new IllegalArgumentException("Value depth exceeded");
    long n = 1;
    Object x = v.data();
    switch (v.kind()) {
      case NULL:
        break;
      case BOOL:
        n++;
        break;
      case INT, UINT, FLOAT:
        n += 8;
        break;
      case STRING:
        n += 4L + utf8((String) x).length;
        break;
      case BYTES:
        n += 4L + ((byte[]) x).length;
        break;
      case LIST, SET:
        if (depth == limit) throw new IllegalArgumentException("Value depth exceeded");
        n += 4;
        for (Value item : (List<Value>) x) {
          n += measure(item, depth + 1, limit);
          if (n > MAX_BYTES) throw new IllegalArgumentException("Value too large");
        }
        break;
      case MAP:
        if (depth == limit) throw new IllegalArgumentException("Value depth exceeded");
        n += 4;
        for (var e : ((Map<String, Value>) x).entrySet()) {
          n += 5L + utf8(e.getKey()).length + measure(e.getValue(), depth + 1, limit);
          if (n > MAX_BYTES) throw new IllegalArgumentException("Value too large");
        }
        break;
    }
    if (n > MAX_BYTES) throw new IllegalArgumentException("Value too large");
    return (int) n;
  }

  /** 大端字段只在验证后写入。 Writes big-endian fields only after validation. */
  @SuppressWarnings("unchecked")
  private static void write(ByteBuffer out, Value v) {
    out.put((byte) v.kind().ordinal());
    Object x = v.data();
    switch (v.kind()) {
      case NULL:
        break;
      case BOOL:
        out.put((byte) ((boolean) x ? 1 : 0));
        break;
      case INT:
        out.putLong((long) x);
        break;
      case UINT:
        out.putLong(((BigInteger) x).longValue());
        break;
      case FLOAT:
        out.putDouble((double) x);
        break;
      case STRING:
        byte[] s = utf8((String) x);
        out.putInt(s.length).put(s);
        break;
      case BYTES:
        byte[] b = (byte[]) x;
        out.putInt(b.length).put(b);
        break;
      case LIST, SET:
        List<Value> a = (List<Value>) x;
        out.putInt(a.size());
        for (Value item : a) write(out, item);
        break;
      case MAP:
        Map<String, Value> m = (Map<String, Value>) x;
        out.putInt(m.size());
        for (var e : m.entrySet()) {
          write(out, Value.string(e.getKey()));
          write(out, e.getValue());
        }
        break;
    }
  }

  /** 网络 EOF 不能伪装成成功 Null。 Network EOF cannot masquerade as successful Null. */
  private static byte[] exact(InputStream in, int n) throws IOException {
    byte[] b = in.readNBytes(n);
    if (b.length != n) throw new EOFException();
    return b;
  }

  static DecodeException protocol() {
    return new DecodeException("ERR_PROTOCOL", true);
  }

  static DecodeException semantic() {
    return new DecodeException("ERR_INVALID_VALUE", false);
  }

  /**
   * 输入有界，count 与剩余长度、预算共同验证。 Bounded input checks counts against remaining bytes and allocation
   * budget.
   */
  static final class Decoder {
    final ByteBuffer data;
    long budget = 128L * 1024 * 1024;
    boolean semantic;

    Decoder(byte[] bytes) {
      data = ByteBuffer.wrap(bytes);
    }

    int count() throws IOException {
      need(4);
      long n = Integer.toUnsignedLong(data.getInt());
      if (n > Integer.MAX_VALUE) throw protocol();
      return (int) n;
    }

    void need(int n) throws IOException {
      if (n < 0 || data.remaining() < n) throw protocol();
    }

    void charge(long n) throws IOException {
      if (n < 0 || n > budget) throw protocol();
      budget -= n;
    }

    void end() throws IOException {
      if (data.hasRemaining()) throw protocol();
    }

    Value value(int depth) throws IOException {
      need(1);
      int tag = Byte.toUnsignedInt(data.get());
      if (tag > 9 || depth > 12) throw protocol();
      Value.Kind k = Value.Kind.values()[tag];
      Object x = null;
      switch (k) {
        case NULL:
          break;
        case BOOL:
          need(1);
          int b = Byte.toUnsignedInt(data.get());
          if (b > 1) throw protocol();
          x = b == 1;
          break;
        case INT:
          need(8);
          x = data.getLong();
          break;
        case UINT:
          need(8);
          byte[] u = new byte[8];
          data.get(u);
          x = new BigInteger(1, u);
          break;
        case FLOAT:
          need(8);
          double f = data.getDouble();
          if (Double.isNaN(f)) {
            semantic = true;
            f = 0;
          }
          x = f;
          break;
        case STRING, BYTES:
          int n = count();
          need(n);
          charge(n);
          byte[] bytes = new byte[n];
          data.get(bytes);
          if (k == Value.Kind.BYTES) x = bytes;
          else
            try {
              x =
                  StandardCharsets.UTF_8
                      .newDecoder()
                      .onMalformedInput(CodingErrorAction.REPORT)
                      .decode(ByteBuffer.wrap(bytes))
                      .toString();
            } catch (CharacterCodingException e) {
              throw protocol();
            }
          break;
        case LIST, SET, MAP:
          int count = count(), minimum = k == Value.Kind.MAP ? 6 : 1;
          if (count > data.remaining() / minimum) throw protocol();
          charge((long) count * 64);
          if (k == Value.Kind.MAP) {
            Map<String, Value> map = new TreeMap<>();
            for (int i = 0; i < count; i++) {
              Value key = value(depth + 1);
              if (key.kind() != Value.Kind.STRING) throw protocol();
              String s = (String) key.data();
              Value item = value(depth + 1);
              if (s.isEmpty() || map.containsKey(s)) semantic = true;
              if (!s.isEmpty()) map.put(s, item);
            }
            x = map;
          } else {
            List<Value> list = new ArrayList<>();
            for (int i = 0; i < count; i++) {
              Value item = value(depth + 1);
              if (k == Value.Kind.SET && item.kind().ordinal() > 6) {
                semantic = true;
                continue;
              }
              list.add(item);
            }
            x = list;
          }
          break;
      }
      return new Value(k, x);
    }
  }
}
