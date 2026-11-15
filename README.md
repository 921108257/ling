# linger-cache-db

独立部署的 Rust 内存缓存数据库：自定义二进制协议、10 种强类型、固定分片和多数据库；已支持毫秒 TTL、Fast 事件与可选 Reliable Journal。

**当前为 0.1.0 阶段 5 事件与 Journal 预览原型。** 已实现共享类型、版本 1 编解码、数据引擎、TCP 服务、Rust SDK 和 CLI。支持十种 Value、隔离数据库、数值与容器命令，以及跨库复制 / 移动、重命名、遍历、清库、数据库交换及完整 TTL 生命周期；清单见 [命令契约](docs/commands.md)。

每个 (数据库, Shard) 拥有独立索引与锁，多 Key 命令按固定顺序取得全部执行权后整体发布。可配置全实例受管内存、每库 Key 配额及 noeviction / lru / lfu / random / ttl-first 策略；受管内存不等于进程 RSS。启用淘汰且配置容量上限时，当前写入保守取得全实例执行权。ttl-first 选择最早到期的活跃条目，无期限候选时拒绝写入。

已支持 SET EX / PX / EXAT / PXAT、EXPIRE 系列、PERSIST 和四种 TTL 查询；逻辑到期不依赖后台回收。默认启用有界 Fast 事件，可选 Reliable 持久化事件与消费元数据；缓存和 TTL 重启后仍为空。尚不支持 TLS、INFO / CONFIG / SHUTDOWN、连接池和 Java / Go SDK。Batch 仅有编码支持，执行返回 `ERR_INVALID_ARGUMENT`；尚未实现的命令返回 `ERR_UNKNOWN_COMMAND`。

## 构建与试用

需要 Rust 2024 工具链，Cargo 声明最低版本为 1.85。以下命令在仓库根目录执行：

```sh
cargo build --workspace
cargo run -p linger-server
```

服务端默认监听 `127.0.0.1:6380`。另开终端调用 CLI：

```sh
cargo run -p linger-cli -- -c 'PING'
cargo run -p linger-cli -- -c 'SET counter int(0)'
cargo run -p linger-cli -- -c 'INCRBY counter 2'
cargo run -p linger-cli -- -c 'SET session null PX 5000 NX'
cargo run -p linger-cli -- -c 'PTTL session'
cargo run -p linger-cli -- -c 'PERSIST session'
cargo run -p linger-cli -- -c 'COPY counter copied DB 1'
cargo run -p linger-cli -- --db 1 -c 'SCAN 0 COUNT 100'
cargo run -p linger-cli -- -c 'SET profile map({"name": "Lin", "visits": uint(1)})'
cargo run -p linger-cli -- -c 'FGET profile name visits'
printf 'SET nothing null\nGET nothing\nQUIT\n' | cargo run -p linger-cli
```

PING 输出 `"PONG"`，INCRBY 输出 `int(2)`；存储的 Null 输出 `null`，缺失 Key 则以独立错误和非零退出码表达。CLI 连接时自动完成 HELLO、可选 AUTH 和 SELECT；输入中的任何失败都会使最终退出码非零。Ctrl-C / SIGTERM 停止服务并关闭连接。SDK 不自动重放请求，发送后的断线或超时可通过 `outcome_unknown()` 判断不确定性。

无 `--config` 时服务端尝试读取当前目录的 `linger.toml`，不存在则使用默认配置。阶段 5 接受 `server`、`auth`、`database`、`protocol`、`engine`、`eviction`、`ttl`、`events`、`event_journal` 九组，完整 v1.0 候选配置暂不能整份使用。可通过 `--config <path>` 指定文件，例如：

```toml
[server]
host = "127.0.0.1"
port = 6380

[auth]
enabled = true
password = "replace-me"

[engine]
shard_count = 4
max_memory_bytes = 268435456
max_keys = 100000

[eviction]
policy = "lru"
sample_size = 5

[ttl]
precision_ms = 1
maintenance_budget = 1024
```

启用认证后，通过环境变量 `LINGER_PASSWORD` 向 CLI 提供同一密码。配置规则与当前限制见 [config.md](docs/config.md)。

## 事件订阅

事件默认 Fast，缓冲溢出会丢弃并计数；跨重启消费需要在配置中加入：

```toml
[events]
mode = "reliable"

[event_journal]
dir = "journal"
max_bytes = 1073741824
```

目录相对配置文件解析，同目录只允许一个实例；复用要求数据库数与实际分片数一致，建议显式固定 `engine.shard_count`。Reliable 逐事务 fsync，容量不足拒绝新变更，不提前丢弃保留期内事件。

先在一个终端订阅，再在另一个写入：

```bash
cargo run -p linger-cli -- -c 'SUBSCRIBE order:* GROUP workers'
cargo run -p linger-cli -- -c 'SET order:1 null PX 60000'
```

输出首列为 `stream/db/shard/sequence`，处理成功后在订阅终端输入 `ACK <首列ID>`。`UNSUBSCRIBE` 退出并保留命名组；Ctrl+C / 断线后重连同组可重投未确认事件。需要输出即确认时显式加 `--auto-ack`，例如 `--count 1 --auto-ack -c 'SUBSCRIBE order:* GROUP workers'`。Rust 使用 `Client::subscribe`、`Subscriber::next_event` 与显式 `ack(event.id)`，见 [SDK](docs/sdk.md)。

当前维护约每秒检查、按完整事务前缀压缩，消费顺序扫描磁盘；未测量其吞吐，不作为生产性能保证。

## 验证

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

测试覆盖独立黄金字节、10 种 Value、数值与容器边界、失败无部分修改、资源限额、跨分片原子观察、锁顺序、跨库身份与期限保留、游标弱一致性、淘汰失败原子性、异步回收计费、可控时钟下的 TTL 边界 / 校时 / 续期 / 旧任务校验 / 过期竞争，以及真实 TCP / CLI 调用。源码、测试和基准工具使用中英双语注释；公开接口缺少 rustdoc 会使构建失败。Journal 测试包含同步失败、截断、校验损坏、容量与真实进程强制终止恢复，消费覆盖 ACK 空洞、组与租约、TCP 半帧和 CLI 显式确认。共享协议数据位于 [tests/fixtures/protocol](tests/fixtures/protocol)。

Linux 下运行 `python3 benchmarks/run.py target/benchmarks/my-run` 可重现 release 引擎和 TCP 的 GET / SET / 混合负载，每项默认三次；输出目录必须不存在。实验使用独立进程和临时端口，记录硬件、配置、种子、错误、分位数、CPU 与 RSS。阶段 3 的历史实测不包含本阶段新增的到期索引计费与维护开销，结果及边界见 [测试与基准](docs/testing.md#阶段-3-基础测量)。

## 从哪里开始

先读 [架构与范围](docs/architecture.md)，再按任务阅读专项规范：

| 主题 | 文档 |
|---|---|
| 值类型与命令 | [数据模型](docs/data-model.md) · [命令契约](docs/commands.md) |
| 网络与客户端 | [协议](docs/protocol.md) · [CLI 与 SDK](docs/sdk.md) |
| 生命周期与可靠性 | [TTL](docs/ttl.md) · [事件](docs/events.md) · [持久化边界](docs/persistence.md) |
| 运维与配置 | [配置规范](docs/config.md) |
| 实施与验收 | [路线图](docs/roadmap.md) · [测试与基准](docs/testing.md) |
| 设计取舍 | [决策记录](docs/decisions.md) |
| 仓库协作规则 | [AGENTS.md](AGENTS.md) |

首个稳定版聚焦单机缓存服务，不包含 Redis 兼容、Embedded API、多节点集群或缓存数据恢复。Reliable 指已落盘事件在保留期内可恢复，**不表示缓存值持久化**。

CLI 优先，并与 Rust SDK 共享实现；Java、Go 实现相同协议。性能提升是待测目标，目前不宣称优于 Redis 或生产可用。

后续实现与阶段验收见 [路线图](docs/roadmap.md)，完整 v1.0 契约仍以专项文档为准。
