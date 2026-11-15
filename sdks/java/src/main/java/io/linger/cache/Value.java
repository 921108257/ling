package io.linger.cache;

import java.math.BigInteger;
import java.util.*;

/** 显式十种类型，不经 JSON 或数值提升。 Ten explicit tags without JSON or numeric coercion. */
public final class Value {
  /** 冻结的线上标记顺序。 Frozen wire tag order. */
  public enum Kind {
    /** 成功空值。 Successful null. */
    NULL,
    /** 布尔。 Boolean. */
    BOOL,
    /** 有符号 64 位。 Signed 64-bit. */
    INT,
    /** 无符号 64 位。 Unsigned 64-bit. */
    UINT,
    /** 排除 NaN 的 IEEE 754。 IEEE 754 excluding NaN. */
    FLOAT,
    /** 严格 UTF-8。 Strict UTF-8. */
    STRING,
    /** 原始字节。 Raw bytes. */
    BYTES,
    /** 有序值。 Ordered values. */
    LIST,
    /** 非空字符串字段。 Nonempty String fields. */
    MAP,
    /** 类型敏感标量集合。 Type-sensitive scalar set. */
    SET
  }

  private final Kind kind;
  private final Object data;

  /** 验证并防御性复制；UInt 只接受范围内 BigInteger。 Validates and copies; UInt requires a bounded BigInteger. */
  public Value(Kind kind, Object data) {
    this.kind = Objects.requireNonNull(kind);
    this.data =
        switch (kind) {
          case NULL -> {
            if (data != null) throw invalid();
            yield null;
          }
          case BOOL -> {
            if (!(data instanceof Boolean)) throw invalid();
            yield data;
          }
          case INT -> {
            if (!(data instanceof Long)) throw invalid();
            yield data;
          }
          case UINT -> {
            if (!(data instanceof BigInteger n) || n.signum() < 0 || n.bitLength() > 64)
              throw invalid();
            yield data;
          }
          case FLOAT -> {
            if (!(data instanceof Double d) || Double.isNaN(d)) throw invalid();
            yield d == 0.0 ? 0.0 : d;
          }
          case STRING -> {
            if (!(data instanceof String s)) throw invalid();
            Codec.utf8(s);
            yield data;
          }
          case BYTES -> {
            if (!(data instanceof byte[] b)) throw invalid();
            yield b.clone();
          }
          case LIST, SET -> {
            if (!(data instanceof List<?> list)) throw invalid();
            List<Value> values = new ArrayList<>();
            for (Object item : list) {
              if (!(item instanceof Value v) || (kind == Kind.SET && v.kind.ordinal() > 6))
                throw invalid();
              if (kind != Kind.SET || !values.contains(v)) values.add(v);
            }
            yield List.copyOf(values);
          }
          case MAP -> {
            if (!(data instanceof Map<?, ?> map)) throw invalid();
            Map<String, Value> values = new TreeMap<>();
            for (var e : map.entrySet()) {
              if (!(e.getKey() instanceof String s)
                  || s.isEmpty()
                  || !(e.getValue() instanceof Value v)) throw invalid();
              Codec.utf8(s);
              values.put(s, v);
            }
            yield Collections.unmodifiableMap(values);
          }
        };
  }

  /** 类型标签。 Type tag. */
  public Kind kind() {
    return kind;
  }

  /** 不可变内容，Bytes 返回副本。 Immutable contents; Bytes returns a copy. */
  public Object data() {
    return data instanceof byte[] b ? b.clone() : data;
  }

  /** 安全 String 构造。 String factory. */
  public static Value string(String s) {
    return new Value(Kind.STRING, s);
  }

  /** UInt 小整数构造；负值拒绝。 Small UInt factory rejecting negatives. */
  public static Value uint(long n) {
    return new Value(Kind.UINT, BigInteger.valueOf(n));
  }

  /** 成功空值。 Successful null. */
  public static Value nil() {
    return new Value(Kind.NULL, null);
  }

  /** 有序参数或列表。 Ordered arguments or list. */
  public static Value list(List<Value> a) {
    return new Value(Kind.LIST, a);
  }

  /** Map 的类型化访问，不进行转换。 Typed Map access without coercion. */
  @SuppressWarnings("unchecked")
  public Map<String, Value> fields() {
    if (kind != Kind.MAP) throw invalid();
    return (Map<String, Value>) data;
  }

  /** 类型敏感相等，Bytes 按内容，Set 无序。 Type-sensitive equality with byte contents and unordered Sets. */
  @Override
  public boolean equals(Object o) {
    if (!(o instanceof Value v) || v.kind != kind) return false;
    if (kind == Kind.BYTES) return Arrays.equals((byte[]) data, (byte[]) v.data);
    if (kind == Kind.SET)
      return new HashSet<>((List<?>) data).equals(new HashSet<>((List<?>) v.data));
    return Objects.equals(data, v.data);
  }

  /** 与相等规则一致。 Consistent with equality. */
  @Override
  public int hashCode() {
    return 31 * kind.hashCode()
        + (kind == Kind.BYTES
            ? Arrays.hashCode((byte[]) data)
            : kind == Kind.SET ? new HashSet<>((List<?>) data).hashCode() : Objects.hashCode(data));
  }

  /** 不在诊断中显示业务内容。 Diagnostics do not expose business contents. */
  @Override
  public String toString() {
    return "Value(" + kind + ")";
  }

  /** 不回显无效业务内容。 Never echoes invalid business contents. */
  private static IllegalArgumentException invalid() {
    return new IllegalArgumentException("ERR_INVALID_VALUE");
  }
}
