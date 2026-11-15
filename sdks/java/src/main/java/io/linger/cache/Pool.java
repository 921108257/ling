package io.linger.cache;

import java.io.IOException;
import java.net.Socket;
import java.util.*;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/** 固定身份与数据库的有界连接池，不重放请求。 Bounded fixed-identity, fixed-database pool without request replay. */
public final class Pool implements AutoCloseable {
  private final Client.Options options;
  private final Semaphore capacity;
  private final ArrayDeque<Client> idle = new ArrayDeque<>();
  private final Set<Client> active = new HashSet<>();
  private final Set<Socket> connecting = new HashSet<>();
  private final int maximum;
  private volatile boolean closed;

  /** 延迟建连，容量为 1..65536。 Lazily connects with capacity 1..65536. */
  public Pool(Client.Options options, int capacity) {
    if (capacity < 1 || capacity > 65536)
      throw new IllegalArgumentException("Invalid pool capacity");
    this.options = options;
    this.capacity = new Semaphore(capacity, true);
    this.maximum = capacity;
  }

  /**
   * 独占固定库租约，可连续 SCAN；排队中断不会发送请求。 Exclusive fixed-database lease supports SCAN; queue interruption
   * never sends a request.
   */
  public Lease acquire() throws IOException {
    if (closed) throw new Client.Failure("ERR_CLIENT_CLOSED", false);
    try {
      if (!capacity.tryAcquire(options.requestTimeoutMs, TimeUnit.MILLISECONDS))
        throw new Client.Failure("ERR_CLIENT_TIMEOUT", false);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new Client.Failure("ERR_CLIENT_CANCELLED", false);
    }
    boolean owned = false;
    Client client = null;
    Socket[] registered = new Socket[1];
    try {
      synchronized (this) {
        if (closed) throw new Client.Failure("ERR_CLIENT_CLOSED", false);
        client = idle.poll();
      }
      if (client == null)
        client =
            Client.connect(
                options,
                socket -> {
                  synchronized (Pool.this) {
                    if (closed) throw new Client.Failure("ERR_CLIENT_CLOSED", false);
                    connecting.add(socket);
                    registered[0] = socket;
                  }
                });
      synchronized (this) {
        if (closed) {
          client.close();
          throw new Client.Failure("ERR_CLIENT_CLOSED", false);
        }
        active.add(client);
      }
      owned = true;
      return new Lease(client);
    } finally {
      synchronized (this) {
        if (registered[0] != null) connecting.remove(registered[0]);
      }
      if (!owned) {
        if (client != null) client.close();
        capacity.release();
      }
    }
  }

  /** 单次普通命令，不重放失败请求。 Executes one ordinary command without replaying failures. */
  public Value execute(String command, Value... args) throws IOException {
    try (Lease lease = acquire()) {
      return lease.execute(command, args);
    }
  }

  /** 独占租约归还前，调用方须等待其操作完成。 Callers finish lease operations before returning exclusive ownership. */
  public final class Lease implements AutoCloseable {
    private final Client client;
    private final AtomicBoolean returned = new AtomicBoolean();

    private Lease(Client client) {
      this.client = client;
    }

    /** 不提供 SELECT 或认证变更。 Excludes SELECT and authentication changes. */
    public Value execute(String command, Value... args) throws IOException {
      if (returned.get()) throw new Client.Failure("ERR_CLIENT_CLOSED", false);
      if (List.of("HELLO", "AUTH", "SELECT", "QUIT", "SUBSCRIBE", "UNSUBSCRIBE", "ACK")
          .contains(command.toUpperCase(Locale.ROOT)))
        throw new Client.Failure("ERR_CONNECTION_STATE", false);
      return client.execute(command, args);
    }

    /** 只归还健康且池仍开放的连接。 Returns only healthy sockets to an open pool. */
    @Override
    public void close() {
      if (returned.getAndSet(true)) return;
      synchronized (Pool.this) {
        active.remove(client);
        if (!closed && !client.isClosed()) idle.add(client);
        else client.close();
      }
      capacity.release();
    }
  }

  /** 关闭空闲及在用连接，并唤醒排队者。 Closes idle and checked-out sockets and wakes waiters. */
  @Override
  public synchronized void close() {
    if (closed) return;
    closed = true;
    for (Client c : idle) c.close();
    idle.clear();
    for (Client c : active) c.close();
    for (Socket socket : connecting) {
      try {
        socket.close();
      } catch (IOException ignored) {
        /* 关闭已成立。 Closed-state semantics already hold. */
      }
    }
    capacity.release(maximum);
  }
}
