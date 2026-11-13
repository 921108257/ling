# linger-cache-db

独立部署的 Rust 内存缓存数据库：自定义二进制协议、10 种强类型。固定分片、多数据库存储、毫秒 TTL 与可靠事件按路线图逐步实现。

**当前为 0.1.0 阶段 1 网络原型，尚不是可用缓存数据库。** 已实现共享类型、版本 1 编解码、TCP 服务、Rust SDK 和 CLI；服务端支持 HELLO、AUTH、SELECT、PING、QUIT。未知或尚未实现的命令返回 `ERR_UNKNOWN_COMMAND`。没有数据引擎、TTL、事件运行能力、TLS、Java / Go SDK，Batch 仅完成编码用例。

## 构建与试用

需要 Rust 2024 工具链，Cargo 声明最低版本为 1.85。以下命令在仓库根目录执行：

```sh
cargo build --workspace
cargo run -p linger-server
```

服务端默认监听 `127.0.0.1:6380`。另开终端调用 CLI：

```sh
cargo run -p linger-cli -- -c 'PING'
cargo run -p linger-cli -- --db 15 -c 'PING "你好"'
printf 'SELECT 1\nPING\nQUIT\n' | cargo run -p linger-cli
```

PING 分别输出 `"PONG"` 和 `"你好"`。CLI 连接时自动完成 HELLO、可选 AUTH 和 SELECT；输入中的任何失败都会使最终退出码非零。Ctrl-C / SIGTERM 停止服务并关闭连接。SDK 不自动重放请求，发送后的断线或超时可通过 `outcome_unknown()` 判断不确定性。

无 `--config` 时服务端尝试读取当前目录的 `linger.toml`，不存在则使用默认配置。阶段 1 仅接受 `server`、`auth`、`database`、`protocol` 四组，完整 v1.0 候选配置暂不能整份使用。可通过 `--config <path>` 指定文件，例如：

```toml
[server]
host = "127.0.0.1"
port = 6380

[auth]
enabled = true
password = "replace-me"
```

启用认证后，通过环境变量 `LINGER_PASSWORD` 向 CLI 提供同一密码。配置规则与当前限制见 [config.md](docs/config.md)。

## 验证

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

测试覆盖独立黄金字节、10 种 Value、错误输入、半包粘包、版本拒绝、连接屏障、请求 ID、超时与取消，以及真实 CLI 进程的网络调用。共享协议数据位于 [tests/fixtures/protocol](tests/fixtures/protocol)。

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
