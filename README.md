# linger-cache-db

独立部署的 Rust 内存缓存数据库：自定义二进制协议、10 种强类型。已具备单分片多数据库存储；固定多分片、毫秒 TTL 与可靠事件按路线图继续实现。

**当前为 0.1.0 阶段 2 单分片内存原型。** 已实现共享类型、版本 1 编解码、数据引擎、TCP 服务、Rust SDK 和 CLI。支持十种 Value、隔离数据库、通用读写、数值 / String / Bytes 操作及全部 List / Map / Set 命令；清单见 [命令契约](docs/commands.md)。

当前所有数据库共享一个引擎执行锁，多 Key 变更整体发布；受管内存和每库 Key 数可配置，达到预算拒绝扩大内存的写入。尚不支持多分片调度、跨库移动、遍历、清库管理、TTL 设置与调度、事件、TLS 和 Java / Go SDK。Batch 仅有编码支持；尚未实现的命令返回 `ERR_UNKNOWN_COMMAND`，SET 的 TTL 选项返回 `ERR_INVALID_ARGUMENT`。

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
cargo run -p linger-cli -- -c 'SET profile map({"name": "Lin", "visits": uint(1)})'
cargo run -p linger-cli -- -c 'FGET profile name visits'
printf 'SET nothing null\nGET nothing\nQUIT\n' | cargo run -p linger-cli
```

PING 输出 `"PONG"`，INCRBY 输出 `int(2)`；存储的 Null 输出 `null`，缺失 Key 则以独立错误和非零退出码表达。CLI 连接时自动完成 HELLO、可选 AUTH 和 SELECT；输入中的任何失败都会使最终退出码非零。Ctrl-C / SIGTERM 停止服务并关闭连接。SDK 不自动重放请求，发送后的断线或超时可通过 `outcome_unknown()` 判断不确定性。

无 `--config` 时服务端尝试读取当前目录的 `linger.toml`，不存在则使用默认配置。阶段 2 接受 `server`、`auth`、`database`、`protocol`、`engine` 五组，完整 v1.0 候选配置暂不能整份使用。可通过 `--config <path>` 指定文件，例如：

```toml
[server]
host = "127.0.0.1"
port = 6380

[auth]
enabled = true
password = "replace-me"

[engine]
shard_count = 1
max_memory_bytes = 268435456
max_keys = 100000
```

启用认证后，通过环境变量 `LINGER_PASSWORD` 向 CLI 提供同一密码。配置规则与当前限制见 [config.md](docs/config.md)。

## 验证

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

测试覆盖独立黄金字节、10 种 Value、数值边界、容器操作、原子失败回滚、深度 / 内存 / 响应限额、数据库隔离、多连接竞争、半包粘包、版本拒绝、连接屏障、请求 ID、超时与取消，以及真实 CLI 进程的网络调用。源码与测试使用中英双语注释；公开接口缺少 rustdoc 会使构建失败。共享协议数据位于 [tests/fixtures/protocol](tests/fixtures/protocol)。

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
