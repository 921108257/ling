# linger-cache-db

独立部署的 Rust 内存缓存数据库：自定义二进制协议、10 种强类型、固定分片和多数据库；毫秒 TTL 与可靠事件按路线图继续实现。

**当前为 0.1.0 阶段 3 固定分片原型。** 已实现共享类型、版本 1 编解码、数据引擎、TCP 服务、Rust SDK 和 CLI。支持十种 Value、隔离数据库、数值与容器命令，以及跨库复制 / 移动、重命名、遍历、清库与数据库交换；清单见 [命令契约](docs/commands.md)。

每个 (数据库, Shard) 拥有独立索引与锁，多 Key 命令按固定顺序取得全部执行权后整体发布。可配置全实例受管内存、每库 Key 配额及 noeviction / lru / lfu / random / ttl-first 策略；受管内存不等于进程 RSS。启用淘汰且配置容量上限时，当前写入保守取得全实例执行权。ttl-first 的候选选择已实现，但公开 TTL 设置仍留到阶段 4，无期限候选时拒绝写入。

尚不支持 TTL 设置与调度、事件、TLS、INFO / CONFIG / SHUTDOWN 和 Java / Go SDK。Batch 仅有编码支持；尚未实现的命令返回 `ERR_UNKNOWN_COMMAND`，SET 的 TTL 选项返回 `ERR_INVALID_ARGUMENT`。

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
cargo run -p linger-cli -- -c 'COPY counter copied DB 1'
cargo run -p linger-cli -- --db 1 -c 'SCAN 0 COUNT 100'
cargo run -p linger-cli -- -c 'SET profile map({"name": "Lin", "visits": uint(1)})'
cargo run -p linger-cli -- -c 'FGET profile name visits'
printf 'SET nothing null\nGET nothing\nQUIT\n' | cargo run -p linger-cli
```

PING 输出 `"PONG"`，INCRBY 输出 `int(2)`；存储的 Null 输出 `null`，缺失 Key 则以独立错误和非零退出码表达。CLI 连接时自动完成 HELLO、可选 AUTH 和 SELECT；输入中的任何失败都会使最终退出码非零。Ctrl-C / SIGTERM 停止服务并关闭连接。SDK 不自动重放请求，发送后的断线或超时可通过 `outcome_unknown()` 判断不确定性。

无 `--config` 时服务端尝试读取当前目录的 `linger.toml`，不存在则使用默认配置。阶段 3 接受 `server`、`auth`、`database`、`protocol`、`engine`、`eviction` 六组，完整 v1.0 候选配置暂不能整份使用。可通过 `--config <path>` 指定文件，例如：

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
```

启用认证后，通过环境变量 `LINGER_PASSWORD` 向 CLI 提供同一密码。配置规则与当前限制见 [config.md](docs/config.md)。

## 验证

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

测试覆盖独立黄金字节、10 种 Value、数值与容器边界、失败无部分修改、资源限额、跨分片原子观察、锁顺序、跨库身份与期限保留、游标弱一致性、淘汰失败原子性、异步回收计费，以及真实 TCP / CLI 调用。源码、测试和基准工具使用中英双语注释；公开接口缺少 rustdoc 会使构建失败。共享协议数据位于 [tests/fixtures/protocol](tests/fixtures/protocol)。

Linux 下运行 `python3 benchmarks/run.py target/benchmarks/my-run` 可重现 release 引擎和 TCP 的 GET / SET / 混合负载，每项默认三次；输出目录必须不存在。实验使用独立进程和临时端口，记录硬件、配置、种子、错误、分位数、CPU 与 RSS。阶段 3 实测及边界见 [测试与基准](docs/testing.md#阶段-3-基础测量)。

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
