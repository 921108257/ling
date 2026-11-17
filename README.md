# ling

ling 是独立部署的内存缓存数据库，使用 Rust 编写，支持 10 种强类型值、毫秒 TTL、多数据库和事件订阅。服务端命令为 `ling`，客户端为 `ling-cli`。

[官网](https://921108257.github.io/ling/) · [快速上手](https://921108257.github.io/ling/docs/quickstart/) · [安装与服务](https://921108257.github.io/ling/docs/install/) · [CLI 指南](https://921108257.github.io/ling/docs/cli/) · [命令速查](https://921108257.github.io/ling/commands/)

当前版本为 **0.1.4 预发布版**，项目自有代码采用 [MIT 许可证](LICENSE)。缓存值与 TTL 只保存在内存中，重启后为空。Reliable 保存的是事件及消费进度；Windows 支持 Fast 模式，Linux / macOS 可使用 Reliable。

## 安装

从 [Releases](https://github.com/921108257/ling/releases) 下载匹配系统和 CPU 架构的安装包。使用同页的 `SHA256SUMS` 校验下载文件。

| 平台 | 架构 | 安装包 | 便携包 |
|---|---|---|---|
| Windows | x86_64 | Setup EXE | ZIP |
| macOS | Intel x86_64、Apple Silicon ARM64 | PKG | tar.gz |
| Linux | x86_64、ARM64 | DEB、RPM | AppImage、tar.gz |

安装器默认注册并启动系统服务；升级保留已有配置，卸载保留用户配置和状态。便携包需要手动启动服务。Windows 安装器未签名，macOS PKG 未签名或公证。服务管理、配置路径、升级和卸载步骤见 [安装指南](docs/install.md)。

## 第一次读写

安装系统服务后，打开新终端连接。使用便携包时，先在另一个终端运行包内的 `ling` 可执行文件。

```sh
ling-cli -c 'PING'
ling-cli
```

PING 返回 `"PONG"`。进入 `ling>` 提示符后输入：

```text
SET greeting "Hello, ling"
GET greeting
SET counter int(0)
INCRBY counter 2
SET session null PX 5000 NX
PTTL session
PERSIST session
HELP SET
QUIT
```

`GET greeting` 返回 `"Hello, ling"`，`INCRBY` 返回 `int(2)`。Value 保留类型：`null` 是成功值，Key 不存在则返回 `ERR_KEY_NOT_FOUND`。容器需要先用 SET 创建，类型专用命令不会隐式创建 Key。

默认地址为 `127.0.0.1:6380`、数据库为 `0`。连接其他实例或数据库：

```sh
ling-cli --host 127.0.0.1 --port 6380 --db 1
```

CLI 的 `--host` 接受 IP 地址，TLS 证书名称通过 `--tls-server-name` 指定。执行错误会使退出码非零；写入发送后的断线或超时可能表示结果未知，应先核实结果再决定是否重试。

## 在终端查帮助

帮助可以离线查看：

```sh
ling --help
ling-cli --help
ling-cli help
ling-cli help containers list
ling-cli help SET examples
ling-cli --cheatsheet
```

`HELP` 显示简短主题目录，`HELP containers` 继续查看容器分类，`HELP containers list` 列出 List 命令及简述。`HELP SET` 展示完整命令说明，`HELP SET examples` 只看示例；`syntax`、`options`、`related`、`description`、`types` 可单独查看对应部分。终端外可用 `ling-cli help SET examples` 或 `ling-cli --help SET examples`，完整命令参考使用 `--cheatsheet`。帮助和诊断支持简体中文、美式英语、韩语、日语、俄语、德语和法语；按 `--lang` → `LING_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANG` → 本机语言选择，未匹配时使用美式英语。例如 `ling-cli --lang zh-CN --help`。

## 更新

从 0.1.3 起可直接检查和安装更新：

```sh
ling update --check
ling update
ling update --version 0.1.4
ling update --stable
```

默认包含预发布版，`--stable` 只选择正式版；指定版本不能降级。Linux / macOS 的系统安装包使用 `sudo ling update` 安装更新，检查更新不需要 sudo。**0.1.2 尚无内置更新，请先手动安装 0.1.3。**

Windows 会退出当前命令并交给后台更新进程，终端显示的 `result.json` 路径仅表示已安排更新；查看该文件的完成结果，再运行 `ling --version` 确认版本。便携更新替换 `ling` 和 `ling-cli`，保留配置；手动启动的进程需自行停止并重新启动。详细步骤见 [安装指南](docs/install.md)。

## 配置与认证

前台运行 `ling` 时，优先读取当前目录的 `ling.toml`，其次读取兼容文件 `linger.toml`，均不存在则使用默认配置。也可显式指定：

```sh
ling --config /path/to/ling.toml
```

系统服务使用固定配置路径，见 [安装指南](docs/install.md)。配置示例：

```toml
[server]
host = "127.0.0.1"
port = 6380

[auth]
enabled = true
password = "replace-with-your-password"

[engine]
shard_count = 4
max_memory_bytes = 268435456
max_keys = 100000
```

认证启用后，在客户端环境中设置 `LING_PASSWORD`，未设置时兼容 `LINGER_PASSWORD`。CLI 不接受密码命令行参数。TLS、淘汰、日志与 Prometheus 配置见 [配置参考](docs/config.md)；只有日志级别可以动态修改，其他配置需要重启。

## 选择事件模式

默认 Fast 使用有界内存队列，溢出时可能丢弃事件。Linux / macOS 可配置 Reliable，将已提交事件和命名消费组进度保存在 Journal 中：

```toml
[events]
mode = "reliable"

[event_journal]
dir = "journal"
max_bytes = 1073741824
```

相对目录按配置文件位置解析。Reliable 需要支持文件与目录同步的文件系统，默认保留 7 天；可能重复投递，业务处理成功后显式 ACK。窗口外的消费位置会报错，缓存值与 TTL 不随 Journal 恢复。Windows 的 Reliable 配置返回 `ERR_CONFIG_INVALID`。完整用法见 [事件订阅](docs/events.md) 和 [持久化](docs/persistence.md)。

## 使用参考

| 主题 | 文档 |
|---|---|
| 命令与类型 | [CLI 指南](docs/cli.md) · [命令参考](docs/commands.md) · [数据模型](docs/data-model.md) |
| 过期与事件 | [TTL](docs/ttl.md) · [订阅与 ACK](docs/events.md) · [持久化](docs/persistence.md) |
| 部署与运维 | [安装与服务](docs/install.md) · [配置](docs/config.md) |
| 应用接入 | [Rust、Java、Go SDK](docs/sdk.md) · [通信协议](docs/protocol.md) |

ling 使用自定义二进制协议，不兼容 Redis；仅支持独立单机服务，不提供 Embedded API、集群、复制或缓存数据恢复。项目原名为 linger-cache-db，Java 包 `io.linger.cache` 和 Go 模块 `linger.local/cache` 保留兼容名称。

<details>
<summary>贡献者：源码构建、测试与文档维护</summary>

需要 Rust 2024 工具链，最低 Rust 1.85。在仓库根目录构建并启动服务：

```sh
cargo build --workspace
cargo run -p ling-server --bin ling
```

另一个终端运行 `cargo run -p ling-cli`。仓库检查命令：

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
python3 tests/sdk_contracts.py
```

跨语言测试还需要 Go 1.22+、JDK 17、OpenSSL 和 Python 3。测试入口、性能数据与测量条件见 [测试与基准](docs/testing.md)；协作前阅读 [AGENTS.md](AGENTS.md)、[架构](docs/architecture.md)、[路线图](docs/roadmap.md) 和 [决策记录](docs/decisions.md)。

官网需要 Node.js 24，在仓库根目录执行：

```sh
cd site
npm ci
npm run build
npm run check
```

静态文件输出到 `site/dist`，线上基础路径为 `/ling/`。[Pages 工作流](.github/workflows/pages.yml) 检查并部署官网；[发行工作流](.github/workflows/release.yml) 对匹配 Cargo 版本的标签构建安装包并创建预发布 Release。

</details>
