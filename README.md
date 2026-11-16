# ling

ling（原 linger-cache-db）是独立部署的 Rust 内存缓存数据库：自定义二进制协议、10 种强类型、固定分片和多数据库；已支持毫秒 TTL、Fast 事件与可选 Reliable Journal。

**当前为 0.1.1 预发布版本，已完成阶段 7 本机性能与资源验收。** 已实现共享类型、版本 1 编解码、数据引擎、TCP / TLS 服务、Rust / Java / Go SDK 和 CLI。支持十种 Value、隔离数据库、数值与容器命令，以及跨库复制 / 移动、重命名、遍历、清库、数据库交换及完整 TTL 生命周期；清单见 [命令契约](docs/commands.md)。

每个 (数据库, Shard) 拥有独立索引与锁，多 Key 命令按固定顺序取得全部执行权后整体发布。可配置全实例受管内存、每库 Key 配额及 noeviction / lru / lfu / random / ttl-first 策略；受管内存不等于进程 RSS。启用淘汰且配置容量上限时，当前写入保守取得全实例执行权。ttl-first 选择最早到期的活跃条目，无期限候选时拒绝写入。

已支持 SET EX / PX / EXAT / PXAT、EXPIRE 系列、PERSIST 和四种 TTL 查询；逻辑到期不依赖后台回收。默认启用有界 Fast 事件，可选 Reliable 持久化事件与消费元数据；缓存和 TTL 重启后仍为空。支持验证证书的 TLS / 双向 TLS、三种 SDK 的固定库连接池、普通 Batch、INFO / CONFIG / SHUTDOWN、日志和可选 Prometheus。单连接串行执行，并发通过多连接提供；普通 Batch 不提供事务或整体回滚。

## 下载安装包

在 [Releases](https://github.com/921108257/ling/releases) 下载预览安装包；私有仓库需要访问权限。新的包名统一为 `ling_<版本>_<平台>_<架构>`，包含服务端 `ling`、客户端 `ling-cli`、配置示例以及安装/CLI/配置/持久化文档。既有 `0.1.0` Release 中的旧名资产不会因源码更名自动更新。

| 平台 | 架构 | 安装包与服务 |
|---|---|---|
| Windows | x86_64 | Setup EXE 默认注册并启动 `ling` 服务；ZIP 为便携包 |
| macOS | Intel x86_64、Apple Silicon ARM64 | PKG 注册 LaunchDaemon；tar.gz 为便携包 |
| Linux | x86_64、ARM64 | DEB / RPM 注册 systemd；AppImage / tar.gz 为便携包 |

完整安装、服务管理、升级与卸载见 [安装指南](docs/install.md)。安装器保留已有配置，卸载保留用户配置和 Journal。便携包不会自动注册服务。发布页的 `SHA256SUMS` 用于校验下载文件；安装器尚未完成 Windows 签名与 macOS 公证。

安装服务后，在新终端验证：

```sh
ling --help
ling-cli --help
ling-cli --cheatsheet
ling-cli -c 'HELP SET'
ling-cli -c 'PING'
```

`ling` 是服务端入口，`ling-cli` 是数据库命令行。帮助和速查均可离线使用；交互终端输入 `HELP` 或 `HELP <命令>` 即可查阅，无需另开浏览器。完整用法与命令速查见 [CLI 指南](docs/cli.md)。

帮助自动选择简体中文、美式英文、韩语、日语、俄语、德语或法语，未匹配时默认美式英语。优先级为 `--lang` → `LING_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANG` → 本机语言。例如 `ling --lang ja-JP --help`、`ling-cli --lang zh-CN --cheatsheet`。语言选择只影响本地帮助，不改变协议或稳定错误码。

**Windows 当前仅支持 Fast 事件；Reliable 配置在文件操作和监听前返回 `ERR_CONFIG_INVALID`，不会降级为 Fast。** Linux / macOS 保留 Reliable 支持；其恢复边界仍是已提交事件和消费元数据，缓存值与 TTL 不恢复。

[GitHub Actions](.github/workflows/release.yml) 在 `v*` 标签、手动触发和面向 `main` 的 PR 上执行检查与原生打包；标签必须与 `Cargo.toml` 版本一致，只有标签构建全部成功后才创建预发布 Release。手动/PR 构建产物保留 14 天，Release 资产长期保留。本次版本标签为 `v0.1.1`，对应 GitHub Prerelease；后续发布同样先更新版本，再推送匹配标签。

## 构建与试用

需要 Rust 2024 工具链，Cargo 声明最低版本为 1.85。以下命令在仓库根目录执行：

```sh
cargo build --workspace
cargo run -p ling-server --bin ling
```

服务端默认监听 `127.0.0.1:6380`。另开终端调用 CLI：

```sh
cargo run -p ling-cli -- -c 'PING'
cargo run -p ling-cli -- -c 'SET counter int(0)'
cargo run -p ling-cli -- -c 'INCRBY counter 2'
cargo run -p ling-cli -- -c 'SET session null PX 5000 NX'
cargo run -p ling-cli -- -c 'PTTL session'
cargo run -p ling-cli -- -c 'PERSIST session'
cargo run -p ling-cli -- -c 'COPY counter copied DB 1'
cargo run -p ling-cli -- --db 1 -c 'SCAN 0 COUNT 100'
cargo run -p ling-cli -- -c 'SET profile map({"name": "Lin", "visits": uint(1)})'
cargo run -p ling-cli -- -c 'FGET profile name visits'
printf 'SET nothing null\nGET nothing\nQUIT\n' | cargo run -p ling-cli
```

PING 输出 `"PONG"`，INCRBY 输出 `int(2)`；存储的 Null 输出 `null`，缺失 Key 则以独立错误和非零退出码表达。CLI 连接时自动完成 HELLO、可选 AUTH 和 SELECT；输入中的任何失败都会使最终退出码非零。Ctrl-C / SIGTERM 停止服务并关闭连接。SDK 不自动重放请求，发送后的断线或超时可通过 `outcome_unknown()` 判断不确定性。

无 `--config` 时服务端优先读取当前目录的 `ling.toml`，没有时兼容旧名 `linger.toml`，均不存在则使用默认配置。系统服务使用安装指南中的固定配置路径。已接受 `server`、`tls`、`auth`、`database`、`engine`、`eviction`、`ttl`、`events`、`event_journal`、`protocol`、`log`、`metrics` 十二组配置，完整默认值见 [config.md](docs/config.md)。可通过 `--config <path>` 指定文件，例如：

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

启用认证后，通过环境变量 `LING_PASSWORD` 向 CLI 提供同一密码；未设置时兼容 `LINGER_PASSWORD`。配置规则与当前限制见 [config.md](docs/config.md)。

## 批量、TLS 与运维

`--batch` 将标准输入提交为一个协议 Batch，最多 1000 条、64 MiB 文本；任一结果失败时退出码非零。默认多行输入仍为逐行独立请求。Batch 不允许连接、管理、订阅和遍历命令，准入失败整批不执行：

```sh
printf 'SET first null\nGET first\nGET absent\n' | cargo run -p ling-cli -- --batch
cargo run -p ling-cli -- -c 'INFO events'
cargo run -p ling-cli -- -c 'CONFIG GET log.*'
cargo run -p ling-cli -- -c 'CONFIG SET log.level debug'
cargo run -p ling-cli -- -c 'CONFIG RELOAD'
cargo run -p ling-cli -- -c 'SHUTDOWN'
```

RELOAD 从启动配置文件重新加载，仅允许日志级别变化；任何静态项变化整体拒绝。没有配置来源时不能 RELOAD。CONFIG GET 隐藏密码；INFO 返回类型化 Map，受管内存与 RSS 分开。

启用 TLS 时配置服务端身份，文件路径相对配置文件；`ca_file` 非空时还要求客户端证书：

```toml
[tls]
enabled = true
cert_file = "certs/server.pem"
key_file = "certs/server-key.pem"
min_version = "tls1.2"

[log]
level = "info"
format = "json"
output = "stdout"

[metrics]
enabled = true
host = "127.0.0.1"
port = 9090
path = "/metrics"
```

```sh
cargo run -p ling-cli -- --tls-server-name cache.example.com --tls-ca certs/ca.pem -c 'PING'
```

省略 `--tls-ca` 时使用内置公共根；双向 TLS 另加成对的 `--tls-cert` 和 `--tls-key`。始终验证服务端名称与证书。指标为独立 HTTP 端点，默认关闭；启用后可在 `http://127.0.0.1:9090/metrics` 读取请求、时延、内存、TTL、事件及 Journal 指标，不带 Key / Value 标签。

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
cargo run -p ling-cli -- -c 'SUBSCRIBE order:* GROUP workers'
cargo run -p ling-cli -- -c 'SET order:1 null PX 60000'
```

输出首列为 `stream/db/shard/sequence`，处理成功后在订阅终端输入 `ACK <首列ID>`。`UNSUBSCRIBE` 退出并保留命名组；Ctrl+C / 断线后重连同组可重投未确认事件。需要输出即确认时显式加 `--auto-ack`，例如 `--count 1 --auto-ack -c 'SUBSCRIBE order:* GROUP workers'`。Rust 使用 `Client::subscribe`、`Subscriber::next_event` 与显式 `ack(event.id)`，见 [SDK](docs/sdk.md)。

当前维护约每秒检查、按完整事务前缀压缩；消费从运行时记录游标继续顺序扫描，压缩和重启后按逻辑水位恢复。[阶段 7 实测](docs/testing.md#阶段-7-本机验收)包含消费积压、Fast 丢弃及 Reliable 成本，不作为生产性能保证。

## 验证

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
python3 tests/sdk_contracts.py
```

最后一项需要 Go 1.22+、JDK 17、OpenSSL 和 Python 3：自动构建 Rust 服务端，分别在 TCP 与双向 TLS 下运行 Go race 检查和 Java 契约。Java 使用 `javac -Xlint:all -Werror`，不依赖 Maven / Gradle；Go 仅使用标准库，SDK 入口与独立编译方式见 [sdk.md](docs/sdk.md)。测试 TLS 身份仅供本地契约使用。

测试覆盖独立黄金字节、10 种 Value、数值与容器边界、失败无部分修改、资源限额、跨分片原子观察、锁顺序、跨库身份与期限保留、游标弱一致性、淘汰失败原子性、异步回收计费、可控时钟下的 TTL 边界 / 校时 / 续期 / 旧任务校验 / 过期竞争，以及真实 TCP / CLI 调用。源码、测试和基准工具使用中英双语注释；公开接口缺少 rustdoc 会使构建失败。Journal 测试包含同步失败、截断、校验损坏、容量与真实进程强制终止恢复，消费覆盖 ACK 空洞、组与租约、TCP 半帧和 CLI 显式确认。共享协议数据位于 [tests/fixtures/protocol](tests/fixtures/protocol)。

Linux 下运行 `python3 benchmarks/run.py target/benchmarks/my-run` 可重现 release 引擎和 TCP 的 GET / SET / 混合负载，每项默认三次；输出目录必须不存在。实验使用独立进程和临时端口，记录硬件、配置、种子、错误、分位数、CPU 与 RSS。阶段 3 的历史实测不包含后来新增的 TTL、事件、TLS 与运维开销，结果及边界见 [测试与基准](docs/testing.md#阶段-3-基础测量)。

阶段 7 提供下列实际入口，输出目录同样必须不存在：

```sh
# 18 类场景各 3 次，并追加 Fast / Reliable 各 600 秒持续运行
python3 benchmarks/stage7.py target/benchmarks/stage7
# 只运行持续实验，可显式改变时长（1—3600 秒）
python3 benchmarks/stage7.py target/benchmarks/soak --soak-only --soak-seconds 600
# Fast / Reliable 各 30 轮真实 TCP 的 TTL 可见性与回收观察
python3 benchmarks/lifecycle.py target/benchmarks/ttl
```

工具保存固定二进制副本、源码摘要、完整配置、请求延迟直方图及 CPU / RSS / Journal 采样。支持 `--only get,mixed,reliable-consumer` 筛选场景；筛选后不自动追加持续运行。仅复测一种持续负载时使用 `--soak-only --soak-mode reliable`（或 `fast`）。实际数据、消费落后及环境限制见 [阶段 7 本机验收](docs/testing.md#阶段-7-本机验收)。十分钟持续运行不等于多日压力或稳定版发布认证。

## 从哪里开始

先读 [架构与范围](docs/architecture.md)，再按任务阅读专项规范：

| 主题 | 文档 |
|---|---|
| 值类型与命令 | [数据模型](docs/data-model.md) · [命令契约](docs/commands.md) |
| 网络与客户端 | [协议](docs/protocol.md) · [CLI 与 SDK](docs/sdk.md) |
| 生命周期与可靠性 | [TTL](docs/ttl.md) · [事件](docs/events.md) · [持久化边界](docs/persistence.md) |
| 运维与配置 | [安装与服务](docs/install.md) · [CLI 指南](docs/cli.md) · [配置规范](docs/config.md) |
| 实施与验收 | [路线图](docs/roadmap.md) · [测试与基准](docs/testing.md) |
| 设计取舍 | [决策记录](docs/decisions.md) |
| 仓库协作规则 | [AGENTS.md](AGENTS.md) |

首个稳定版聚焦单机缓存服务，不包含 Redis 兼容、Embedded API、多节点集群或缓存数据恢复。Reliable 指已落盘事件在保留期内可恢复，**不表示缓存值持久化**。

CLI 优先，并与 Rust SDK 共享实现；Java、Go 实现相同协议。性能提升是待测目标，目前不宣称优于 Redis 或生产可用。

后续实现与阶段验收见 [路线图](docs/roadmap.md)，完整 v1.0 契约仍以专项文档为准。
