# 验证与性能基准

> 状态：阶段 1—7 已建立正确性契约、本机负载矩阵、资源耗尽及各 10 分钟持续运行证据。阶段 3 为历史基线，阶段 7 实测见下文；稳定发布仍需目标环境评估，要求矩阵不能直接视为已通过清单。

当前实际入口为 `cargo fmt --all -- --check`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 和 `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。类型、协议、引擎、服务端、Rust SDK、CLI 的测试分别位于各 crate 的 `tests` 中；分片内部所有权契约另在 `crates/engine/src/ownership_tests.rs`，共享字节文件在 [tests/fixtures/protocol](../tests/fixtures/protocol)。CLI 端到端测试启动真实 TCP 服务并运行编译后的 CLI 进程。已覆盖全部数据类型、数值与容器边界、失败无部分修改、generation / revision、容量限制、多连接竞争、跨分片独立推进 / MSET / SWAPDB 原子观察、反向锁序、跨库迁入、游标绑定及弱一致性、淘汰失败原子性、异步回收预算和超限 POP 保留数据。TTL 契约位于 `crates/engine/src/ttl_tests.rs`，覆盖可控双时钟、严格到期边界、墙钟跳变、版本与期限溢出、1000 次续期、每轮回收预算、迁移与清库、旧任务逐字段校验，以及到期和 NX / XX / DEL / COPY / SCAN 的并发竞争。服务端另验证空闲回收、等待许可时关闭，以及真实 TCP / CLI 生命周期。事件契约位于 `crates/events/src/tests.rs`、`crates/engine/tests/events.rs`，覆盖独立磁盘向量、六类生命周期、Fast 丢弃、ACK 空洞、组隔离、租约、保留与容量、目录锁、CRC / 拓扑、重复序号、截断多分片事务、真实只读目录、Linux /dev/full 写满错误和模拟 fsync 错误。`crates/server/tests/crash_recovery.rs` 强制终止真实服务子进程，验证 MSET 单个物理事务、ACK 恢复及缓存为空；网络验证半个 ACK 与推送交错，SDK / CLI 验证显式消费及取消。阶段 6 的 crates/server/tests/operations.rs 与 tls.rs 验证配置脱敏、重载整体拒绝、指标、关闭、Batch 准入及响应预算、证书与服务端名称校验。Rust SDK 覆盖池容量、固定库、取消及关闭。这些测试不等于物理断电、长期压力或生产验收。公开接口使用 missing_docs 强制 rustdoc，源码、测试及基准工具注释为中英双语。

## 三语言客户端验收

完整 Reliable 驱动适用于 Linux / macOS。Windows 发行任务运行 `cargo test --locked --target x86_64-pc-windows-msvc --workspace`，覆盖所有可移植契约；依赖 Reliable 的单项测试以 `cfg(unix)` 排除，清单及实际运行结果见下方。保留 Windows 明确拒绝 Reliable 且不创建目录的正向测试，不将排除的测试计为通过。

在仓库根目录执行：

```sh
python3 tests/sdk_contracts.py
```

需要 Python 3、Go 1.22+、JDK 17 与 OpenSSL。驱动先构建 Rust 服务端和独立契约 example，并以 `javac -Xlint:all -Werror` 编译 Java，然后分别启动 TCP 与双向 TLS 的真实服务端，每轮使用独立临时 Reliable Journal。Rust example、Go `go test -race -count=1` 和 Java `java -ea` 分别执行相同计划；结束清理服务进程和临时目录。测试中使用仓库内公开的测试证书与私钥，仅用于本地验证。

共享的 `values.tsv`、`invalid-values.tsv`、`frames.tsv` 验证字节、类型与错误，`commands.tsv` 验证真实服务端上的 Null / 缺失、最大 UInt、数值溢出不修改及 TTL 哨兵值。三语言在每个传输上还覆盖十种 Value 往返、INFO 字段、Batch 逐项错误、SELECT 后订阅数据库、断线重投稳定 ID、显式 ACK、订阅取消、固定库租约与关闭。每条夹具输出 `LING_CONTRACT` 记录，驱动以输入文件的 ID 集合核对执行，漏跑、重复、多报、失败或跳过均失败；表格单列计划、执行、失败、跳过和缺失。

独立运行 Go 测试时，没有 `LINGER_TEST_ADDR` 默认失败，并提示使用驱动；仅显式设置 `LINGER_TEST_SKIP_LIVE=1` 才允许离线跳过，输出仍报告跳过数，不能作为互操作验收。Java 缺地址也列出跳过并非零退出。`go vet ./...` 在 `sdks/go` 运行。Java 无 Maven / Gradle 工程，实际构建入口为 JDK。Rust 真实服务入口为 example，普通 workspace 测试不会把缺地址的提前返回计为该入口成功。

`sessions.tsv` 的 41 个本地对端用例在三语言分别运行并单列 `local-peer`：验证普通请求、Batch、订阅控制及空闲订阅中的旧非零响应、零或未来 ID、ERROR ID0、EVENT / HEARTBEAT，以及写出后断线、响应截断、发送前失败与固定库租约。监听器继续观察连接，检查没有隐式重连重放；它们不算作双向 TLS 验证。CLI 另用真实子进程与 Fast 命名组核对 stdin EOF、显式 ACK、auto-ack、输出失败和主动取消后的重投，而非只检查退出码。

Windows 可显式运行 `python3 tests/sdk_contracts.py --event-mode fast`；该模式不宣称验证 Reliable。没有 cgo 工具链时可追加 `--no-go-race` 做本地回归，输出注明关闭 race；默认命令和 Linux CI 继续开启 race。Unix 的 CLI 子进程接收真实 SIGINT；Windows 用内部选择测试区分 EOF 与取消，本轮未向 Windows 控制台发送信号。阶段 7 的持续运行与协议突变烟测不等于持续模糊、多日压力或物理断电验证。

## Windows 契约分类（v0.1.4）

Windows 支持 Fast，不提供 Reliable 所需的目录同步与原子替换路径。CI 在 Windows 原生目标运行整个 workspace，包含引擎所有权、TTL、容器、遍历、淘汰、Fast 事件、Rust SDK、服务端 TCP / TLS / 资源 / 运维及 CLI；不按 crate 或整份混合模式文件跳过。六类生命周期和消费可见性均拆为 Fast 与 Reliable 两项，Fast 在 Windows 继续执行。Journal 原始记录的三项只读 / 修尾单元测试直接使用临时文件，可在 Windows 运行；这不表示 Windows 已支持 Reliable。

仅以下测试要求 Unix；表中的名字位于所列文件，不含模块前缀。`crash_recovery.rs` 仅含一个 Reliable 测试，整文件门控；它把子进程 stderr 保存到测试临时文件，退出和就绪超时时连同退出状态输出，避免丢失启动失败原因。

| 测试文件 | Windows 排除的测试 | 原因 |
|---|---|---|
| `crates/events/src/tests.rs` | `reliable_recovery_keeps_holes_stream_and_sequences` | 需要恢复 Reliable 日志及持久 ACK 水位。 |
| 同上 | `sync_failure_is_unknown_and_globally_faults_writes_and_ack` | 注入 Journal fsync 失败并恢复未知提交。 |
| 同上 | `incomplete_tail_recovers_but_complete_corruption_and_wrong_topology_fail` | EventStore 启动恢复要求 Reliable 目录持久化。 |
| 同上 | `empty_named_group_is_rejected_during_recovery` | 从 Reliable 控制记录恢复并校验组状态。 |
| 同上 | `runtime_consumption_and_redelivery_preserve_corrupt_journal` | 在线修改 Reliable 日志，验证运行期读取保留证据。 |
| 同上 | `retention_compaction_and_capacity_backpressure_preserve_history` | 日志保留压缩需要原子替换与目录同步。 |
| 同上 | `interrupted_multi_shard_transaction_is_never_partly_recovered` | 从中断的 Reliable 多分片事务恢复。 |
| 同上 | `complete_duplicate_sequence_is_rejected_even_with_valid_checksum` | 从 Reliable 日志拒绝重复序号。 |
| 同上 | `ack_sync_failure_and_group_creation_failure_keep_unknown_outcomes` | 注入持久 ACK / 建组同步失败。 |
| 同上 | `maintenance_never_expires_timestamp_zero_before_retention` | 维护真实 Reliable 日志的保留前缀。 |
| 同上 | `read_only_journal_refuses_startup` | 使用 Unix 权限位设置只读 Journal，且需 Reliable 启动。 |
| 同上 | `stats_keep_drop_sync_failures_and_group_progress_separate` | 注入 Journal 同步失败并核对指标。 |
| 同上 | `reliable_consumer_visibility_waits_for_complete_publication` | Reliable 的发布屏障；对应 Fast 测试跨平台保留。 |
| 同上 | `sequential_reader_preserves_filtered_siblings_and_compaction_recovery` | 真实 Journal 游标、压缩和恢复。 |
| 同上 | `control_capacity_compaction_does_not_republish_stale_read_offset` | Journal 控制区容量触发的原子压缩。 |
| 同上 | `maintenance_only_rewrites_when_retention_can_advance` | 比较 Reliable 保留维护前后的文件并恢复。 |
| `crates/events/src/journal.rs` | `enospc_faults_the_journal_globally` | 仅 Linux，使用 `/dev/full` 注入内核写入拒绝。 |
| `crates/engine/tests/events.rs` | `six_kinds_revisions_noops_and_relocation_in_reliable_mode` | 六类生命周期的 Reliable 分支；Fast 分支保留。 |
| 同上 | `journal_backpressure_preserves_atomic_writes_and_expiry_accounting` | 真实 Journal 填满、清理与到期记账。 |
| `sdks/rust/tests/subscription.rs` | `reliable_ack_holes_recover_and_cache_starts_empty` | 服务端重启后恢复持久 ACK 空洞。 |
| `crates/server/tests/crash_recovery.rs` | `kill_preserves_atomic_journal_and_ack_hole_without_restoring_values` | 强制杀死 Reliable 服务进程并恢复。 |
| `crates/server/tests/resources.rs` | `journal_full_preserves_state_and_ack_capacity` | Reliable 磁盘预算耗尽及持久 ACK 预留。 |

Windows 仍执行 `windows_reliable_rejected_without_creating_directory`，同时检查 Fast 提交可用、Reliable 返回 `ERR_CONFIG_INVALID` 且不创建目录。Linux / macOS CI 保留全部本平台测试；Linux 的 `/dev/full` 契约不计为 macOS 通过项。

2026-11-17 在本机 Windows x86_64、rustc 1.94.0 上执行 `cargo test --locked --workspace --no-fail-fast`：182 项通过、0 失败、0 ignored（47 个测试目标及文档测试汇总）；上述平台条件排除的测试未编译，不计入 182 项。`cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps` 及 `npm --prefix site run build` / `check` 同机通过。该记录对应随后提交为 `601a1e9` 的 v0.1.4 首批实现工作区，不是 v0.1.3 历史结果。

同日 GitHub Actions 的 `Cross-platform packages` 运行 `44043386846` 对验证分支 `codex/v0.1.4-baseline`、完整提交 `601a1e944a843a5472b98b754af2d7f32da89c95` 执行成功。Linux `checks` 实际通过 fmt、workspace clippy（所有目标、拒绝警告）、严格 rustdoc、打包源码检查及 `python3 tests/sdk_contracts.py`；后者的 Go race / Java `-Xlint:all -Werror` 在 TCP 与双向 TLS 两轮通过。各包任务的测试数量由日志中所有 `test result: ok` 汇总：

| CI 平台 | 测试命令 | 通过 | 失败 / ignored | 构建与包验证 |
|---|---|---:|---|---|
| Linux x86_64 | `cargo test --locked --workspace` | 198 | 0 / 0 | 通过 |
| Linux aarch64 | `cargo test --locked --workspace` | 198 | 0 / 0 | 通过 |
| macOS x86_64 | `cargo test --locked --workspace` | 197 | 0 / 0 | 通过 |
| macOS aarch64 | `cargo test --locked --workspace` | 197 | 0 / 0 | 通过 |
| Windows x86_64 | `cargo test --locked --target x86_64-pc-windows-msvc --workspace` | 182 | 0 / 0 | 通过 |

Linux 比 macOS 多一项 `/dev/full` 契约；Windows 未编译上表列明的 Unix 条件测试，不把它们计为通过。Linux 测试使用宿主目标，包构建使用 musl 目标，本次尚未完成版本计划 7.3 的 musl 目标测试。非标签手动运行未执行 Release 发布；CI 包验证也不代替目标部署主机的安装、升级或物理断电验收。

## v0.1.4 发布候选验证

2026-11-17，最新实现提交 `40aa3a8ee4d07ac90a834acf466f4f07b631c525` 已在 Actions `44052477723` 全部通过：Linux x86_64 / aarch64 各 213 项、macOS x86_64 / aarch64 各 212 项、Windows x86_64 197 项；各平台均 0 失败、0 ignored。fmt、严格 clippy/rustdoc、三语言 TCP / 双向 TLS 契约、五个平台构建与原安装包验证通过。这次运行仍使用 0.1.3 元数据，未执行 Release 发布，不是最终 0.1.4 制品。

随后准备的 0.1.4 候选工作区在同一本机通过 `cargo fmt --all -- --check`、`cargo test --locked --workspace --no-fail-fast`（206 项，0 失败、0 ignored，47 个测试目标与文档测试汇总）、`cargo clippy --locked --workspace --all-targets -- -D warnings` 与严格 rustdoc。官网构建和检查通过：18 个生成页面，19 个 HTML、769 个本地链接、192 个搜索条目。日志为本机 `target/v0.1.4-release-tests.log`，未将日志作为发行制品。

新增验证覆盖：增量帧缓冲不按 64 MiB 广告提前分配；跨连接帧许可在完成、错误、取消和超时后归还；许可耗尽的握手前后等待受请求期限约束；命令名处理次数不随参数数增长；单许可下错误 AUTH 及已排队下一帧不会阻挡健康连接 HELLO/PING。已开始的阻塞任务在连接取消后继续持有参数对应许可；每连接只保留一个请求，认证失败比较完成后丢弃口令和许可再异步延迟。

接入故障测试另覆盖真实超限四条连接全部断开、内部拒绝计数为四且限频告警一次、原连接仍可 PING；实例专属指标 accept 故障连续三轮后业务仍可 PING；引擎及事件状态锁中毒触发维护退出。连接 panic 故障点改为实例隔离，所有钩子仅在测试编译。没有对真实内核监听器注入 EMFILE，也未做物理进程断电。

发行输入本机检查已实际运行：两个官方 appimagetool 1.9.1 下载内容的 SHA-256 与仓内固定摘要及 GitHub 资产摘要一致；真实 `sha256sum --check` 拒绝故意置零的摘要；实际 CI 特性守卫确认 dev 图有 failpoints、发行图无、故意启用时可检测。原仅显示依赖边的命令不能显示命令行启用特性，现使用 `{p} [{f}]` 同时检查解析后的特性集合。缺失 0.1.3 CHANGELOG 小节的演练明确失败，恢复后通过。九包 `cargo metadata` 均为 MIT，Windows 目标可生成 152 个依赖和 Rust 标准库的许可原文。

最终候选 `e60427e72b8bc9198a0662c8d641d7078db51985` 已在 Actions [`44056119224`](https://github.com/921108257/ling/actions/runs/44056119224) 全部通过，实际结果如下。工作流手动触发，Release 任务按条件跳过；标签发布另行执行。

| 平台 / 目标 | 测试范围 | 通过 | 失败 / ignored | 测试目标与文档测试汇总 |
|---|---|---:|---|---:|
| Linux x86_64 / aarch64，各自原生宿主 | 全 workspace | 各 222 | 均 0 / 0 | 各 47 |
| Linux x86_64 / aarch64，各自 musl 发行目标 | server / CLI | 各 101 | 均 0 / 0 | 各 18 |
| macOS x86_64 / aarch64，各自原生目标 | 全 workspace | 各 221 | 均 0 / 0 | 各 47 |
| Windows x86_64 原生目标 | 全 workspace，Fast 与明确拒绝 Reliable | 206 | 0 / 0 | 47 |

公共检查中的 fmt、严格 clippy / rustdoc 和 Java / Go TCP 与双向 TLS 契约全部通过；Rust 对应契约由各平台 workspace 测试覆盖。五个平台实际构建并验证 14 个包，解包后的便携服务均通过 PING、SET、GET、PTTL、SHUTDOWN 烟测；Linux AppImage 另通过统一烟测。两个 Linux 目标均实际拒绝故意错误的 appimagetool 摘要，正确摘要校验后才运行工具。各安装包、便携包与 AppImage 的项目 MIT 许可及第三方声明检查通过：macOS 各收集 151 个 crate、Linux 各 152 个、Windows 152 个，并附 Rust 标准库许可原文；Linux 另附 musl 声明，Windows 保留 WinSW 声明。日志为本机 `target/ci-44056119224.log`。

2026-11-17，标签 `v0.1.4` 指向 `b073054bca2202dfe21ce1210cc19531306f4b68`，仅比上述候选增加验收文档。标签 Actions [`44057670375`](https://github.com/921108257/ling/actions/runs/44057670375) 全部成功，测试数量与上表一致，并创建 [v0.1.4 Preview](https://github.com/921108257/ling/releases/tag/v0.1.4) 预发布。实际下载全部 15 个资产后，文件大小与 SHA-256 均匹配 GitHub 资产元数据，14 个包同时匹配 `SHA256SUMS`；五个便携归档内的 MIT 许可和第三方声明均通过检查。下载的 Windows 便携包在本机再次通过 PING、SET、GET、PTTL、SHUTDOWN 烟测。日志为本机 `target/ci-44057670375.log`，这些验证日志与下载文件不作为发行资产。

### 生产路径 panic 审阅

审阅排除测试模块及返回 Result 的 CLI 语法匹配方法 `Parser::expect`。可传播的错误在发布前返回：服务端本地参数/版本解析、SDK 已关闭连接、Journal 句柄/组进度/重投状态、MSET、SCAN、TTL/到期分片定位和候选存在性。新增缺失 Journal 句柄的回归确认返回 `ERR_JOURNAL_UNAVAILABLE`，不调用状态发布回调、提交计数及流尾不变。

| 保留的不变量 | 涉及位置与证据 |
|---|---|
| 已验证数据库与固定锁集合 | engine 的 transaction、administration；协调器预先取得分片执行权，fsync 后回调不再添加失败点。 |
| 持锁期间条目与候选仍存在 | engine 的 ttl、expiry、eviction；候选过滤及提交预检后不释放目标锁。 |
| 固定长度字节切片 | events/journal、protocol/codec；固定数组与已校验头部使切片恒为指定宽度，与字节内容无关。 |
| 内嵌静态目录、译文及 String 格式化 | cli-help、CLI JSON 字符串与 update 译文表；静态目录/七语契约覆盖，String 写入不发生外部 I/O。 |
| 已供池的帧必持许可 | protocol/transport 的预算入口固定传 Some，成功结果必定保留取得的许可。 |

历史 `Engine::managed_bytes` 保留 usize 返回接口，rustdoc 明确其预算锁中毒时会 panic；服务端使用可失败的 `memory_usage`，不调用该历史便捷接口。保留处均补充中英双语说明；本次公开文档注释已通过拒绝警告的 rustdoc。

## 验证原则

优先覆盖用户可观察契约和实际故障边界，不用与实现逐行同构的测试代替行为验证。纯文案改动检查链接和一致性即可；类型、协议、TTL、跨 Shard 与可靠提交是非平凡共享行为，必须有针对性测试。

先运行受影响的测试及必需格式/lint，结果足够后停止。没有工具或环境时记录具体限制，不凭空填写通过状态。

## 命名、本地帮助与安装验收

产品更名后的测试与构建使用 `ling-*` Rust 包及 `ling` / `ling-cli` 可执行文件。更名不得改变共享协议黄金字节、错误注册表或 Journal 恢复行为；Java 包与 Go 模块保留兼容身份。CLI 密码需验证 `LING_PASSWORD` 优先、旧 `LINGER_PASSWORD` 回退，并确保错误不回显凭据。

本地帮助验证入口为 `cargo test -p ling-cli-help` 以及 CLI 对应契约测试，至少包含：

- 七种语言的选项说明、帮助标题和速查入口；命令目录覆盖所有已开放命令，签名符合 [commands.md](commands.md)。
- 启动帮助、速查和单命令详情均有主组 / 子组与组间空行；无 ANSI 颜色时层级仍可辨识。说明在窄终端可读，示例保留可复制命令，单命令详情提供语法、说明、类型、示例和相关命令。
- `--lang`、`LING_LANG`、`LC_ALL`、`LC_MESSAGES`、`LANG` 与系统语言优先级；空值处理、`C` / `POSIX`、不支持语言及繁体中文的英语回退。
- 服务器未启动、端口不可用或认证未提供时，`ling --help`、`ling-cli --help`、`--cheatsheet`、单命令 HELP 仍成功且不创建网络连接。
- 普通逐行输入的 HELP 在本地处理，不发送协议请求；Batch 含 HELP 时整批不执行；错误帮助主题不伪装成成功数据库 Value。
- CLI 指南中的类型、TTL、数值、List / Map / Set 示例可解析，并通过真实网络保持严格类型和错误退出语义。
- 参数、解析、本地连接与服务端错误显示稳定错误码及本地化说明，使用示例 / 相关命令分组展示；错误不回显用户的敏感原文，结果未知提示与普通校验失败明确区分。

系统安装必须在各自原生环境验证，不在开发者本机随意注册服务。对 DEB / RPM、macOS PKG 和 Windows 安装器分别验证首次安装、服务注册 / 启停 / 重启、PING、同版本重装或升级、卸载；检查配置不被覆盖、卸载后服务已注销且用户配置 / Journal 未删除。便携包不应在解压或显示帮助时改变系统服务。发行流水线和 [install.md](install.md) 是实际入口；CI 脚本存在不等于已通过原生安装验收，未执行平台需明确列出。

完整稳定版还需目标环境多日压力、物理断电及语义可比的 Redis 8 测量；本次本地帮助和打包验证不替代这些结果，发布顺序见 [roadmap.md](roadmap.md#100-发布准备)。

## 最小正确性矩阵

| 范围 | 必须建立的证据 |
|---|---|
| Value | 10 类型往返；Null/缺失；Int/UInt边界；NaN拒绝；Infinity与零；相等/哈希一致；嵌套与大小上限 |
| 协议 | 黄金字节向量；半包/粘包；保留位与未知版本；畸形长度/count；非法UTF-8；分配前限额；错误帧关闭 |
| 命令 | 成功/缺失/类型错/条件不满足；错误无部分写入；重复参数；空容器；所有返回类型 |
| 多数据库 | 默认选择、越界、同名隔离、选库屏障、跨库复制/移动、交换与清空 |
| 并发 | 同Key顺序、多连接竞争、跨Shard原子可见、固定锁序、无死锁、读写与清库竞争 |
| TTL | 到期前/恰好/之后；停止调度仍不可见；续期/PERSIST/删除重建；时钟跳变；过期竞争 |
| 事件 | 六类事件与无变化操作；Fast丢弃可观测；Reliable提交前不可见；稳定ID与流内顺序 |
| 消费 | 显式ACK、乱序空洞、ACK丢失、租约转移、组隔离、过期offset、过滤去重 |
| Journal | 提交边界、尾部截断、坏中段、检查点、轮转、目录锁、fsync失败、磁盘满、恢复ID连续 |
| SDK | 共享协议向量；响应乱序；迟到响应；数据库池隔离；断线写结果未知；关闭释放任务 |
| 配置 | 默认值、未知字段、范围、组合错误、密钥隐藏、静态项热改拒绝、RELOAD整体校验 |
| 资源 | 慢消费者、连接上限、缓冲上限、大对象、深层对象、淘汰失败原子性、受管内存与RSS分开 |

“原子性”应由另一个并发观察者验证看不到中间状态，不能只断言最后两个 Key 都已修改。超时测试区分尚未执行和提交结果未知，不把超时等同于回滚。

协议测试至少包含独立期望字节，与同一编码器“编码后解码”等测试互补。模糊测试重点是恶意长度、整数溢出、嵌套、截断以及解析器的有界内存；不能仅靠随机合法值。

## 可控时间与故障注入

TTL 测试应注入单调时钟和墙钟，推进到边界，不依赖真实长时间睡眠。验证大批同时到期和高频续期队列有界；没有任务调度也不能读到过期 Value。

Journal 测试在追加记录、提交标记、fsync、发布内存及发送响应各处注入故障。分别验证明确未提交、明确已提交和结果未知，恢复只接受完整事务。

至少测试：终止进程、磁盘满、权限变化、fsync错误、日志尾部截断、中段损坏、重启期间新请求被拒绝、ACK写入中断、组初次创建中断。涉及文件目录持久化时验证真实文件系统行为；内存假实现不能独自证明持久性。

可靠性仅承诺已提交记录在保留期内；测试也必须验证不补发冷启动删除和停机期间到期事件，避免把未承诺能力悄悄变成依赖。

## 基准方法

基准分别记录引擎微基准与完整网络链路结果，不能用内存函数吞吐替代真实远程服务吞吐。Fast 和 Reliable 分开报告，单独记录 TLS 与关闭TLS结果。

每次可复现实验至少保留：

- 项目版本、编译器与优化参数、依赖版本、Redis 8 的确切版本。
- CPU型号/核数、内存、存储、操作系统/内核、线程与CPU绑定、服务端和压测机拓扑。
- 完整配置、数据库数、Shard数、并发连接和流水线深度、请求大小、Key/Value分布与命中率。
- 暖机时间、测量时长、独立重复次数、随机种子以及原始测量输出。
- 完成吞吐、错误/超时、P50/P95/P99/P99.9、CPU占用、受管内存、RSS、Bytes per Key。

延迟以发出请求至收到完整响应计算，包含队列等待；说明采用开放负载还是闭环客户端，避免忽略过载下的排队时间。压测客户端不能与服务争抢全部CPU却不记录。

基础矩阵至少包括：纯GET、纯SET、混合读写；小/中/大Value；均匀/热点Key；命中/未命中；固定/高频更新TTL；事件禁用/Fast/Reliable；无消费者/快消费者/慢消费者。

测量 TTL 时分别报告“过期后错误可见次数”（必须为0）与“到期至回收/事件投递的延迟”（允许排队但应度量），不能用平均调度精度掩盖旧值泄漏。

内存实验需说明统计分母是逻辑活跃Key还是实际仍占内存条目，并给出预热/稳态/删除后RSS。协议缓冲、Journal页缓存及分配器滞留不能归为不存在的“免费内存”。

## 阶段 3 基础测量

以下为阶段 3 源码的历史基线；阶段 4 起增加到期索引预留计费和维护循环，当前版本另见阶段 7 实测，不能用历史结果表示当前开销。

2026-11-14 在 VMware 虚拟机中测量，客户机暴露 8 个逻辑 CPU，型号为 i7-11800H @ 2.30GHz，内存约 23.4 GiB，Linux x86_64，rustc 1.98.1。使用 Cargo release 默认优化，不绑核；TCP 客户端与独立服务端共享同机 CPU，经回环地址通信。完整内核、CPU、存储、编译器、Cargo.lock 摘要和源码版本见 [环境原始数据](../benchmarks/results/stage3-2026-11-14-environment.json)。所记录工作区只有未追踪的 Python 字节码，已清理，无源码改动。

工具为 [baseline.rs](../crates/server/examples/baseline.rs) 和 [run.py](../benchmarks/run.py)。复现命令为 `python3 benchmarks/run.py target/benchmarks/my-run`，需要 Linux `/proc`、Python 3 和 Rust；输出目录须不存在。脚本构建 release，为每次 TCP 实验启动全新服务端，保存配置、原始 JSON、错误输出及进程指标。没有运行 Redis 比较，也没有 TLS、认证、TTL、事件或 Journal。

固定 16 个数据库、每库 4 个 Shard，只使用数据库 0；noeviction，实例预算 256 MiB，无 Key 配额。TCP 最大 32 个连接，请求 / 响应上限 1 MiB，其余使用默认配置。8 个闭环客户端，每连接流水线深度 1；预填充 10,000 个固定长度 Key（`baseline:` 加 8 位数字），值为 64 字节 Bytes，均匀命中读取。分别测纯 GET、纯 SET、50% GET / 50% SET；写入内容随序号变化。负载种子 20261114，进程内部路由哈希仍随机。每客户端预热 1,000 次再测量 10,000 次，每次共 80,000 个测量请求，各负载独立重复三次。

下表每列为三次独立结果的中位数，分位数来自各次成功请求按最近秩计算；不是合并样本的分位数。单请求计时从参数准备后调用 SDK / 引擎开始，到完整结果返回，TCP 包含编码、队列、网络与解码；吞吐使用所有客户端测量阶段的总完成时间，包含收集结果的少量开销。原始输出保留每次结果、微秒直方图与 CPU / RSS：[18 次测量原始数据](../benchmarks/results/stage3-2026-11-14-results.json)。

| 路径与负载 | 成功请求/秒 | P50 μs | P95 μs | P99 μs | P99.9 μs |
|---|---:|---:|---:|---:|---:|
| 引擎 GET | 1,603,359 | 0.923 | 5.903 | 82.145 | 331.379 |
| 引擎 SET | 616,263 | 1.903 | 27.505 | 205.732 | 1,126.791 |
| 引擎混合 | 841,442 | 1.432 | 28.521 | 144.168 | 749.856 |
| TCP GET | 35,087 | 195.343 | 464.379 | 776.431 | 1,490.872 |
| TCP SET | 34,896 | 196.899 | 456.088 | 743.046 | 1,446.911 |
| TCP 混合 | 35,400 | 193.866 | 449.323 | 737.623 | 1,432.582 |

全部测量与预热错误 / 超时为 0。网络测量阶段每次约 2.17—2.56 秒，引擎仅约 0.048—0.151 秒，短实验容易受调度影响，不能视为稳态容量或过载验收。闭环负载不会模拟无界到达请求；尾延迟也不能代表开放负载下的排队。此结果是阶段 3 基线，完整矩阵、热点、大值、长期压力及淘汰策略成本留待后续阶段。

TCP 服务端 VmHWM 峰值为 9,781,248—10,313,728 字节（约 9.33—9.84 MiB），CPU 时间约 6.76—7.74 秒；CPU 指标包含预填充和预热，不能直接除以测量时长当作 CPU 百分比。引擎所在进程的采样 RSS 峰值约 9.23—9.77 MiB，含客户端任务和采样 Vec。RSS 每 20 ms 采集 Linux VmHWM；极短进程仍可能漏采末尾高点。

引擎最终受管字节均为 26,730,000，即每个活跃 Key 保守计费 2,673 字节；该逻辑模型包含有意高估的索引开销，因此可能高于 RSS。阶段 3 测量时尚未开放 INFO，服务端受管字节未经网络测得，原始记录为 null；不得拿直接引擎值冒充 TCP 观测。尚未进行删除后 RSS 或分配器滞留实验。

## 阶段 7 本机验收

2026-11-15 在 VMware 客户机完成本轮测量：8 个逻辑 CPU，i7-11800H @ 2.30GHz，约 23.4 GiB 内存，Linux 7.0.0-43、ext4、VMware 虚拟磁盘，rustc 1.98.1。release 默认优化，无 CPU 绑定；客户端与服务端共享本机 CPU，经回环 TCP 通信，除 TLS 场景外不启用 TLS，均不启用认证。环境、源码摘要、工作区状态和参数见 [环境原始数据](../benchmarks/results/stage7-2026-11-15-environment.json)，每次配置、响应直方图、CPU / RSS 时间序列和结果见 [测量原始数据](../benchmarks/results/stage7-2026-11-15-results.json)。

复现入口为 `python3 benchmarks/stage7.py target/benchmarks/stage7`：18 类场景各三次，再运行 Fast / Reliable 各 600 秒。`--only` 筛选短场景且跳过持续运行，`--soak-only --soak-seconds 600` 仅执行持续运行，附加 `--soak-mode reliable` 或 `fast` 可复测一种模式。真实 TTL 专项入口为 `python3 benchmarks/lifecycle.py target/benchmarks/ttl`。均需 Linux `/proc`、Python 3 和 Rust，输出目录必须不存在；脚本保存固定二进制副本，后续编译不会改变已经启动的实验版本。

本轮矩阵主体基于 `198d555`，`matrix_first` 保留前 9 类的 27 次成功结果；大值场景因驱动重复传入 `--keys` 被拒绝，修正后 `matrix_remaining` 保存余下 9 类的 27 次成功结果。失败启动不计入结果。优化前后另存 `before` / `after`；`after` 当时尚未提交的事件源码摘要对应 `63e3323`。矩阵与持续运行使用物理游标优化后的版本，最终控制区压缩候选修复 `35a4d85` 后重新通过全部契约及 TTL 专项；持续运行没有消费者，不经过该修复路径。精确来源以各次环境中的 revision、working_tree 和 source_sha256 为准，不把未提交测量冒充干净版本。已存在的固定二进制摘要在归档时补采并标明时点。

### 负载矩阵与测量边界

网络实例固定 16 库、每库 4 Shard，只访问库 0，noeviction，受管预算 128 MiB；最大 32 连接，请求 / 响应各 1 MiB。直接引擎使用 1 库、4 Shard、256 MiB 预算，结果单列。4 个闭环客户端，每连接流水线深度 1，各预热 100 次；网络每次测量 3 秒，引擎每客户端 100,000 次。种子 20261114，不固定内部路由哈希。预填充 1000 个 `baseline:` 加八位数字的 Key，默认 64 B Bytes；4 KiB 场景仍 1000 Key，64 KiB 场景为 256 Key。混合为各 50% GET / SET；热点场景将 90% 请求集中于首 1% Key；未命中场景全部读取预留的不存在命名空间。

TTL 场景每次 SET 使用 PX 100，允许已过期读取明确返回缺失并单列计数；未命中同样属于预期有效响应。事件缓冲 8 MiB、每组窗口 128、最多 32 组；Reliable 目录预算 64 MiB，统一保留 7 天，仍逐事务 fsync。所谓快消费者只是没有额外业务睡眠，慢消费者每次事件额外等待 20 ms，再显式 ACK；不保证任一消费者跟上生产速度。

每列是三次独立结果的中位数，不合并各次分位数。单请求从准备好参数后调用引擎 / SDK 至完整响应，包含编解码和队列等待；限速等待不计入请求延迟。直方图先向下取整到整数微秒（最小记录为 1），再按每二进制数量级 32 桶向上量化，桶舍入误差小于 3.2%，另有不足 1 μs 的取整误差。不能用引擎的 1 μs 记录推断亚微秒精度。

| 路径与负载 | 有效请求/秒 | P50 μs | P95 μs | P99 μs | P99.9 μs |
|---|---:|---:|---:|---:|---:|
| 引擎 GET | 2,033,769 | 1 | 1 | 2 | 52 |
| 引擎 SET | 639,947 | 1 | 9 | 59 | 116 |
| 引擎混合 | 1,095,162 | 1 | 3 | 33 | 86 |
| TCP GET | 21,640 | 172 | 304 | 456 | 1,216 |
| TCP SET | 20,927 | 176 | 304 | 448 | 1,504 |
| TCP 混合 | 16,554 | 192 | 448 | 1,088 | 3,392 |
| TCP 未命中 | 19,099 | 180 | 368 | 832 | 2,816 |
| TCP 热点混合 | 19,884 | 180 | 344 | 560 | 1,504 |
| TCP 4 KiB 混合 | 18,609 | 192 | 368 | 576 | 1,536 |
| TCP 64 KiB 混合 | 9,996 | 296 | 688 | 1,120 | 2,112 |
| TCP TTL 混合 | 18,410 | 192 | 376 | 624 | 1,952 |
| TCP Fast，无消费者 | 12,232 | 216 | 768 | 2,112 | 5,760 |
| TCP Reliable，无消费者 | 2,815 | 1,248 | 3,264 | 4,864 | 30,720 |
| TCP Fast，快消费者 | 20,385 | 180 | 328 | 496 | 1,440 |
| TCP Fast，慢消费者 | 20,247 | 180 | 328 | 528 | 1,440 |
| TCP Reliable，快消费者 | 2,623 | 1,344 | 3,456 | 5,120 | 57,344 |
| TCP Reliable，慢消费者 | 2,651 | 1,280 | 3,456 | 4,864 | 54,272 |
| TLS 混合，关闭事件 | 19,136 | 188 | 352 | 576 | 1,632 |

54 次共完成 5,644,670 个有效响应，测量和预热请求错误均为 0；缺失响应共 202,591 次，主要来自独立未命中及 TTL 负载。消费者错误另列：Fast 快 / 慢消费的每次实验都出现一次 `ERR_OFFSET_EXPIRED`，ACK 分别为 190 / 143 / 143 和 30 / 24 / 26。Reliable 无消费错误，快 / 慢消费者 ACK 分别为 302 / 306 / 303 和 65 / 63 / 63，仍有积压；这不表示事件全部已消费或端到端延迟有界。每次事件年龄的 10 ms 桶、pending、Fast dropped 和 Journal errors 均保留原始记录。

短测量按场景顺序连续执行，没有随机交错。后一半矩阵与低速持续运行同机重叠，期间还运行少量 Cargo 验证；CPU / 磁盘争用和系统漂移均可能改变结果。Fast 无消费者三次约 11,764—17,071 请求/秒，不能从此表推导增加消费者会加速，也不能将阶段 3 的不同客户端数、计时方式和旧版本直接作性能升降结论。3 秒闭环测量不是稳态容量、开放到达过载或生产尾延迟承诺。

64 B 预填充的受管内存为 3,970,000 B，即每个逻辑活跃 Key 保守计费 3,970 B；4 KiB 为 8,002 B/Key，64 KiB 为 69,442 B/Key。计费包含索引、身份和到期任务等预留，与 RSS 不等价。全部 54 次 FLUSHDB 后受管内存归零；网络矩阵每 20 ms、持续运行每 1 s 采样；原环境的 sample_seconds=1 字段仅对应持续运行，归档说明已更正。每次启动 / 中途 / 删除后 RSS 与进程 CPU 保留在 resources。短矩阵服务端峰值 RSS 最大为 43,057,152 B（大值负载）；例如首轮大值在启动 / 中途 / 删除后为 5,795,840 / 41,050,112 / 26,152,960 B，体现分配器可能滞留物理内存。CPU 记录含预填充、预热、测量与清理，不直接除以 3 秒解释为测量期 CPU 百分比；系统 Journal 页缓存也不属于受管缓存预算。

### 已测瓶颈与修复

Reliable 原先每次取事件都从头扫描 Journal，并在收集较多候选后只交付首个匹配事件。改为保留不持久化的记录游标、匹配后尽早停止；同事务后继靠逻辑 cursor 去重，恢复 / 压缩重置物理位置。独立三次对照中，50/50 读写且带 Reliable 消费者的有效请求吞吐中位数从 771 增至 2,496 请求/秒（约 3.24 倍），请求 P99 从 34,816 降为 5,120 μs；三次 ACK 数从 171 / 154 / 164 增为 304 / 304 / 307。倍率只指这组短测量的读写请求吞吐，不是事件吞吐或所有场景提升保证。

fsync、保留、ACK 与原子事务均未放宽。回归覆盖过滤事件、同事务多个匹配事件、读至 EOF 后追加、压缩、重启未 ACK 重投，以及 1 MiB Journal 中 2,500 条事件逐一消费时控制区满触发的压缩。最后一项测试在去掉候选游标重置时实际失败，修复后通过；未改网络或磁盘 v1 格式。没有据此引入自定义分配器、无锁索引或复杂时间轮。

首次十分钟 Reliable 实验暴露另一个随日志增长的成本：后台每秒持有事件提交锁，全量扫描并重写尚在 7 天保留期内的日志。该次 P99 达 294,912 μs，服务端累计 CPU 178.49 秒，虽然请求错误为 0，仍属于需要修复的尾延迟退化。`ad6665b` 改为先检查首个保留事件是否可能到期，无候选时跳过全量维护；容量不足仍按需压缩控制记录，故障仍报错。新增真实磁盘契约覆盖期限前文件不变、恰好到期、后继保留、重启和故障；对照复测见下方，原始较差结果继续保留。

### TTL 与资源边界

`lifecycle.py` 在游标修复版本 `3f9340c` 的独立 TCP 服务上运行 Fast / Reliable 各 30 轮，每轮 SET 两个新 Key、PX 20；从第二个应答后再等待完整 20 ms 后 GET 一个 Key，另一个 Key 不读取以观察后台回收；显式接收并 ACK 两个 Expired 事件，最终 INFO managed_bytes 为 0。以下延迟相对第二个 SET 应答，包含客户端处理 / 拉取 / ACK 和网络开销，且第一个 Key 更早写入；不是服务端到期时点到调度执行的精确误差，原始逐次结果保留于 ttl。

| 模式 | 截止后旧值可见 | 两个到期事件观察范围 μs | 最晚观察到受管内存归零 μs |
|---|---:|---:|---:|
| Fast | 0 / 30 | 37,488—66,472 | 66,732 |
| Reliable | 0 / 30 | 28,768—66,909 | 67,134 |

新增 `crates/server/tests/resources.rs` 四项真实 TCP 契约：五种淘汰策略的 32 KiB 内存 / 8 Key 上限及恢复；超大写入拒绝前不删除其他条目；半帧占满连接后回收及超大声明帧关闭；慢读端使用小接收缓冲且不读取 512 KiB 响应，写超时释放连接同时其他客户端 PING 成功；1 MiB Reliable Journal 填满后保留最后成功值、pending 有界且 ACK 仍可提交。协议另对黄金帧执行固定种子的 20,000 次字节突变和截断，解码无 panic；这是烟测，不是持续覆盖引导模糊测试。

### 持续运行

每种模式独立运行 600 秒，4 个闭环连接每连接限速 20 请求/秒（总计约 80），50/50 读写、64 B、1000 Key、90% 请求集中首 1% Key，每次写入 PX 1000，无消费者；预算与短矩阵一致。预填充和每连接 100 次预热在计时外，资源每秒采样且包含这些阶段。下表保留维护优化之前的首次完整实验，不以复测覆盖较差结果。限速负载用于观察资源随时间的变化，不代表满载容量。两次实验与部分矩阵、Cargo 验证及 TTL 探针共享同机资源。

| 模式 | 测量时长 s | 有效响应 | 预期缺失 | 请求错误 | P99 μs | 服务端峰值 RSS MiB | 结束观测 Journal B |
|---|---:|---:|---:|---:|---:|---:|---:|
| fast | 600.002 | 48,004 | 2,025 | 0 | 1,888 | 8.48 | 0 |
| reliable | 600.148 | 47,984 | 2,065 | 0 | 294,912 | 6.43 | 4,422,630 |

两种模式预热错误、Journal 错误均为 0；结束 INFO 在 FLUSHDB 前采集，清库后受管内存与到期索引均为 0。fast 提交 50,415 个事件、丢弃 35,952 个，结束前受管内存 369,210 B、到期项 15；服务端第 10 秒 / 中点 / 清库后 RSS 为 7,213,056 / 8,769,536 / 8,892,416 B，全进程 CPU 累计 53.14 秒。reliable 提交 50,393 个事件、丢弃 0 个，结束前受管内存 369,210 B、到期项 15；服务端第 10 秒 / 中点 / 清库后 RSS 为 6,406,144 / 6,594,560 / 6,668,288 B，全进程 CPU 累计 178.49 秒。Fast 无消费者仍按预算丢弃旧事件，Reliable 保留已提交记录且本次未触达容量上限；这两个现象均不代表无限保留。观察期间未见服务退出或请求失败；十分钟数据不能证明没有长期泄漏或足以通过多日生产压力验收。

`ad6665b` 的保留维护优化后，用相同 600 秒、80 请求/秒、TTL / 热点和预算重新测量 Reliable，完成 48,004 个有效响应（预期缺失 2,036），请求及预热错误均为 0；P50 / P95 / P99 / P99.9 为 1,152 / 2,752 / 3,904 / 27,136 μs。服务端全程 CPU 累计 54.48 秒，峰值 RSS 6,828,032 B，清库后 RSS 5,824,512 B，受管内存与到期项均为 0；清库前 Journal 4,424,053 B、提交事件 50,415、丢弃 0、Journal 错误 0。复测初段有同机短矩阵和验证竞争，与原实验也非严格隔离对照；结果支持此负载下消除重复维护成本，不代表所有部署均有相同改善。

优化后另外重复 Reliable 无消费者 / 快消费者各三次，原始数据在 maintenance_matrix：有效请求/秒中位数为 2,905 / 2,256，请求 P99 为 4,352 / 6,272 μs，请求与消费错误为 0；快消费者 ACK 为 284 / 318 / 319，仍不代表跟上生产。短测量存在波动，不将全部收益归因于维护优化。首次长测、完整矩阵与两项复测均保留版本及环境，不覆盖旧结果。

本轮最终运行 `cargo fmt --all -- --check`、`cargo test --workspace`（135 项）、`cargo clippy --workspace --all-targets -- -D warnings`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 及 `python3 tests/sdk_contracts.py` 全部通过；后者包含 Go race、Java `-Xlint:all -Werror`、TCP 与双向 TLS 的真实服务契约。Python 驱动语法检查通过，TTL 专项真实运行通过。

本机安装 Redis 7.0.15，没有 Redis 8，因此未运行 Redis 对比。`perf_event_paranoid=4` 拒绝性能计数器采样，保留工具错误且未修改系统权限；瓶颈判断来自源码执行路径及对照负载。本轮未做持续 fuzz、多日压力、物理断电、独立压测机或目标部署硬件验收。0.1.0 保持预览版本，稳定发布需要这些实际部署条件下的评估，不能据此宣布优于 Redis 或生产可用。

## 0.1.5 可观测性与基线重测

本轮开发以 `b1220f4`（0.1.4 发布后的 main）为基线。当前开发主机是 Windows，不能执行 Reliable 测量；使用 GitHub Actions 的 `ubuntu-24.04` runner，同机回环并将服务端与客户端绑定到互不重叠的 CPU。它与阶段 7 的 VMware 环境不同，旧数据保持原样，只并列中位数、极差和异常，不将差异解释为版本性能升降或目标部署环境验收。

`benchmarks/v015.py` 复用阶段 7 的 18 类负载，使用种子 20261117 将三次重复随机交错；旧 `stage7.py` 的命令行参数保持兼容。先从干净的独立源码检出构建服务端，再只覆盖 `baseline.rs` 编译测量客户端并恢复原文件。环境 JSON 分开记录被测源码、工具提交、覆盖文件与二进制摘要、文件系统、CPU 绑定和性能计数器探针。性能计数器可用时在矩阵之外单独采样，采样不混入吞吐对照；不可用时保留原始失败原因，不能据源码猜测火焰图。

消费任务分别记录 ACK 数、原始错误和终止原因。Fast 场景允许 `ERR_OFFSET_EXPIRED`，作为保留丢失单列；Reliable 允许积压，但消费任务必须持续到请求停止。生产请求成功不能掩盖消费端的意外错误、提早退出或缺失结果。历史阶段 7 JSON 缺少终止状态，只能复核已有计数，不能追认消费任务持续性；归档的 `historical-summary.json` 中 `valid_runs=0` 表示没有新验收字段，不代表历史实验执行失败。后续工具已将这种未知状态改为 `null`，归档仍保留当时的原始输出。

可在干净的 Linux 检出运行：

```sh
python3 -B -m unittest discover -s benchmarks -p 'test_v015.py' -v
python3 benchmarks/v015.py /tmp/ling-v015-full --source-root "$PWD" --suite full
python3 benchmarks/v015.py /tmp/ling-v015-metrics --source-root "$PWD" --suite metrics
```

完整测量包含 54 次短矩阵与 Fast / Reliable 各 600 秒限速持续运行。指标对照将 GET、SET、混合分别在关闭指标、开启但不抓取、开启且每秒抓取三种状态下各运行三次，分别展示采集与抓取成本。高频计时由现有 `metrics.enabled` 控制，默认关闭；便宜的生命周期计数持续累计。

基线运行 `44065341888` 的源码为 `b1220f4a27b336cb1ae3b9ecd07bb6e3a256ce42`，工具为 `7174f6ff580b23a673a54026add45d68c6aa7348`，两者工作树均为空。基线 runner 暴露 4 个逻辑 CPU（AMD EPYC 9V45、2 核 × 2 线程），内存 16,367,524 KiB，Linux `7.0.0-1017-azure`、ext4，rustc `1.100.0` / LLVM `23.1.5`。候选完整运行 `44067271629` 的源码及工具均为 `e3a76c026592fc5b9c689a58c22c4ba438e2a1c6`，runner 是 Intel Xeon 6975P-C、Linux `7.0.0-1014-aws`、ext4；指标对照 runner 则为 Intel Xeon Platinum 8573C / Azure。三个运行都暴露 4 个逻辑 CPU，逻辑 CPU 0、1 分给服务端，2、3 分给客户端，不能据此证明物理核心或宿主机隔离。完整环境及源码、工具、二进制摘要保留于各运行的 `environment.json`；不同 CPU 与宿主环境的结果不能作为版本性能升降对照。

各运行的 `perf_event_paranoid=4`，探针返回 `No supported events found` 并明确拒绝性能监测访问，本轮没有火焰图。首次候选运行 `44066271464` / `44066271925` 因测量客户端遗漏 INFO Float 转换，在写结果时退出；修复后重测，不把无结果的首次尝试计为通过。失败 stderr 与环境保留在 [首次完整测量](../benchmarks/results/v015-2026-11-17/failed-candidate-full-44066271464/environment.json)和[首次指标对照](../benchmarks/results/v015-2026-11-17/failed-candidate-metrics-44066271925/environment.json)。基线没有新增 Float 字段，客户端修复只影响诊断序列化；两个工具提交的差异与各自摘要均保留。

原始记录中的 `resources.json` 与聚合 `results.json` 无损压缩为 `.json.gz`，逐场景结果、环境与汇总仍可直接读取。各运行的 `artifact-manifest.json` 记录原始和归档文件的字节数及 SHA-256，已逐项解压核对；二进制、Journal 和重复聚合文件不入库。

### 18 类矩阵与持续运行

基线 `44065341888` 与候选 `44067271629` 各完成 18 类 × 3 次短测及 2 次长测，分别为 56 / 56 项验收通过；生产和预热错误均为零。两次完整测量的随机调度文件完全相同。下表吞吐为三次独立结果的中位数 [最小值，最大值]，取整至请求/秒；分位数为候选各次分位数的中位数，没有合并直方图。完整对照见 [comparison.json](../benchmarks/results/v015-2026-11-17/comparison.json)，候选原始环境与记录见 [candidate-full-44067271629](../benchmarks/results/v015-2026-11-17/candidate-full-44067271629/environment.json)。

| 场景 | 阶段 7 req/s | 0.1.4 req/s | 0.1.5 req/s | 0.1.5 P99 / P99.9 μs |
|---|---:|---:|---:|---:|
| 引擎 GET | 2,033,769 [1,962,422，2,165,973] | 2,744,902 [2,705,947，2,748,141] | 2,616,937 [2,403,143，2,672,358] | 1 / 3 |
| 引擎 SET | 639,947 [583,328，658,743] | 1,347,943 [1,329,863，1,365,648] | 1,170,458 [1,169,766，1,196,068] | 2 / 10 |
| 引擎混合 | 1,095,162 [1,023,183，1,097,567] | 1,769,313 [1,740,885，1,799,375] | 1,399,132 [1,369,907，1,548,015] | 2 / 9 |
| TCP GET | 21,640 [21,556，21,837] | 64,387 [62,156，66,708] | 88,205 [87,628，88,346] | 84 / 144 |
| TCP SET | 20,927 [20,723，21,645] | 65,254 [63,427，65,643] | 84,420 [84,079，84,595] | 88 / 132 |
| TCP 混合 | 16,554 [15,397，20,025] | 63,977 [63,330，65,204] | 85,537 [84,778，85,624] | 88 / 140 |
| TCP 未命中 | 19,099 [15,942，22,119] | 65,058 [63,204，65,600] | 89,417 [86,288，90,137] | 84 / 176 |
| TCP 热点混合 | 19,884 [19,653，20,393] | 65,606 [62,854，65,675] | 85,840 [85,773，86,213] | 86 / 140 |
| TCP 4 KiB 混合 | 18,609 [18,384，20,164] | 62,573 [60,188，62,652] | 70,211 [70,126，70,302] | 108 / 200 |
| TCP 64 KiB 混合 | 9,996 [9,912，10,471] | 25,630 [24,878，26,549] | 23,389 [22,888，23,449] | 304 / 448 |
| TCP TTL 混合 | 18,410 [17,975，19,044] | 63,094 [62,881，63,126] | 82,882 [81,698，83,395] | 90 / 144 |
| Fast，无消费者 | 12,232 [11,764，17,071] | 64,200 [63,516，64,227] | 83,663 [83,497，83,912] | 90 / 144 |
| Reliable，无消费者 | 2,815 [2,730，2,828] | 8,465 [8,445，8,495] | 15,840 [15,612，17,105] | 576 / 720 |
| Fast，快消费者 | 20,385 [17,574，20,799] | 64,482 [64,135，65,226] | 83,820 [83,788，83,971] | 90 / 144 |
| Fast，慢消费者 | 20,247 [18,795，20,907] | 62,925 [62,158，64,681] | 83,791 [83,195，83,921] | 90 / 144 |
| Reliable，快消费者 | 2,623 [2,309，2,698] | 7,940 [7,774，8,368] | 16,694 [16,387，16,762] | 544 / 704 |
| Reliable，慢消费者 | 2,651 [2,419，2,867] | 8,250 [7,674，8,436] | 16,888 [16,648，16,939] | 528 / 672 |
| TLS 混合 | 19,136 [18,504，19,557] | 65,120 [64,771，65,723] | 83,264 [83,250，83,727] | 90 / 128 |

阶段 7 与两次新测量均使用 4 个客户端、单连接深度 1，TCP 每次 3 秒、引擎每客户端 100,000 次，请求计时含义保持一致。环境、CPU 绑定、种子（20261114 → 20261117）、源码与场景顺序不同，CPU 资源核算也改为客户端精确 `wait4` 和服务端独立采样；这些列仅并列展示，不能据此宣称版本加速或退化。引擎低分位数仍受整数微秒量化限制。

| 短测消费者 | 0.1.4 三次 ACK | 0.1.5 三次 ACK | 两版实际终止 / 错误 |
|---|---|---|---|
| Fast 快 | 46 / 45 / 45 | 35 / 36 / 35 | 每次因预期 ERR_OFFSET_EXPIRED 停止，各 1 次保留错误 |
| Fast 慢 | 9 / 9 / 9 | 7 / 7 / 7 | 每次因预期 ERR_OFFSET_EXPIRED 停止，各 1 次保留错误 |
| Reliable 快 | 305 / 299 / 305 | 302 / 303 / 302 | 每次均 requested_stop，无消费错误 |
| Reliable 慢 | 63 / 64 / 63 | 63 / 63 / 63 | 每次均 requested_stop，无消费错误 |

Fast 的保留丢失是本轮允许且单列的结果；它不等于持续消费成功。Reliable 允许积压，但两版各六次短测均保持消费直到停止信号；ACK 数不表示所有事件均已消费。所有原始错误和事件年龄保留在逐场景结果中。

四项长测均为 4 个连接各限速 20 请求/秒、50% GET / SET、热点与 PX 1000，且没有消费者。每项成功响应 48,004 次，请求、预热与 Journal 错误均为零；CPU 范围包含预填充、预热及清理，服务端为停机前采样，RSS 为采样峰值。

| 版本 / 模式 | 实际时长 s | P99 / P99.9 μs | 客户端 / 服务端 CPU s | 服务端峰值 RSS B |
|---|---:|---:|---:|---:|
| 0.1.4 Fast | 600.000686 | 344 / 912 | 2.177731 / 12.57 | 9,777,152 |
| 0.1.4 Reliable | 600.002208 | 1,888 / 14,336 | 2.944535 / 14.72 | 7,716,864 |
| 0.1.5 Fast | 600.001949 | 560 / 784 | 2.678208 / 17.59 | 10,067,968 |
| 0.1.5 Reliable | 600.002215 | 1,056 / 1,440 | 2.641747 / 19.25 | 7,602,176 |

四项长测均提交 49,942 个事件；Fast 各丢弃 35,479 个旧事件，Reliable 各丢弃 0 个。Reliable 清库前 INFO Journal 为 4,382,141 / 4,382,078 B，包含清理后的末次文件采样分别为 4,389,610 / 4,389,547 B。两版全部网络场景在 FLUSHDB 后的 `managed_bytes` 与 `scheduled_expirations` 均为 0；候选两项长测清理后 RSS 仍为 10,067,968 / 7,598,080 B，受管归零不代表分配器立即释放物理内存。长测没有 ACK 或消费者终止结果，十分钟观察也不构成多日稳定性或目标部署环境验收。

### 指标采集与抓取开销

Actions `44067279489` 在 `e3a76c026592fc5b9c689a58c22c4ba438e2a1c6` 上完成全部 27 次对照。服务端绑定 CPU 0、1，客户端绑定 CPU 2、3；每格为三次独立吞吐的中位数 [最小值，最大值]，不合并请求分位数。原始环境、随机调度、逐次结果与资源记录见 [candidate-metrics-44067279489](../benchmarks/results/v015-2026-11-17/candidate-metrics-44067279489/environment.json)，汇总见 [metrics-comparison.json](../benchmarks/results/v015-2026-11-17/candidate-metrics-44067279489/metrics-comparison.json)。

| 负载 | 关闭指标 req/s | 开启、不抓取 req/s | 开启、每秒抓取 req/s | 采集变化 | 额外抓取变化 | 三态 P99 中位数 μs |
|---|---:|---:|---:|---:|---:|---|
| GET | 54,363 [54,017，54,636] | 53,808 [53,807，53,941] | 53,454 [53,019，53,861] | −1.02% | −0.66% | 126 / 124 / 128 |
| SET | 52,544 [51,809，52,805] | 52,582 [52,569，52,716] | 52,445 [51,869，53,104] | +0.07% | −0.26% | 132 / 132 / 132 |
| 混合 | 52,949 [52,934，53,391] | 52,483 [52,151，52,769] | 52,811 [52,230，52,961] | −0.88% | +0.62% | 128 / 132 / 132 |

采集变化比较「开启、不抓取」与「关闭」，额外抓取变化比较「开启、每秒抓取」与「开启、不抓取」。全部请求验收通过；9 次带抓取实验各完成 4 次成功抓取，共 36 次，没有 HTTP、超时或响应超限失败。吞吐中位数的变化约为百分之一量级；按相同 repeat 编号比较的单次采集变化可达 −2.32%，这些实验随机交错、并非同时配对。三次样本不足以作统计显著性或性能提升结论；[逐次变化](../benchmarks/results/v015-2026-11-17/candidate-metrics-44067279489/metrics-cost-details.json)保留原始范围，高频采集维持默认关闭。

引擎参考模型以独立的 `BTreeMap` 表示十种 Value 与毫秒期限，用六个固定种子各执行 1200 步；每步比较结果、错误码、所有 Key 的值及 PTTL，失败时核对身份与修订不变。墙钟独立跳变，单调时间只前进。并发契约保留 MSET、SWAPDB 观察，新增跨分片 RENAME 的完整条目观察；模型不取代已有预算、Journal 和真实网络契约，也不等同持续覆盖引导模糊测试。

### 构建与契约验证

2026-11-17，Actions `44067263752` 在提交 `e3a76c026592fc5b9c689a58c22c4ba438e2a1c6` 完成五平台构建，公共检查与全部打包任务成功。实际通过 `cargo fmt --all -- --check`、严格 Clippy、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps`、发行依赖图不含测试 failpoints 的检查、workspace 中的 Rust SDK / TLS 契约，以及 `python3 tests/sdk_contracts.py` 的 Go / Java TCP / 双向 TLS 契约。

| 平台 | 实际测试 | 制品与安装验证 |
|---|---|---|
| Linux x86_64 / ARM64 | 每架构原生 workspace 234 项，musl server / CLI 107 项 | 各 DEB、RPM、AppImage、tar.gz；安装、服务、重复安装保留配置、卸载保留状态、AppImage 与便携包 TCP 烟测 |
| macOS Intel / ARM64 | 每架构 workspace 233 项 | 各 PKG、tar.gz；原生安装、launchd 服务、配置与状态保留、便携包 TCP 烟测 |
| Windows x86_64 | workspace 217 项，包含 Fast 与明确拒绝 Reliable | Setup EXE、ZIP；安装、服务、PATH 与配置保留、卸载及便携包 TCP 烟测 |

以上测试均为零失败、零忽略；不同平台的条件编译测试数不相同。共生成 14 个包，本机下载后生成并重读核对 SHA-256 清单，检查五份便携包的 MIT 正文及第三方声明，另对下载的 Windows ZIP 解压后运行 `scripts/smoke-package.py`，真实 PING / SET / GET / PTTL / SHUTDOWN 均通过。制品对应上面的 `e3a76c0`；后续仅修正基准工具的历史验收状态展示与测试样例、归档数据及更新文档，未改服务端或 CLI 实现。该次为开发分支构建，标签发布任务按条件跳过。测试也不改变 Windows 不支持 Reliable、安装包未签名以及目标环境尚未验收的边界。

### v0.1.5 标签发布验证

2026-11-17，`main` 快进合并至 `853f672fd141dce991d4065fa2c1000fea6ac340`，同一提交的 `v0.1.5` 标签触发 Actions `44075730510`。公共检查、五平台构建、安装验证及便携包烟测均通过；从原始日志汇总的测试数仍为 Linux 两架构各 234 项原生 workspace 与 107 项 musl server / CLI、macOS 两架构各 233 项、Windows 217 项，全部零失败、零忽略。发布任务成功创建 `v0.1.5 Preview`，非草稿、标记为预发布，包含 14 个包及 `SHA256SUMS` 共 15 项资产。

本机重新下载公开发行资产，14 个包合计 83,849,149 字节；逐项比对 `SHA256SUMS` 与 GitHub 资产 SHA-256 摘要，并核对资产大小及上传状态。五份便携包的 MIT 正文与第三方声明通过核对，Linux 含 musl 声明。公开 Windows ZIP 解压后的版本为 `ling 0.1.5`，真实 TCP PING / SET / GET / PTTL / SHUTDOWN 烟测通过。主分支官网流水线 `44075730214` 构建与部署成功。本次发布保留前述顺延项及平台边界，不新增性能提升或目标环境验收结论。

## 0.1.6 供应链与升级验证

2026-11-17，开发分支 `codex/v0.1.6-supply-chain` 在 Windows 11 x64、Rust 1.94.0 完成本机验证：默认配置 workspace 日志报告 225 项、关闭默认 feature 的 workspace 测试 206 项，均为零失败、零 ignored；前者含一项需要 Actions 产物的真实证明测试，本机没有设置产物环境变量，该项提前返回，不计作真实验签通过。两种配置的严格 Clippy、fmt 和严格 rustdoc 通过；Rust 1.88.0 的两种配置 workspace 检查通过。Unix 条件测试未计入本机结果。

Python 输入与构建工具契约 15 项通过；覆盖固定动作与工具版本、错误下载摘要拒绝、归档成员约束、独立构建参数、摘要比较及 Cargo 旧式许可证到 SPDX 表达式的转换。实际 0.1.6 Windows 调试程序生成的两份 SBOM 均含 163 个组件，通过 CycloneDX 1.6 官方 schema 与全部 SPDX 表达式校验。官网构建与检查通过：18 个生成页面，19 个 HTML、773 个本地链接、193 个搜索条目。

固定 `cargo-audit 0.22.1` 和 RustSec 数据库提交 `233155b4025b854c68a4b199c0e32d7fd53291a2` 的审计没有已知漏洞，保留 `rustls-pemfile 2.2.0` 的停止维护提示 `RUSTSEC-2025-0134`。ZIP 升级至 9.6.1 后最低 Rust 版本提高为 1.88，未对旧版 ZIP 公告设置忽略项。

### Actions 构建与来源证明

2026-11-17，提交 `c1070396e1ecb8c6b12c1c233fbcd1d429d20f8e` 的 Actions [`44087465734`](https://github.com/921108257/ling/actions/runs/44087465734) 已完成五平台测试、14 个包构建、原生安装脚本、每包 SBOM 和来源证明。公共检查的 fmt、严格 Clippy／rustdoc、最低 Rust 1.88.0 两种配置、关闭更新器 workspace 测试（228 项）以及 Java／Go TCP 与双向 TLS 契约通过。固定版本和数据库的依赖审计扫描 212 个锁定依赖、1,527 条公告，0 个漏洞；保留上文停止维护提示。数据库提交固定于工作流及日志，审计 JSON 自带的数据库提交／更新时间为 null，不能从这两个字段推断数据库版本。

| 构建目标 | 实际系统 | 默认 workspace 测试 | 额外目标测试 |
|---|---|---:|---:|
| Linux x86_64 | Ubuntu 24.04.5 LTS | 246 | musl server／CLI 119 |
| Linux ARM64 | Ubuntu 24.04.5 LTS | 246 | musl server／CLI 119 |
| macOS Intel | macOS 15.7.9，24G830 | 245 | — |
| macOS ARM64 | macOS 26.7.2，25G332 | 245 | — |
| Windows x86_64 | Server 2022 Datacenter，10.0.20348 | 225 | — |

以上日志均为零失败、零 ignored。包任务的 workspace 统计包含未设置真实 bundle 环境时提前返回的验签入口；之后单独的 Linux／Windows／macOS 验证任务提供真实产物，各实际运行一项 Rust 更新器验证测试，通过有效证明、篡改字节、改名、错误 ref 与缺失证明检查。三个系统还各用独立 Python 驱动，在无令牌、空配置与不可用网络代理下真实验证全部 30 个 subject，并通过六类反向用例；不以模拟验证器代替密码学检查。新增宽松 umask 目录权限与便携替换中断恢复测试已在 Unix 实际运行，不改变父测试进程的 umask。

五份原生安装日志均以 `native_acceptance_exit_code=0` 结束，覆盖首装、服务停止／启动／重启、同版重装、0.1.5 包升级到 0.1.6、卸载注销与配置摘要保留。Unix 同时验证真实 Reliable Journal 的摘要保留；Windows 使用 Fast，验证状态标记及双程序版本，不声称支持 Journal。Ubuntu 上的 RPM 使用 `--nodeps`，仅计兼容演练。Linux AppImage 与五个平台便携包的 PING／SET／GET／PTTL／SHUTDOWN 烟测通过。

实际下载 `verified-release` 全部 31 个附件后，再次通过固定文件集与 SHA-256 校验：14 个包、14 份 CycloneDX SBOM、审计 JSON、`SHA256SUMS` 与 `PROVENANCE.sigstore.json`。全部 SBOM 通过官方 1.6 schema 和 SPDX 表达式校验：Linux 每份 163 个组件，macOS 每份 164 个，Windows ZIP 163 个、含 WinSW 的 Setup 164 个。本机中文 Windows 的独立验签复核发现 Python 子进程默认采用 GBK，补充显式 UTF-8 后再次通过 30 个 subject 及六类反向测试；下载的 Windows 便携包也通过真实 TCP 烟测。该测试脚本编码修正及本节记录不改变上述构建的产品源码。

### 更新器依赖和双构建测量

同一干净提交、Linux x86_64 GNU 目标、Rust 1.94.0 的独立 release 构建结果如下；normal／build 依赖按 crate 名称和版本去重，排除开发依赖与服务端自身，包含构建工具。数字对应本次 GNU 测量，不代替 musl 包体积。

| 配置 | 依赖数量 | `ling` 字节数 |
|---|---:|---:|
| 默认开启 `self-update` | 161 | 9,270,528 |
| 关闭默认 feature | 84 | 6,378,688 |
| 差值 | 77 | 2,891,840（约 2.758 MiB） |

两次独立 target 目录构建固定提交时间、禁用增量并映射源码／输出路径；`ling` 和 `ling-cli` 的大小与 SHA-256 均一致，前后源码保持干净。服务端摘要为 `c1a6131357e9c5dba052f7123cfaada0d4f48f1a99a0552d95da9651ddf387c7`；CLI 为 `17c02f39bb55f8f309fce9ce06274a2b9ef67f83c54499219332d5d5a9561762`，4,692,440 字节。该结果只覆盖同一 runner、同一工具链和同一提交，不证明跨机器或同标签逐位可复现。原始测量、审计、包、验收和证明可从本次运行对应 artifact 复核，本机副本位于 `target/ci-44087465734/`。

上述分支构建的证明绑定开发分支 ref，生产更新器只接受目标版本的标签身份；正式标签与公开更新验证见下节。真实 Windows 11 管理员虚拟机、原生 RPM 系 systemd 主机和交互 UAC 取消仍未执行。托管 Windows Server 2022 和 Ubuntu 上 RPM 兼容安装不能替代前两项。Unix 恢复测试模拟进程中断，不构成物理断电验收；重复恢复按顺序执行，不宣称并发恢复安全。

### v0.1.6 标签发布与公开升级验证

2026-11-17，`main` 快进合并至 `0187e3cb2490c6544b1148d225631bbfbec83ef3`，`v0.1.6` 指向同一提交。Actions [`44092506366`](https://github.com/921108257/ling/actions/runs/44092506366) 第一次运行的公共检查、审计、测量和四个平台通过；macOS ARM64 长时间未分配到运行器，取消后仅重跑该任务与后续步骤。第二次尝试复用其余成功结果，macOS ARM64、证明生成、三系统验签和发布均通过，未移动版本标签。22:05:40（Asia/Shanghai）创建 `v0.1.6 Preview`，非草稿、预发布，含 31 个附件。

标签运行的测试计数与上表一致：两种 Linux 各 246 项 workspace 与 119 项 musl server／CLI、两种 macOS 各 245 项、Windows 225 项；全部零失败、零 ignored。公共关闭更新器测试 228 项、Python 15 项、Java／Go TCP 与双向 TLS、fmt、严格 Clippy／rustdoc 和 MSRV 检查通过。五份原生安装日志退出码均为 0，保留此前平台边界。依赖审计仍为 212 个锁定依赖、1,527 条公告、零漏洞及同一停止维护提示；标签提交的两次独立构建大小和摘要一致，默认／关闭更新器的依赖数和二进制体积与上文相同，仅证明本次同 runner 构建。

三个操作系统都对 `refs/tags/v0.1.6` 的 30 个 subject 完成独立离线验证和六类反向测试，并通过调用真实 GitHub CLI 的 Rust 更新器验证测试；验证了正式标签身份，没有以分支证明替代。

发布后本机重新下载全部 31 个公开附件，固定资产矩阵、包与 SBOM 摘要和 `SHA256SUMS` 均通过核对；14 份 SBOM 通过 CycloneDX 1.6 官方 schema 与 SPDX 表达式验证。独立离线验证再次覆盖全部 30 个 subject 和六类反向用例，Rust 的真实证明测试显式设置公开产物目录和 `refs/tags/v0.1.6` 后通过。公开资产保存在 `target/release-v0.1.6/`。

本机 Windows 在唯一的隔离便携目录下载并核对 0.1.5 ZIP 的官方 SHA-256，再独立验证 0.1.6 ZIP 的摘要、签名、仓库、工作流及精确标签。旧程序实际执行 `ling update --version 0.1.6`，后台 helper 返回 `success`；两个程序均升级至 0.1.6，字节与已验签的新版 ZIP 相同，配置标记摘要不变，TCP PING／SET／GET／PTTL／SHUTDOWN 烟测通过。新版 `update --check` 正确报告版本、证明与验证器可用且已是最新版本。过程未注册或操作系统服务，没有修改现有安装。

该在线升级由 0.1.5 更新器执行，只校验 SHA-256 并替换两个程序，旧许可声明保持原样；0.1.6 ZIP 额外由独立验证器验签。0.1.6 同版本检查不重新下载或安装，因此当时新版“下载、验签、替换”的完整跨版本路径仍待下一版本验证，不能与上述结果混为一项；后续结果见 [0.1.7 公开升级验证](#v017-标签发布与公开升级验证)。原始日志、发行元数据和测量位于 `target/release-v0.1.6-evidence/`，在线升级记录位于 `target/public-update-v0.1.6-5b5bd20512054d5d831fda7c4cd1709b/evidence/`。

## 0.1.7 互操作与 CLI 构建验收

2026-11-17，开发分支 `codex/v0.1.7-interop` 的源码提交 `af8cc1f161f1c356a1b0e8e8824632ef21df3187` 通过 Actions [`44109461968`](https://github.com/921108257/ling/actions/runs/44109461968)。本轮认领的 P0 会话、断线与 CLI 确认契约已完成；公共语言包和可选恢复策略未认领。该次工作流由开发分支手动触发，发布任务按条件跳过，当时尚未合并 `main` 或创建版本标签；后续正式发布见下节。

Windows 本机最终 workspace 默认配置 231 项、关闭更新器配置 212 项，零失败、零 ignored。fmt、两种配置严格 Clippy、严格 rustdoc、Python 23 项工具回归及文档站构建/链接检查通过；release 服务端与 CLI 通过真实 TCP 烟测。本机三语言使用显式 Fast 和 `--no-go-race`，不作为 Reliable 或 race 的证据。普通本机 workspace 未设置产物环境变量时的证明测试提前返回，不算真实验签；最终下载产物后单独运行真实证明测试已通过。

Actions 公共检查在 Linux 实际开启 Reliable 与 Go race；三语言逐项核对结果如下，所有路径的失败、跳过、缺失均为 0。TCP / 双向 TLS 各 105 项包含 39 项离线夹具和 66 项真实服务断言，`local-peer` 单列，不充作 TLS 证据。

| 语言 | local-peer | TCP | 双向 TLS |
|---|---:|---:|---:|
| Rust | 41 / 41 | 105 / 105 | 105 / 105 |
| Go | 41 / 41 | 105 / 105 | 105 / 105 |
| Java | 41 / 41 | 105 / 105 | 105 / 105 |

公共关闭更新器 workspace 为 235 项，Python 工具回归 23 项；fmt、严格 Clippy / rustdoc、Rust 1.88.0 两种配置及 release 依赖图 failpoints 隔离检查通过。平台测试数来自各 job 日志，不把条件编译排除项算作执行：

| 平台 | 原生 workspace 通过 | musl server / CLI 通过 | 失败 / ignored |
|---|---:|---:|---:|
| Linux x86_64 | 253 | 125 | 0 / 0 |
| Linux ARM64 | 253 | 125 | 0 / 0 |
| macOS Intel | 252 | — | 0 / 0 |
| macOS ARM64 | 252 | — | 0 / 0 |
| Windows x86_64 | 231 | — | 0 / 0 |

四个 Unix 原生任务都执行了真实 SIGINT 子进程用例；CLI 还用命名组核对 EOF、显式 ACK、auto-ack、输出失败后的实际重投。同一信号监听贯穿事件和确认阶段，新增测试验证控制行不会消耗监听器。Windows 未发送真实控制台信号，不能由内部取消选择测试推断该路径已验收。

五平台原生包验证退出码均为 0，包含从 0.1.6 安装包升级、配置与状态保留、服务生命周期和便携 TCP 烟测；共生成 14 个包。Windows 为 Server 2022，Linux RPM 测试运行于 Ubuntu，不能替代 Windows 11 管理员安装、原生 RPM 发行版或交互 UAC 取消验收。此处安装包升级与 `ling update` 的公开在线升级分别记录；后者结果见下节。

来源证明的独立验证及更新器真实验证在 Linux、Windows、macOS 均通过。本机下载完整 `verified-release` 的 31 个附件，固定文件集及 SHA-256 全部匹配；14 份 SBOM 通过 CycloneDX 1.6 schema 与 SPDX 表达式校验。离线验证覆盖 30 个 subject 和六类篡改反例，显式设置分支 ref 的 Rust 真实证明测试通过。五份便携包含正确 MIT 正文与第三方声明，Linux 另含 musl 声明；下载的 Windows ZIP 通过 PING / SET / GET / PTTL / SHUTDOWN 烟测。构建包与证明的源码均为上面的 `af8cc1f`，后续验收记录提交不改变该产物身份。

固定工具和数据库的审计报告为 212 个锁定依赖、1,527 条公告、0 个已知漏洞，继续保留 `rustls-pemfile 2.2.0` 停止维护提示。双构建测量的两个程序大小及 SHA-256 一致，仅证明同 runner、同工具链与同一源码的本次结果，不证明跨机器复现或性能提升。原始日志、测量及最终产物位于 `target/ci-44109461968/`；Windows Reliable、平台签名 / 公证、多日压力及物理断电等既有边界不变。

### v0.1.7 标签发布与公开升级验证

2026-11-18，`main` 快进合并至 `958cf25221d34d740e2fa5767c3473c30c133c67`，`v0.1.7` 指向同一提交。标签 Actions [`44128908035`](https://github.com/921108257/ling/actions/runs/44128908035) 首次运行成功，所有必需任务通过，benchmarks 按条件跳过；01:05:07（Asia/Shanghai）发布 [v0.1.7 Preview](https://github.com/921108257/ling/releases/tag/v0.1.7)：非草稿、预发布，含 31 个附件。后续文档记录提交不改变标签及包的源码身份。

标签测试计数与上面的开发验收表一致：两种 Linux 各 253 项 workspace 与 125 项 musl server / CLI、两种 macOS 各 252 项、Windows 231 项，全部零失败、零 ignored。公共关闭更新器 workspace 235 项、Python 23 项、fmt、严格 Clippy / rustdoc、Rust 1.88.0 两种配置及 failpoints 隔离检查通过。Linux 驱动实际开启 Reliable 与 Go race，Rust / Go / Java 各完成 41 项 local-peer、105 项 TCP、105 项双向 TLS，失败、跳过、缺失均为 0；四个 Unix 原生任务的真实 SIGINT 用例通过。五平台原生安装、0.1.6 安装包升级与便携 TCP 烟测均成功，安装验收退出码均为 0。

三个操作系统均对 `refs/tags/v0.1.7` 的 30 个 subject 完成独立离线验签和六类反向测试，并通过 Rust 更新器调用真实 GitHub CLI 的证明验证。公开后重新下载全部 31 个附件，固定文件集、大小、上传状态、GitHub 资产摘要和 `SHA256SUMS` 一致；14 份 SBOM 通过 CycloneDX 1.6 官方 schema 和 SPDX 表达式验证。本机独立离线验证再次覆盖 30 个 subject 及六类反例，Rust 真实验签测试显式设置标签 ref 和产物目录后通过。五份便携包的 MIT 正文与第三方声明内容正确，Linux 含 musl 声明；Windows 许可文件使用 CRLF，核对正文时统一换行。

固定公告库扫描 212 个锁定依赖、1,527 条公告，未检出漏洞，继续保留 `rustls-pemfile 2.2.0` 的 `RUSTSEC-2025-0134` 停止维护提示。审计日志另有 21 项依赖撤回状态索引查询失败，因此不宣称撤回状态检查完整。标签的同 runner 两次独立构建中，服务端 9,272,008 字节、CLI 4,694,680 字节，分别大小及 SHA-256 一致，源码未变化；默认与关闭更新器的服务端分别为 161 / 84 个依赖、9,272,008 / 6,380,272 字节。该结果不证明跨机器可复现或版本性能提升。

本机 Windows 在唯一隔离便携目录中，先独立验证公开 0.1.6 与 0.1.7 ZIP 的摘要和精确标签证明，再实际执行旧程序的 `ling update --version 0.1.7`。0.1.6 更新器完成在线下载、来源证明验证及后台替换，helper 返回 `success`、退出码 0；安装后的两个 EXE、`LICENSE`、`THIRD-PARTY-NOTICES.txt` 与已验签 0.1.7 ZIP 字节一致。配置标记摘要与其他便携文件保持不变，两个程序报告 0.1.7，真实 TCP PING / SET / GET / PTTL / SHUTDOWN 烟测通过；新版 `update --check` 报告已是最新版本且证明和验证器可用。此次特意将隔离安装中的旧许可文件改为标记内容，验证它们确实刷新；没有注册或操作系统服务，也未修改用户安装。

公开附件位于 `target/release-v0.1.7/`，标签日志与测量位于 `target/ci-44128908035/`，发行元数据和完整日志位于 `target/release-v0.1.7-evidence/`，在线升级证据位于 `target/public-update-v0.1.7-2a596587a3d2460fbe2dfb87ddecf58e/evidence/`。在线升级只覆盖本机 Windows 便携路径；Windows 11 管理员安装、原生 RPM 发行版、交互 UAC、平台签名 / 公证、Windows Reliable、多日压力及物理断电等既有未验收项仍保留。

## 0.2.0 首轮通知对照（2026-11-18）

产品候选 `2b9503f9289303c8d916f1b32268fa5b22b912bd` 与基线 `89076a4a98b4cd72a732d48eebfc4938d09c7921` 使用冻结工具 `8ecb2aa` 对照。完整原始配置、环境、逐次结果和延迟样本位于 [v020-2026-11-18](../benchmarks/results/v020-2026-11-18/summary.json)。Linux Actions `44144974913` 在同一运行器交错执行 legacy/off/on，各三次、每场景五秒，覆盖 Fast / Reliable 的积压与 0/1/100 空闲订阅，共 72 场景；Windows 仅 Fast，共 36 场景。108 场景功能判断通过，但不等于性能验收通过。Windows 记录的工具工作区不干净，分别保留二进制身份；Linux 干净提交构建是主要对照证据。所有二进制仍显示版本 0.1.7，不能仅凭版本文本识别产品候选。

通知开启后，Linux Fast 积压确认速率为 48.712/48.722/48.945 次每秒，基线为 100.349/100.395/100.435；Reliable 为 48.597/48.885/48.889，基线为 101.327/101.433/101.535。关闭通知恢复到约 100–102 次每秒。此约 52% 的一致回退阻断发布，须定位并复测；不能用所有功能场景通过掩盖。Linux 100 个空闲订阅的 PING 吞吐中位数约下降 15%，P99/P99.9 与 CPU 时间下降，不能仅据 CPU 降低宣称整体提升。Windows 开启通知后的积压吞吐反而提高，且 100 订阅 P99.9 中位数约上升 15.8%，不把跨平台相反表现合并成单一结论。

开启通知时两平台无事件且无 pending 的空闲场景 `subscription_poll_delta=0`；关闭时仍为 10 ms 派发。每次独立计算最近秩延迟分位数，不合并样本。积压场景只记录确认吞吐，没有请求延迟样本。五秒短样本不能代表持续容量、物理持久性或目标部署环境。

### 0.2.0 修复后对照与发布取舍

产品提交 `508d39c073a92f420627d7ea0a35dfa5a4d94567` 在已准入 TCP 上、TLS 包装前启用 `TCP_NODELAY`。Linux Actions `44148898104` 使用同一干净运行器交错执行 legacy / off / on，各三次；Windows 另以干净副本构建并执行相同对照。完整原始记录见 [修复后归档](../benchmarks/results/v020-final-2026-11-18/summary.json)：Linux 72、Windows 36 场景均功能有效，81 份延迟样本的最近秩 P50 / P99 / P99.9 重算一致。Windows 候选源码为 `b5459bc`，相对产品提交只增加证据换行保护；全部二进制仍显示 0.1.7，身份以记录的源码、二进制和工具摘要区分。

Linux 通知开启的 Fast 积压确认速率为 3534.698 / 3533.389 / 3535.995 次每秒，Reliable 为 2496.073 / 2452.676 / 2544.512；基线中位数分别为 100.346 / 101.478，关闭通知为 101.116 / 102.335。首轮约 52% 回退未再出现。没有抓取数据包或逐 ACK 延迟，不能把结果当成对某个 TCP 定时器机制的直接证明。

100 个空闲订阅的 Linux Fast / Reliable，开启通知相对基线的 PING 吞吐中位数仍下降 11.45% / 13.38%，P99 下降 2.88% / 0.68%，P99.9 下降 11.14% / 8.09%，服务 CPU 时间下降 21.10% / 22.05%。这些是各指标的取舍，不代表请求性能全面提高。Windows 五秒样本的同场景 P99 / P99.9 中位数上升 31.24% / 12.41%，因此追加安静窗口下的三次 30 秒复测。

Windows 补测使用相同干净源码及固定二进制，保留原电源计划，不并行构建、测试或 WSL 负载；九次均功能有效，原始证据见 [30 秒补测](../benchmarks/results/v020-final-2026-11-18/summary-tail-30s.json)。以下为每次独立计算分位数后三次结果的中位数，不合并延迟样本：

| 派发方式 | PING P99（微秒） | PING P99.9（微秒） | 吞吐（次/秒） |
|---|---:|---:|---:|
| 0.1.7 基线 | 97.1 | 137.7 | 19127.05 |
| 候选关闭通知 | 91.5 | 126.0 | 19217.36 |
| 候选开启通知 | 121.4 | 200.5 | 21271.78 |

补测确认本机 Windows Fast 在 100 个空闲订阅下仍有 P99 +25.03%、P99.9 +45.61% 的回退，不能归为五秒样本噪声，也不以吞吐提高抵消。Preview 接受并公开此限制，保留默认通知及 `events.notification_enabled=false` 回退选项；需要优先控制该场景尾延迟的部署应据实际负载选择关闭。当前验收没有为通知设定所有性能指标不得下降的硬门槛，组提交和流水线未实施，其专项门槛不套用于本项。此决定不宣称所有部署可接受该取舍。

全部测量为同机回环、Python 闭环客户端；五秒及三十秒数据不代表持续容量或目标部署性能。Linux runner 的 ext4 使用 `data=writeback,nobarrier,journal_async_commit`，仅能观察软件同步调用，不能验证物理掉电持久性。Windows 没有 CPU 亲和性隔离，进程 CPU 计数有量化和采样差异，不用它推导精确收益。Windows Reliable 未运行，不计通过。

版本元数据改为 0.2.0 后，本机 Windows 再次执行完整默认 workspace 测试、fmt、严格 Clippy、严格 rustdoc、Python 发行工具回归及 release-notes 校验，均通过；原始日志为 `target/v020-metadata-*.log`，退出码为 `target/v020-metadata-checks.json`。站点实际构建为 0.2.0，检查 20 个 HTML、834 个本地链接和 200 个搜索条目通过。标签构建、跨平台发行及公开制品验收单独记录，不能用这次本机检查替代。

标签 `v0.2.0` 固定提交 `441ffc7dbb2bf48d6257c0ca3b831f099554af1a`，首轮 Actions `44154274480` 的 Linux x86_64 CLI 输出失败契约在 `end_to_end.rs:455` 失败：子进程成功退出、stderr 为空，未出现预期的输出错误。原始日志保存在 `target/ci-44154274480/package-linux-x86_64.log`。同测试源码在 WSL 的 200 轮、每轮 11 项并发运行全部通过，记录为 `/root/ling-acceptance/cli-output-repeat-20261118/`。独立 fork / 管道实验确认：父进程关闭读端时，exec 前的其他子进程若仍持有继承读端，写入可以成功；这说明原测试的“唯一读端”前提没有保证，但没有捕获该 CI 进程的实际文件描述符，因此只记疑似测试竞态。

后续 `main` 的 Linux 测试改为 `/dev/full` 确定产生写错误，其他平台保留原管道检查，继续验证输出失败不得 ACK；只修改测试，不改变 `v0.2.0` 标签或产品代码。修改后的本机 Windows 10 项、WSL Linux 11 项 CLI 端到端测试通过。首次失败与后续标签重跑的结果分别保留，重跑通过不能当成已证明首次失败根因。

### 0.2.0 标签发布与公开制品核验

2026-11-18，标签 `v0.2.0` 始终固定于 `441ffc7dbb2bf48d6257c0ca3b831f099554af1a`。Actions [`44154274480`](https://github.com/921108257/ling/actions/runs/44154274480) 首轮 Linux x86_64 契约失败，macOS ARM64 因未获得托管运行器而被平台取消，其余七个已执行任务成功。仅执行一次失败任务重跑；第二次尝试复用首轮成功结果，最终 14 个必需任务成功，benchmarks 按条件跳过。03:36:59（Asia/Shanghai）发布 [v0.2.0 Preview](https://github.com/921108257/ling/releases/tag/v0.2.0)，非草稿、预发布，共 31 个附件。首轮 macOS ARM64 没有获得 runner 或执行步骤，不能归为产品测试失败；Linux 失败及夹具边界保留在上文。

下表是测试执行次数，不合并视为唯一测试数；首轮 Linux 失败前的 19 通过、1 失败（其中 CLI 10 通过、1 失败）单独保留。

| 步骤 | 通过 | 失败 / 忽略 |
|---|---:|---:|
| Linux x86_64 workspace，重跑 | 266 | 0 / 0 |
| Linux x86_64 musl server / CLI | 132 | 0 / 0 |
| Linux ARM64 workspace | 266 | 0 / 0 |
| Linux ARM64 musl server / CLI | 132 | 0 / 0 |
| macOS Intel workspace | 265 | 0 / 0 |
| macOS ARM64 workspace，重跑 | 265 | 0 / 0 |
| Windows workspace | 242 | 0 / 0 |
| checks，关闭 updater workspace | 248 | 0 / 0 |
| SDK 脚本内 Rust 测试 | 1 | 0 / 0 |
| 三系统真实 provenance 测试 | 各 1 | 0 / 0 |

Python 发行工具回归 23 项通过。Rust、Go、Java 的共享用例分别完成 local-peer 41 项、真实 TCP 105 项及双向 TLS 105 项，均无失败、跳过或缺失。五个平台 native acceptance 日志均为退出码 0，覆盖原生安装、安装器升级与卸载；CI 内实际 `ling update` 原生升级均明确未执行，需要已发布的 `UPDATE_VERSION`，不能以安装器升级或证明验证代替实际自更新验收。

三个操作系统分别独立验证 30 个来源证明 subject，并通过六类反向检查；Rust 更新器调用真实 GitHub CLI 的验证测试也均通过。供给链与发布步骤核对了 14 个包、14 份 SBOM、审计报告、校验文件及来源证明。发布后本机重新下载全部 31 个附件，固定名称集合、上传状态、大小、GitHub SHA-256 与 `SHA256SUMS` 一致；14 份 SBOM 通过 CycloneDX 1.6 官方 schema 及 SPDX 表达式校验，五份便携包的 MIT 正文与第三方声明核对通过。本机再次完成 30 个 subject 的离线验签、六类反例及 Rust 更新器真实验证测试；公开 Windows ZIP 与 Linux x86_64 便携包均通过 PING / SET / GET / PTTL / SHUTDOWN 烟测。

锁定依赖审计包含 212 个依赖、1,527 条公告，公告库固定提交 `233155b4025b854c68a4b199c0e32d7fd53291a2`，报告漏洞数为 0，并保留 `rustls-pemfile 2.2.0` 未维护提示 `RUSTSEC-2025-0134`。原始日志另有 21 项依赖因索引未找到 crate 而无法查询撤回状态，因此撤回状态检查不完整。

同一 Linux GNU runner、同一干净源码的两个独立 target 构建中，服务端均为 9,319,904 字节，CLI 均为 4,694,872 字节，各自 SHA-256 一致；这只证明本次同 runner 结果，不外推跨主机可复现。默认与关闭 updater 的依赖计数分别为 161 和 84，关闭 updater 的服务端为 6,433,072 字节。

两次尝试的完整日志 ZIP、API 元数据、取消注释、公开核验日志与资产证明见[发行证据清单](../benchmarks/results/v020-release-2026-11-18/manifest.json)。完整下载保留于 `target/release-v0.2.0/`，展开日志位于 `target/ci-44154274480/`。后续文档或夹具提交不改变已发布标签。实际内置升级至 0.2.0、Windows 11 管理员安装、原生 RPM 发行版、交互 UAC、签名 / 公证、Windows Reliable、多日压力和物理断电均不在本次已通过清单。

## 0.3.0 WSL 崩溃恢复（2026-11-18）

在专用 `ling-acceptance` WSL2 / ext4 环境，以已核验的公开 0.2.0 Linux x86_64 制品完成 30 轮强制终止恢复，seed `3002030`，每条 MSET 含 8 个唯一 Key。宿主持久记录的 899 条成功 MSET（含每轮一条预热）对应 7,192 个事件，均在恢复中保留。30 条结果未知的 MSET 中，3 条完整提交、27 条未提交，没有部分事务；原始 Journal 合计 7,216 个事件。二进制 SHA-256 为 `70b209554df2f236c6c4c30d6bbae5ec001a3b2754761038265483f659a0f285`，对应发布提交 `441ffc7dbb2bf48d6257c0ca3b831f099554af1a`；工具工作区 `344f6f1` 与产品身份分列。

1,328 个已确认 ACK 均保留，未被重投；30 个结果未知 ACK 中，磁盘显示 10 个已确认、20 个未确认，恢复投递与磁盘状态一致。60 个刻意保留的 ACK 空洞均恢复，且每个空洞后存在同分片已确认事件。审计组恢复 7,216 个事件，工作组恢复 5,878 个事件，无重复或身份、内容变化。重启后的 240 个后继事件保持原 stream_id，并从各自分片尾部继续连续编号。30 轮全部通过，安全拒绝和测试失败均为 0。

独立只读审查核对了全部宿主意图 / 成功 / ACK 记录、原始 Journal 与 330 项崩溃后和最终文件摘要；[原始归档与清单](../benchmarks/results/v030-crash-2026-11-18/manifest.json)包含完整 577 个文件，[逐轮结果](../benchmarks/results/v030-crash-2026-11-18/summary.json)保留未知结果与终止边界。30 份崩溃 Journal 均为完整记录，本轮没有实际触发短尾修复。终止请求时 writer 均仍运行，请求在途标志为 14 次 true、16 次 false，不能据此声称每轮均命中请求执行或 fsync。重启空缓存检查由工具执行并通过，但原始 DBSIZE 响应未单独保存；恢复日志记录正常停止，恢复进程退出码未单独保存。

本次环境为 Windows 11 `10.0.22631`、i7-11800H、WSL2 内核 `6.18.40.1`、ext4 `/dev/sdd` 和 `data=ordered`。结果仅覆盖此 WSL 强制终止组合，不代表物理掉电、设备易失写缓存或其他部署环境。存储故障、72 小时正式压力及物理断电另行记录，不由本轮推导通过。

## 0.3.0 WSL 存储故障（2026-11-18）

在同一专用 WSL 中，以公开 0.2.0 制品运行独立存储套件。六个主场景中五个通过，启动恢复场景内两项通过、两项诊断失败，整套退出码为 1；不能将其写成全部通过。冻结驱动在 Windows、WSL 各完成 18 项离线工具测试。[完整原始文件、宿主执行记录及工具副本](../benchmarks/results/v030-storage-2026-11-18/manifest.json)保留失败结果，原始归档 SHA-256 为 `5e546c36bfe9b6b48db2a20c94e19b256e2414a7e9026662d5f15dd819c2f25d`。

已通过的实测包括：停服目录副本恢复相同 stream_id、ACK 空洞和后继序号，缓存为空；不同拓扑及目录独占拒绝；非 root 真实只读权限拒绝；1 MiB Journal 配额、拒绝写入的原子性及扩大预算后的副本恢复；运行中短头和 CRC 损坏传播为全局 Journal 故障并唤醒空闲订阅。配额场景最后成功写入 365，Journal 为 393,190 字节、pending 为 4、journal_errors 为 0。

私有 mount namespace 内的 64 MiB ext4 镜像实际耗尽，strace 记录 `write = -1 ENOSPC`。第 65 次写返回 `ERR_COMMIT_UNKNOWN`，后续写返回 `ERR_JOURNAL_UNAVAILABLE`，GET 仍为已确认值 64；订阅先收到明确故障，之后 ACK 遇到 EOF，因此该 ACK 属于未确认，不能报告为收到稳定错误响应。保存后的故障目录副本因模糊尾拒绝启动，未改写原 Journal。该实验验证磁盘写满路径，不等于强制 fsync EIO 或物理断电。

启动修尾实际覆盖两例：在完整 1,330 字节 Journal 后追加 2 字节短记录头，以及追加完整载荷和 CRC 但缺最后 4 字节的提交尾。两者恢复成功，DBSIZE 为 0，修复后逐字节等于原完整前缀。另两例为偏移 1,240 的模糊载荷尾和偏移 151 的中段 CRC 损坏：均以退出码 1、`ERR_JOURNAL_UNAVAILABLE` 安全拒绝，文件大小和 SHA-256 不变，但公开启动诊断丢失规范要求的字节偏移。底层错误保留偏移，服务端启动诊断只显示静态错误码译文，故为真实诊断契约缺陷。处理结论为 P2：本轮 Preview 接受并明确列为限制，后续修复；不修改原失败记录，也不将拒绝与保原件的通过推导为诊断通过。跨版本升级回退、跨主机迁移及其他物理文件系统均未由本轮验证。

## 0.3.0 开放到达率负载（2026-11-18）

在专用 WSL 中使用相同公开 0.2.0 制品，Fast / Reliable 各独立三轮。每轮顺序运行基线、名为 overload 的高到达率段、降载段，各 60 秒，计划到达率分别为 100 / 5000 / 100 次每秒，PING / SET 各半。使用 32 个连接、有界队列 512、调用总期限 2 秒、服务端请求期限 50 ms，服务端 CPU 为 `0,1`，驱动为 `2,3`。overload 是阶段名称，不表示所有模式都已达到容量上限；本机 Fast 完成全部计划请求。

六轮均正常停止，逐请求身份、计划到达、接受/丢弃及最终结果对账一致；原始数据、独立分析器和清单见[开放负载证据](../benchmarks/results/v030-open-2026-11-18/manifest.json)。工具的 `passed=true` 表示记账完整、最终恢复探测通过及正常退出，不能解释为零产品错误。每轮完整计划 312,000 次，下表只列其中计划 300,000 次的高到达率段。

| 模式 / 轮次 | 成功 | 测试端队列丢弃 | 连接 EOF | 其中结果未知 SET |
|---|---:|---:|---:|---:|
| Fast / 1 | 300000 | 0 | 0 | 0 |
| Fast / 2 | 300000 | 0 | 0 | 0 |
| Fast / 3 | 300000 | 0 | 0 | 0 |
| Reliable / 1 | 19769 | 269981 | 10250 | 4121 |
| Reliable / 2 | 19799 | 270008 | 10193 | 3936 |
| Reliable / 3 | 19618 | 270118 | 10264 | 4094 |

六轮基线均完成 6,000 次请求。Fast 降载段各完成 6,000 次；Reliable 降载段成功分别为 5,954 / 5,953 / 5,959，队列各丢弃 1 次，EOF 分别为 44 / 46 / 40，第一轮另有 1 次 `ERR_BUSY`。Reliable 全程合计 12,196 个 SET 的结果未知，均在请求阶段遇到 EOF；未核对这些写入的最终提交情况，也未自动重放，不能当作写入失败或丢失。测试端队列丢弃表示未入队，不是服务端拒绝；请求调用开始也不证明完整帧已发送。

最终探测在降载段结束且队列排空后才开始，通过条件为三个新连接 PING 连续低于 100 ms（含连接与 HELLO）。Fast 在计划到达结束后约 0.252 秒、Reliable 约 0.261–0.263 秒确认通过；从高到达率段结束计算则约为 60.252 / 60.261–60.263 秒。该探测不能定位过载后首次恢复时刻，不能把后者改写成约 0.26 秒恢复。延迟分位数为直方图桶上界；Python 调度、编码及原始记录会影响负载，此结果不是服务器独立容量或多日稳定性结论。连接错误与结果未知作为本次过载行为保留，不据工具成功旗标消除。

## Redis 对比边界

只比较语义可比的String/Bytes GET/SET等工作负载。相同数据量、连接、流水线、TTL与持久性条件下运行；自定义类型、命令差异和额外事件成本单列说明。

“吞吐达到 Redis 8 的两倍”仍是研究目标，不是测试断言。Reliable 必须与具有对应持久化成本的设置比较；如果无法完全等价，明确列出差异，不制造单一排名。

## 稳定版门槛

全部首版公开契约有对应测试；目标环境的压力、资源耗尽和故障恢复测试无未解决的数据一致性问题。长时间运行的时长、负载与资源预算在执行前固定并记录，不以“跑过一会儿”宣称生产可用。

文档中的能力与实际构建一致，CLI及三种SDK跑同一契约。性能未达到研究目标可以如实报告，不通过弱化TTL、关闭fsync或丢弃事件换取好看的数字。

Rust 构建、测试与 lint 入口已记录于本文及 README。Java / Go 的真实构建与网络验证由下述共享驱动执行，不将跳过的用例当作通过证据。
