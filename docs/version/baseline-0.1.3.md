# 0.1.3 基线审查

> 状态：审查记录，写于 2026-11-17。基线为 `main`@`53a9f46`（Cargo.toml 版本 0.1.3）。本文登记发现、证据、影响与计划处理版本；条目编号被各版本文档引用，不重用。索引见 [README.md](README.md)。

## 方法与边界

本轮清点 `git ls-files` 的 181 个跟踪文件（9 个 workspace 成员），另读本目录现有规划。按架构串读类型/协议/网络、引擎与事件、服务端、CLI、三种客户端、打包更新、CI、官网与阶段 7 原始数据；重点追调用链，不宣称每行均经过动态验证。

| 证据 | 本轮结果（Windows，0.1.3） |
|---|---|
| 行为复核 | 注入成对时钟复核 R-13；通过存储/引擎公开接口复核 R-10、R-14。三项结果及未量到的部分分别列在条目内。 |
| Rust | `cargo test --locked --offline` 的 types/protocol/network/cli-help 子集、server 的 config、client 的 client 集成测试，以及 Windows 拒绝 Reliable 的专测通过。另跑 engine/types/protocol 组合时，engine 的 events 集成测试 1 过、2 项因 Windows 不支持 Reliable 失败；不是全量通过。E-08 的全 workspace 数字是此前记录，不重复计入本次。 |
| 跨语言 | 在两个新启动的本机 Fast 服务上分别设置 `LINGER_TEST_ADDR`：Go `go test -count=1 -v ./...` 实际执行 `TestLiveContracts`；Java `javac -Xlint:all -Werror` 后 `java -ea … io.linger.cache.Contracts` 通过。只验证明文 TCP；不是 Unix 上 Reliable / 双向 TLS 驱动的结果。 |
| 优化与平台 | M-09 只重新统计仓库内历史 JSON，没有重跑性能基准。未做 Linux/macOS 原生 Journal、原生安装/升级、多日压力或物理断电验证。 |

**实测**是观察到的结果，**代码**是定位到的实现，**文档**是规范或既有记录，**推断**仍需验证。L1：影响正确性、可靠性或服务整体可用；L2：明显的可用性、安全或性能退化；L3：工程与维护成本；L4：文档不一致。L 不是版本 P0/P1 优先级。

## R：稳健性

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| R-01 | 接入循环遇到任何 accept 错误都整体退出，句柄耗尽（EMFILE）或连接在接受前被对端重置这类瞬时错误会让整个服务停止。默认 `max_connections` 为 10,000，高于常见的 1024 软句柄上限，当前 `packaging/linux/ling.service` 未设置 `LimitNOFILE`；实际软上限取决于部署环境 | 代码：`crates/server/src/lib.rs:177`；推断：句柄上限 | L1 | v0.1.4 |
| R-02 | 连接任务 panic 时服务整体以错误退出；而引擎锁中毒后，请求只会得到 ERR_BUSY 或结果未知，服务大概率继续运行在不明确的状态。两条路径没有统一的故障策略 | 代码：`lib.rs:173-175`、`connection.rs:598-604`；文档：`engine/src/lib.rs:270-278` 的说明 | L1 | v0.1.4 定策略 |
| R-03 | 超过 `max_connections` 的连接被直接丢弃。客户端只看到 EOF 或重置，分不清是限流还是网络故障；服务端没有计数与日志 | 代码：`lib.rs:178` | L2 | v0.1.4 可见性；v0.2.0 ERR_BUSY 帧 |
| R-04 | 未完成 HELLO 的空闲连接可占用连接许可直到 `idle_timeout_ms`（默认 300 s）；TLS 握手同样占用许可，期限复用 `shutdown_timeout_ms`。默认连接上限可被慢速连接占满 | 代码：`lib.rs:178-189`、`connection.rs:614-628` | L2 | v0.1.4 首帧期限；v0.2.0 握手期限配置化 |
| R-05 | `server.pipeline_enabled`、`server.max_inflight_per_connection`、`event_journal.group_commit_ms`、`ttl.precision_ms` 能通过校验却没有运行时效果：连接逐条串行，Journal 逐事务 fsync，精度固定为 1 | 代码：这些字段只出现在 `config.rs` 与测试；文档：`config.md` 已声明其中部分 | L3 | v0.1.4 标注；v0.2.0 实现 |
| R-06 | 生产路径用 unwrap / expect 维护不变量，例如 Reliable 路径假定 journal 存在、分片已加锁。不变量一旦被破坏就 panic，并经 R-02 的路径放大 | 代码：`events/src/store.rs` 8 处、`engine/src/transaction.rs` 8 处、`events/src/journal.rs` 3 处 | L3 | v0.1.4 审阅 |
| R-07 | 维护循环对未列入容忍名单的错误一律返回 Err，接入循环随之整体退出。名单只有 ERR_JOURNAL_FULL、ERR_JOURNAL_UNAVAILABLE、ERR_COMMIT_UNKNOWN；系统时钟早于 Unix 纪元时读取失败，被映射为 ERR_BUSY，不在名单内，会使服务停止。config.md 只规定维护错误仅记录稳定错误码，没有规定哪些错误应终止服务，该策略需与 R-02 一并定下 | 代码：`crates/server/src/maintenance.rs:47-53,65-79`、`crates/server/src/lib.rs:167-171`；文档：config.md 日志一节 | L2 | v0.1.4（工作包 4.1） |
| R-08 | 指标端点的 accept 错误结束后台任务，接入主循环随之退出；观测端点的瞬时故障会带停数据服务。 | 代码：`crates/server/src/operations.rs:444`、`crates/server/src/lib.rs:167-171` | L1 | v0.1.4 工作包 1.1：隔离端点故障并验证数据连接仍可用 |
| R-09 | 启动恢复、运行期消费和按 ID 查找共用可截断文件的遍历器。读到不足一条记录的剩余字节会改短文件；仅凭 v1 长度不能总是分清真正的短尾与已提交区的长度损坏。此项是已读到的写盘路径，不是实测的数据丢失。 | 代码：`crates/events/src/journal.rs:142-211`、`crates/events/src/store.rs:855-894`；契约：[恢复边界](../persistence.md) | L1 | v0.1.4 工作包 4.3：运行期校验只读、可疑恢复保留原件；若需要更多格式证据另立版本 |
| R-10 | 同一过滤条件在订阅与遍历给出相反结果：订阅的星号回溯没有在遇到下一个星号时重置起点。 | 实测（Windows，Fast）：对 `*xy*yz` 与 `xyz`，KEYS 和 SCAN 均不命中，订阅却交付该 Key。代码：`crates/engine/src/scan.rs:67-97`、`crates/events/src/store.rs:976-1005` | L2 | v0.1.4 工作包 4.4：按同一组正反例比较三条路径 |
| R-11 | 读取帧头后即按完整声明大小分配缓冲；单连接的上限不能约束多个尚未完成握手的连接合计占用。未测 RSS。 | 代码：`crates/protocol/src/transport.rs` 的 `read_frame`、`crates/server/src/connection.rs` 的读任务；[资源契约](../config.md) | L1（外部可达部署） | v0.1.4 工作包 1.7：分段读、全局在途字节预算与释放验证 |
| R-12 | 命令参数循环反复复制并转大写同一命令名，解码开销同时随名字长度和参数数目增长；帧长限制不能把这项开销约束为线性。未测服务延迟。 | 代码：`crates/protocol/src/codec.rs:258-282`、`crates/protocol/src/transport.rs` | L1（外部可达部署） | v0.1.4 工作包 1.7：一次规范化、预算内完成；v0.1.5 工作包 6.3 保留回归语料 |
| R-13 | 同一绝对毫秒期限重复设置时，Unix 时间按毫秒取整，单调时钟仍有亚毫秒精度。直接比较两个完整 Deadline 会移动实际到期点并增加 revision；按提交路径还会产生多余 TTL_CHANGED。 | 实测（注入时钟）：同一 PEXPIREAT，Unix 期限保持 15000 ms，单调到期推迟 250 µs，revision 从 2 变 3；未计事件数。代码：`crates/engine/src/clock.rs:35-49`、`crates/engine/src/ttl.rs:21-43,122-156`；SET 路径另见 `transaction.rs:90-100` | L1 | v0.1.4 工作包 4.5：绝对期限幂等、事件数与真实改期对照 |
| R-14 | 建组只校验空组，首次待确认记录还需 256 B；超过共享额度时 next 返回空。单组、无待 ACK、没有可释放状态时也会持续表现为没有消息。规范允许临时背压，问题是接受了在此配置下不能推进且无法辨别原因的订阅，不等于丢事件。 | 实测（Fast 存储接口）：4 KiB、1 库 6 分片、1 个命名组、单组窗口为 1，提交一条匹配事件，三次 next 均为空、pending 为 0。代码：`crates/events/src/store.rs:73-89,472-500,604-609,772-795`；[事件预算](../config.md) | L2 | v0.1.4 工作包 4.6：区分永久无法首次交付与暂时背压 |

全仓 unwrap / expect 共 526 处，绝大多数在测试代码；上表只列已确认处于生产路径的文件。

## S：安全与供应链

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| S-01 | AUTH 用 `==` 比较明文密码，不是常量时间比较；失败后没有延迟、计数或断开，可无限在线猜测。失败的重新认证会撤销既有认证，这一点是正确的 | 代码：`crates/server/src/connection.rs:524`；以 password、ct_eq、auth_failures、backoff 搜索服务端源码，只命中配置、该比较与脱敏 | L2 | v0.1.4 |
| S-02 | 内置更新的完整性来源只有同一 GitHub Release 的 `SHA256SUMS`。资产与校验和同源，Release 被篡改时校验起不到保护作用；以 signature、minisign、cosign、attest 搜索 `update.rs` 无命中，即未检出签名验证 | 代码：`crates/server/src/update.rs:21-22,162`；文档：install.md | L2 | v0.1.6（DEC-02） |
| S-03 | CI 下载 appimagetool 后没有校验摘要，同一脚本对 nfpm 做了 checksums 校验。该工具参与生成发行的 AppImage | 代码：`.github/workflows/release.yml:138-143` | L2 | v0.1.4 |
| S-04 | 没有暴露面告警。监听非回环地址且未启用认证时，任何可达客户端都能执行 SHUTDOWN、CONFIG SET、FLUSHALL；启用认证而未启用 TLS 时密码明文传输；Prometheus 是明文、无认证的 HTTP | 代码：`crates/server/src/lib.rs:133-146` 的启动日志只含版本、地址、分片数与事件模式；文档：commands.md 说明认证是实例级权限，config.md 说明明文链路不保密。`config.rs` 的校验未读完，实现前先核实是否已有告警 | L2 | v0.1.4 |
| S-05 | 密码以明文保存在配置文件。系统安装后 `ling.toml` 的属主与权限由 postinstall 脚本和 Windows 安装器决定，本次未读 | 推断 | L2 | v0.1.4 核查 |
| S-06 | 没有依赖漏洞审计（无 `deny.toml`，CI 无 audit 步骤）。服务端二进制链接 reqwest、压缩与归档库，用途只是内置更新，扩大了依赖与攻击面 | 代码：各 crate 的 Cargo.toml、`release.yml` | L3 | v0.1.4 审计；v0.1.6 评估 feature 化 |

S-02 的签名方案取决于 DEC-02 与 DEC-03。决定之前，v0.1.6 只做不依赖密钥的部分，例如构建来源证明与发行资产的独立复核。

## P：性能与资源

本节数值取自 [testing.md](../testing.md) 的既有记录，不是本次重测。P-02 至 P-04 的成因来自源码路径判断，没有剖析数据支持（阶段 7 记录 `perf_event_paranoid=4` 拒绝了性能计数器），按推断对待，由 v0.1.5 证实或否定。

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| P-01 | TTL 维护循环空闲时仍约每 1 ms 唤醒一次：睡眠 1 ms、取得执行许可、派发一次阻塞的 `expire_due`；成功后延迟复位为 1 ms，没有「无到期项则退避」的分支。空闲服务持续消耗 CPU，并与请求竞争许可。持续运行实验中 Fast 十分钟全进程 CPU 累计 53.14 秒（约 80 请求/秒），其中维护循环的占比未单独测量 | 代码：`crates/server/src/maintenance.rs:18-39,60-64`；文档：testing.md 持续运行 | L2 | v0.1.4 |
| P-02 | 订阅推送靠 10 ms 定时轮询：每次 tick 经许可与阻塞线程池调用 `store.next`，至多发出一个事件。推断单订阅上限约 100 事件/秒，空闲时也持续派发阻塞任务。与阶段 7 中 Reliable 快消费者 3 秒约 302 个 ACK（约 100/秒）相符，但该数据也可能有别的解释 | 代码：`crates/server/src/connection.rs:44-45,63-78`；文档：testing.md | L2 | v0.1.5 测量；v0.2.0 改唤醒驱动 |
| P-03 | 服务端接受的连接没有设置 TCP_NODELAY；仓库内只有 Rust SDK 设置了 `set_nodelay(true)`。Java SDK 的套接字选项未检查。响应是一次编码后的一写，多数请求应答场景下影响可能很小，需要实验 | 代码：搜索 `nodelay` 仅命中 `sdks/rust/src/lib.rs:176` | L3 | v0.1.5 |
| P-04 | 每条数据命令都经「信号量许可 → `spawn_blocking` → 回到异步任务」，两次线程切换。引擎直调 GET 的 P50 为 1 μs，TCP GET 为 172 μs（虚拟机、同机回环），差距在编解码、TCP 与调度之间如何分配没有分解 | 代码：`connection.rs:555-606`；文档：testing.md 阶段 7 表 | L2 | v0.1.5 |
| P-05 | Reliable 逐事务 fsync、没有组提交。阶段 7 的 TCP Reliable 无消费者为 2,815 请求/秒，P99.9 为 30,720 μs；事务在持有执行权期间完成 fsync，并发事务不能合并。`group_commit_ms` 只校验不生效（R-05） | 文档：testing.md、persistence.md、config.md | L2 | v0.2.0 |
| P-06 | 启用淘汰且有内存或 Key 上限时，写入锁全实例；带每库 Key 配额时锁全库。除热点分片外的并发度退化没有数据 | 文档：architecture.md、config.md | L2 | v0.1.5 测量 |
| P-07 | 受管字节与 RSS 缺少校准：大值场景峰值 RSS 43,057,152 B，清库后仍有 26,152,960 B（启动时 5,795,840 B），属于分配器滞留的观察，但没有按 Key 数与 Value 大小扫描的曲线，也没有把事件缓冲、Journal 页缓存单独归因 | 文档：testing.md | L3 | v0.1.5 |
| P-08 | 受管字节计费保守：64 B Value 每 Key 计 3,970 B（阶段 3 为 2,673 B），4 KiB 为 8,002 B，64 KiB 为 69,442 B。README 示例同时配置 `max_memory_bytes = 268435456` 与 `max_keys = 100000`，按 3,970 B/Key 约 67,600 个 64 B Key 即触顶，`max_keys` 达不到。计费系数的构成没有文档 | 文档：README、testing.md；计算：268,435,456 ÷ 3,970 ≈ 67,616 | L3 | v0.1.4 文档；v0.1.5 校准 |
| P-09 | 锁定范围比文档描述的更宽。`locations()` 中：启用淘汰且配置了内存或 Key 上限时，凡非只读命令（包括不扩大内存的 EXPIRE、PERSIST、DEL）都锁全实例；RENAME、RENAMENX、COPY、MOVE 无条件锁源库与目标库的全部分片，条件里没有检查是否配置了 Key 配额；`max_keys` 非零时非只读命令锁全库。config.md 与 architecture.md 写的是带配额写入锁全库、启用淘汰且有限额的写入锁全实例，没有列出 RENAME、COPY、MOVE 的无条件全库锁定；config.md 中「写入」是否包含不扩大内存的命令，文字上不明确。代码注释写的是「涉及容量协调时预先锁全库」，条件却不含容量协调 | 代码：`crates/engine/src/lib.rs:416-455`；文档：config.md 内存与淘汰部分、architecture.md 并发一节 | L3 | v0.1.5 工作包 7.2 记录实际取锁范围与同机对照；v0.2.0 工作包 1.1 评估缩小范围 |
| P-10 | Reliable 下每条非只读命令都会先对所有已锁分片做到期记账，记账用对分片索引全量 `iter().find` 查找到期项：每次写入至少扫描一遍被锁分片的全部条目，即使没有任何到期项；有 E 个到期项时每个再扫一遍，最坏 O(N×E)。Fast 模式在函数入口直接返回，不受影响。每个终结事实各自一次 Journal 事务（一次 fsync），且调用时持有预算锁，所以 INFO、指标与维护取候选会排在 fsync 之后（后者为推断）。写入延迟随键数增长的幅度没有测，量级为推断 | 代码：`crates/engine/src/events.rs:100-141`（扫描在 113-123，预算锁与提交在 132-137）、`crates/engine/src/lib.rs:323-325` | L2 | v0.1.5 工作包 7.3 测量；v0.2.0 工作包 1.1 评估到期记账与锁时长，不省略终结事实 |
| P-11 | Fast 模式（默认）下，订阅读取每次都从队列头开始，先对每个已排队事件执行 `clone()`，再在闭包里按数据库与游标过滤，直到遇到第一个未投递的匹配事件才停止；游标之前的事件同样被克隆，没有新事件时整个队列被克隆一遍。服务端对每个订阅连接每 10 ms 调用一次（P-02），全程持有事件存储的全局锁，而提交路径取的是同一把锁。Fast 队列上限为 buffer_bytes 的一半（默认 32 MiB），按每事件几百字节估算在十万量级，所以一个已追平的空闲订阅者也会周期性克隆整个队列并拖慢写入提交。耗时我没有测；`fast` 队列的写入端以及投递后是否弹出，我没有读，该量级为推断 | 代码：`crates/events/src/store.rs:869-875`、`crates/server/src/connection.rs:44-45,63-78`；文档：config.md 事件容量一节 | L2 | v0.1.5 工作包 5.1 测量已追平的 Fast 订阅；v0.2.0 工作包 1.3 评审游标与唤醒 |
| P-12 | Reliable 下，单个事务含大量事件（FLUSHDB、FLUSHALL、大 MSET、SWAPDB）时，消费是平方复杂度。`next` 每次只取一个匹配事件，回调返回 false 时 `visit_from` 返回的是当前记录的起点（在 `pos += n + 16` 之前就 `return Ok(pos)`），读游标不会越过这条记录；下一次 `next` 重新读取、校验 CRC、解码这条记录的全部 N 个事件，再靠逻辑游标跳过已投递的。N 个事件的事务要解码约 N²/2 个事件，全程持事件锁，提交也取该锁。N 的上限由事件事务预检决定（buffer_bytes / 4，默认 16 MiB，按每事件几百字节约五万量级，量级为推断）。我没有构造该负载 | 代码：`crates/events/src/journal.rs:195-198`、`crates/events/src/store.rs:855-868`（注释只写了已验证前缀不重复扫描，没有处理记录内的重复解码） | L2 | v0.1.5 工作包 7.3 测量；v0.2.0 工作包 1.3 评审记录内游标、压缩失效与内存上限 |
| P-13 | 租约重投与水位以内的重复 ACK 可能从日志头查找，持事件锁。水位同时越过被过滤的记录，不是已交付证明；已清理的旧记录也可能无法再判别。不能用序号低于水位直接认定 ACK 有效。 | 代码：`crates/events/src/store.rs:546-548,617-667,879-894`；[ACK 与保留](../events.md) | L2 | v0.1.5 工作包 7.3 测量；v0.2.0 工作包 1.3 分开验收有效重复 ACK、从未交付、历史已过期 |
| P-14 | 启用事件的 SWAPDB 在持两库分片锁和预算锁时，对并集中的每个 Key 再遍历目标库；键数增加时比较次数可平方增长。关闭事件不走此环节。 | 代码：`crates/engine/src/administration.rs:153-202`、`crates/engine/src/transaction.rs:21-29`；复杂度来自循环，尚无耗时测量 | L2 | v0.1.5 工作包 7.4：几何递增键数，分离事件生成成本；据测量决定是否按 Key 定位 |

## E：工程与发行

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| E-01 | 没有项目级 LICENSE、SECURITY.md、CONTRIBUTING.md；不能把仓库公开或可下载视为已授予开源再分发权。仓库内未列明安全报告渠道。 | 代码：仓库根目录；见 [DEC-01 确认单](license-application.md) | L2 | v0.1.4 工作包 6；无需申请软件著作权 |
| E-02 | 没有 CHANGELOG。发行说明是固定文件 `packaging/release-notes.md`，随版本手工改写，历史变更只能从 68 个提交与四个标签推断 | 代码：`release.yml:213`；`git tag`、`git log` | L3 | v0.1.4 |
| E-03 | Windows 在 CI 中只运行测试子集：类型、协议、网络、CLI 的部分契约和单个事件测试；不运行引擎契约、服务端网络契约、Rust SDK 测试与 `tests/sdk_contracts.py`。Windows 是对外发行的平台之一 | 代码：`release.yml:148-161` | L2 | v0.1.4 评估可移植部分 |
| E-04 | 测试对象与发行对象不一致：Unix 任务在宿主默认目标上运行 `cargo test`，发行的 Linux 二进制是 musl 静态目标，烟测只覆盖 AppImage。基准同样在宿主上测得（testing.md 只记录 Linux x86_64，没有目标三元组），而 musl 的默认内存分配器在多线程负载下与 glibc 不同，已发布的性能数字未必描述发行二进制 | 代码：`release.yml:79,84,145-147`；分配器影响属推断，需实验确认 | L2 | v0.1.4 测试；v0.1.5 测量 |
| E-05 | CI 的动作使用可变标签（如 `actions/checkout@v4`、`Swatinem/rust-cache@v2`），Rust 工具链用浮动的 `stable`。上游标签被替换或工具链升级，都会在不改动仓库的情况下改变同一标签的构建结果 | 代码：`release.yml:20,36,39,110-114` | L2 | v0.1.6 |
| E-06 | 已声明 `rust-version = 1.85`，CI 没有 MSRV 任务；仓库没有 `rust-toolchain.toml`。本机工具链为 1.94.0，testing.md 记录的测量用 1.98.1，三者互不对应 | 代码：`Cargo.toml`；本机 `rustc --version`；文档：testing.md | L3 | v0.1.6 |
| E-07 | 发布任务对每个 `v*` 标签都创建预发布 Release，标题固定带 Preview，资产数量断言为 14。按现有流程无法发布 1.0.0 稳定版；在出现稳定 Release 之前，`ling update --stable` 没有可选版本 | 代码：`release.yml:210,213`；文档：install.md | L3 | v1.0.0-rc |
| E-08 | 在 Windows 开发机上，README 与 testing.md 列出的 `cargo test --workspace` 有 19 项失败，分布在 5 个测试二进制：`ling-events` 库 14 项、`ling-engine` 的 events 测试 2 项、`ling-client` 的 subscription 测试 1 项，`ling-server` 的 crash_recovery 与 resources 各 1 项。其中 18 项的 panic 信息相同：`ERR_CONFIG_INVALID`，`Reliable Journal is not supported on Windows; use Fast mode`。另 1 项（crash_recovery）断言服务进程已退出，其测试配置写入 `mode="reliable"`，按配置推断与前者同源；测试把服务端的 stdout 与 stderr 丢给 `Stdio::null()`，退出原因没有被直接观测。检索 `cfg(…windows 或 unix)` 时，测试代码只在 `crates/events/src/tests.rs` 有两处命中，其余失败的测试没有平台门控；该检索不含 `cfg!` 宏与 `cfg_attr`。同一台机器上 `cargo fmt --all -- --check` 与 `cargo clippy --locked --workspace --all-targets -- -D warnings` 通过。该次完整运行共执行 183 项测试（46 个测试二进制与文档测试汇总），164 项通过、19 项失败；testing.md 阶段 7 记录的是 135 项，对应更早的源码，两者不可直接对比。这是已知的 Windows 限制（testing.md 第 9 行已写明不把整套 Journal 测试列为通过）在开发体验上的后果，不是回归 | 实测：2026-11-17，本机 Windows 11、rustc 1.94.0，`cargo test --locked --workspace --no-fail-fast`；代码：`crates/server/tests/crash_recovery.rs:30-37,76-78`；文档：README.md:151、testing.md:9 | L3 | v0.1.4 工作包 7.2：依赖 Reliable 的测试按平台门控并写明原因，crash_recovery 的失败信息带上服务端 stderr |
| E-09 | 内置更新的 `--stable` 在当前发行流程下不可能成功：`select_release` 在选择时排除 `prerelease` 为真的 Release，而发布任务对每个标签都以 `--prerelease` 创建 Release（E-07）。install.md 与 README 里的 `ling update --stable` 示例因此现在找不到可选版本，直到发布流程能产出正式 Release。两侧代码我都读到了；`--stable` 此时返回哪个错误码、输出什么，没有运行确认 | 代码：`crates/server/src/update.rs:230-250`、`.github/workflows/release.yml:213`；文档：install.md、README.md | L2（E-07 的后果，E-07 原为 L3） | v0.1.4 工作包 5.7 说明无稳定候选；v1.0.0-rc 工作包 3.1、3.3 验收通道 |
| E-10 | Linux 包已给出可写的绝对 Journal 路径；从 README 复制相对路径到 /etc 下的服务配置则指向 /etc/ling/journal，不在 systemd 单元放行的 /var/lib/ling 内。不是包内默认配置损坏。 | 代码：`packaging/linux/ling.toml`、`packaging/linux/ling.service`、`packaging/linux/postinstall.sh`；文档：README、[安装指南](../install.md)。未在本轮启动 systemd | L2 | v0.1.4 工作包 5.7 区分前台和服务；v0.1.6 工作包 4.3 在原生环境验证 |
| E-11 | Windows 更新助手对被取消的更新报告状态未知：`$rollback` 在启动原生安装器之前就被置为 `$null`（第 70 行），Start-Process 在其后（第 78 行）。非管理员会话使用 RunAs，用户在 UAC 提示上拒绝或被策略拦截时，Start-Process 会抛出异常并进入 catch；此时 `$records` 为空、`$committed` 为 false、`$null -eq $rollback` 成立，于是写出 `unknown` 与 `LOCAL_UPDATE_INSTALL`（第 134 行）。install.md 对 `unknown` 的指引是检查安装器与已安装版本、不要直接重复更新，所以一次没有任何改动的取消，会让用户被告知结果未知。UAC 取消时 Start-Process 抛异常，是我对 PowerShell 行为的判断，没有在 Windows 上触发验证（推断）。另有两点：安装器超时分支（第 79 至 87 行）写出 `unknown` 与 `LOCAL_UPDATE_PENDING` 后，用没有上限的循环等待安装器退出，安装器若挂起，助手进程与更新互斥锁会一直保留；`parent_pid` 为正时先写 `scheduled`（第 49 行），助手被执行策略拦截或崩溃时，结果文件停在该状态，没有 pid 或时间戳字段，无法与进行中区分（推断，未触发） | 代码：`crates/server/src/update/windows.ps1:49,70,78-87,127-136`；文档：install.md | L3 | v0.1.6 工作包 4.3：在原生 Windows 区分取消、安装器已启动与未知状态 |
| E-12 | 三平台打包从明确的文件清单复制，新增根 LICENSE 不会自动覆盖所有独立制品；便携更新只替换两个程序，也不会刷新随版本变化的声明。Windows Setup 已携 WinSW 及其 MIT 文本，不能把这项重复算作缺失。 | 代码：`packaging/{linux,macos}/build.sh`、`packaging/linux/nfpm.yaml`、`packaging/windows/build.ps1`、`packaging/windows/installer.iss:38-46`、`crates/server/src/update.rs:177-180,492-568` | L2 | v0.1.4 工作包 6.1 查新包；v0.1.6 工作包 4.4 查升级后的实际内容 |
| E-13 | Unix 便携包分步备份、替换两个程序；函数返回错误时才执行回滚。若进程中途终止，存在其中一个程序缺失或两个版本不一致的窗口。未在真实主机中断升级。 | 代码：`crates/server/src/update.rs:214-219,624-660`；进程中止后果为推断 | L2 | v0.1.6 工作包 4.4：隔离目录中断演练并核对两程序，不把普通进程中止当物理断电 |

## D：文档

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| D-01 | decisions.md 的状态行仍写「v0.3 设计草案」「不表示实现或基准已经完成」，而阶段 1 至 7 已交付；状态行已过期 | 文档：decisions.md 第 3 行 | L4 | v0.1.4 |
| D-02 | roadmap.md 阶段 7 末段写「当前版本为 0.1.2 预发布版本」，testing.md 阶段 7 末段写「0.1.0 保持预览版本」；Cargo.toml 与 README 已是 0.1.3 | 文档：roadmap.md、testing.md、`Cargo.toml:6` | L4 | v0.1.4 |
| D-03 | config.md 的默认配置块同时给出 `pipeline_enabled = true`、`max_inflight_per_connection = 1024`、`group_commit_ms = 2`、`precision_ms = 1`，只有后文段落才说明其中多项没有运行时效果（R-05），读默认配置块的人会误以为已生效 | 文档：config.md | L4 | v0.1.4 |
| D-04 | protocol.md 在「请求排序与连接状态」写「跨 Shard 请求可以并行，RESPONSE 可乱序」，architecture.md 与 sdk.md 明确当前每连接串行、没有多路复用；协议规范描述的是能力上限，没有标注与现状的差距 | 文档：protocol.md、architecture.md、sdk.md | L4 | v0.1.4 |
| D-05 | 版本号手写在多处：README 的更新示例 `--version 0.1.3` 与「0.1.2 尚无内置更新」，install.md 的第 3、9、14 行。每次发布都要人工同步，而官网构建会自动读取 Cargo.toml | 文档：README.md、install.md；代码：`site/build.mjs:14` | L4 | v0.1.4 |
| D-06 | 已读文档里没有指标与 INFO 字段的完整参考：Prometheus 只在 config.md 有一段概述，没有指标名清单，`linger_` 前缀只在 install.md 的迁移说明中出现。cli.md、ttl.md 未读，处理前先核实 | 文档：config.md、install.md | L3 | v0.1.4 清单；v0.1.5 随新增指标维护 |
| D-07 | 没有按任务组织的运维手册：容量规划（受管字节与 RSS 的关系，见 P-08）、Journal 目录备份与迁移、`ERR_COMMIT_UNKNOWN` 之后如何核实、磁盘满与 `ERR_JOURNAL_FULL` 的处置、升级前检查，分散在 persistence、install、config 中 | 文档：已读各专项规范 | L3 | v0.1.4 初版；v0.3.0 补完 |

## F：功能缺失

以下条目是文档已承认或可由文档推出的能力缺口，不全是缺陷；列入是为了让优先级与取舍有据可查。architecture.md 已排除的范围（集群、复制、缓存持久化、Redis 兼容等）不在此列。

| 编号 | 缺口 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| F-01 | 单连接串行执行，没有连接内流水线或多路复用。协议允许响应乱序，服务端与三种 SDK 都不使用；吞吐只能靠多连接与普通 Batch | 文档：architecture.md、sdk.md、protocol.md | L2 | v0.2.0 设计并评估 |
| F-02 | 命名组只能创建，没有查看、删除或修改过滤条件的接口。组数量受 `events.max_groups` 限制；Reliable 下组状态持久化，上限用尽后没有回收途径（推断：只能换用新的 Journal 目录与新 stream_id，旧消费历史随之失效） | 文档：events.md、config.md | L3 | v0.2.0 评估 |
| F-03 | Windows 不支持 Reliable，配置会以 ERR_CONFIG_INVALID 明确拒绝。这是诚实的限制，但意味着 Windows 上没有任何持久化事件能力 | 文档：architecture.md、persistence.md、install.md | L2 | v0.2.0 决定实现或写入正式支持矩阵 |
| F-04 | TLS 证书、认证密码和全部限额只能重启后更换，而重启会清空缓存：证书续期或改密码等于一次全量缓存丢失 | 文档：config.md 的运行时变更一节、architecture.md | L2 | v0.2.0 评估证书热加载 |
| F-05 | 没有逐命令追踪，也没有慢命令日志；日志级别变化不增加命令级可见性 | 文档：config.md 日志一节 | L3 | v0.1.5 评估 |
| F-06 | 认证是实例级权限，没有按数据库或命令的 ACL；已认证连接可执行 SHUTDOWN、CONFIG SET、FLUSHALL | 文档：commands.md | L3 | 后续独立评估，不排期 |

## C：客户端

C-01 至 C-03 是可选发行工作的缺口，不是服务器缺陷；C-10、C-12、C-13 另列 CLI 与参考实现的具体行为。第三方客户端以协议与共享向量为准。

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| C-01 | Go 模块路径 `linger.local/cache` 不是可解析的地址，外部项目无法 go get，只能用 replace 指向本地副本 | 文档：sdk.md、architecture.md | L2 | v0.1.7 可选工作包 1，仅在维护者决定发布相应 SDK 时处理 |
| C-02 | Java 没有 Maven 或 Gradle 工程，构建靠 javac，契约入口是 main 方法；Java 项目无法声明依赖 | 文档：testing.md、sdk.md；代码：sdks/java 下只有 src 目录 | L2 | v0.1.7 可选工作包 2，仅在维护者决定发布相应 SDK 时处理 |
| C-03 | Rust 客户端及其依赖 crate 只以路径引用，外部项目无法从注册表引入 | 代码：根 Cargo.toml 的 workspace.dependencies | L2 | v0.1.7 可选工作包 3，仅在维护者决定发布相应 SDK 时处理 |
| C-04 | 三种 SDK 不自动重连、不重试，应用需要自建恢复逻辑；默认不重放写入是正确的，但连接建立与只读请求没有任何可选策略 | 文档：sdk.md | L2 | v0.1.7 |
| C-05 | 目录列表中没有示例工程或示例代码，新用户只能从测试和文档拼凑用法 | 代码：sdks 目录列表 | L3 | v0.1.7 |
| C-06 | Java 与 Go 的 Payload、Value 64 MiB，解码预算 128 MiB，业务深度 8 等限制固定，既不跟随服务端配置，也不能由调用方调整 | 文档：sdk.md | L3 | v0.1.7 |
| C-07 | 套接字选项不统一：Rust SDK 设置了 TCP_NODELAY，Java 的设置本次未检查，三种语言也没有统一的 keepalive 约定 | 代码：`sdks/rust/src/lib.rs:176`；Java、Go 属推断 | L3 | v0.1.7 |
| C-08 | 独立运行 Go 测试时，没有 `LINGER_TEST_ADDR` 会跳过真实服务用例且仍显示通过，只有驱动脚本能取得完整结果 | 文档：testing.md | L3 | v0.1.7 |
| C-09 | CLI 的依赖里没有行编辑库，交互模式的历史、补全与编辑取决于终端；是否有机器可读输出，cli.md 本次未读，实现前核实 | 代码：`crates/cli/Cargo.toml`；其余属推断 | L3 | v0.1.7 评估 |
| C-10 | CLI 显式 ACK 模式下显示事件后从 stdin 读确认；EOF 和 Ctrl-C 汇入同一分支，关闭连接并可按成功退出，未确认的事件仍可能重投。 | 代码：`crates/cli/src/main.rs:254-290`；未运行 CLI 的 EOF 端到端用例 | L3 | v0.1.7 工作包 7.4：区分无输入与主动取消，检查退出状态及 ACK |
| C-12 | SDK 连接池不检查空闲连接是否仍然存活：Go 的 `Acquire` 直接取 `idle` 末尾的连接，`Lease.Close` 只要连接没有被本地关闭就放回，没有空闲期限，也没有取用前探测。服务端默认 `idle_timeout_ms = 300000`，空闲超时后由服务端关闭连接，下一次从池里取到该连接的请求在发送后读到 EOF，被报为 DISCONNECTED 且结果未知，写入尤其无从判断，而 SDK 又不允许重放。Rust 与 Java 的池结构相同（审查代理报告：`sdks/rust/src/pool.rs:101-115`、`Pool.java:46-49`），我只读了 Go 的池。读线程能否在发送前就发现连接已失效，我没有读到读线程的实现，不确定 | 代码：`sdks/go/pool.go:50-120`、`crates/server/src/connection.rs:614-628`；文档：config.md（idle_timeout_ms 默认值）、sdk.md | L2 | v0.1.7 工作包 4.3：空闲租约与发送边界；可选池不重放已发写入 |
| C-13 | 等待响应时不同入口的 ID 匹配不一致。Rust 普通请求丢弃迟到的普通响应，但 Batch 或订阅控制请求收到旧响应会报协议错误；Java / Go 等待响应时先匹配当前 ID，把允许的 ID=0 连接 ERROR 改报本地协议错误，可能遮住服务端的真实原因。 | 代码：`sdks/rust/src/lib.rs:320-325,379-395`、`sdks/rust/src/subscriber.rs:217-236`、`sdks/go/client.go:409-420`、`sdks/java/src/main/java/io/linger/cache/Client.java:402-409`；服务端 `crates/server/src/connection.rs:71-75`；[ID 契约](../protocol.md)。未单独实测这几条交错路径 | L2 | v0.1.7 工作包 5.5：按帧种类区分迟到 RESPONSE、致命 ERROR 和非法 ID；语言无关用例 |

## M：测量与验收

| 编号 | 发现 | 证据 | 级别 | 处理 |
|---|---|---|---|---|
| M-01 | 全部性能数据取自同一台 VMware 客户机：客户端与服务端共享 CPU、经回环通信、不绑核，没有独立压测机。testing.md 自述短测量按场景顺序连续执行、没有随机交错，后一半矩阵与低速持续运行同机重叠 | 文档：testing.md 阶段 7 | L2 | v0.1.5；DEC-09 决定前仍属本机验收级别 |
| M-02 | 没有属性测试或参考模型对照。正确性证据是手写契约、固定种子的 20,000 次协议突变烟测和并发观察者测试，没有随机命令序列与最小语义模型的逐步比对 | 文档：testing.md；代码：根 Cargo.toml 与各 crate 依赖段没有属性测试库 | L3 | v0.1.5 |
| M-03 | 没有多日压力、物理断电与真实设备故障验证。持续运行各 10 分钟；Journal 故障靠强制终止进程、只读目录、Linux /dev/full 与模拟 fsync 错误，不能代表断电后的磁盘状态 | 文档：testing.md、roadmap.md | L2 | v0.3.0 |
| M-04 | 没有 Redis 8 可比测量，本机只有 Redis 7.0.15。吞吐目标仍是研究目标，不是断言 | 文档：testing.md | L3 | v0.3.0 |
| M-05 | CI 没有覆盖率统计，也没有对持锁路径做并发调度探索（如 loom）或线程检查；Go 有 race 检查，Rust 没有对应项。工作区禁用 unsafe，内存安全类工具收益较小，价值需评估 | 代码：release.yml 无相关步骤；价值属推断 | L3 | v0.3.0 评估 |
| M-06 | 原生安装验收没有执行。testing.md 已写明 CI 脚本存在不等于已通过原生验收，这一缺口仍在 | 文档：testing.md、install.md | L2 | v0.1.6 |
| M-07 | 阶段 7 数据对应更早的源码（矩阵基于 198d555，复测在 ad6665b 之后）。此后的提交包含内置更新与分层帮助（git log 标题），这些改动对热路径的影响没有数据，testing.md 也没有 0.1.3 的测量记录 | 文档：testing.md；git log | L3 | v0.1.5 |
| M-08 | 没有持续模糊测试。只有固定种子突变烟测；Journal 恢复、配置解析、Value 解码没有覆盖引导的 fuzz 目标 | 文档：testing.md 自述未执行持续模糊测试 | L3 | v0.1.5 |
| M-09 | 消费或 ACK 失败可以结束消费任务；基准程序仍按生产/预热错误决定退出，外层驱动据退出码收结果。旧报告保留了消费错误，缺的是每个场景可执行的通过条件，不是历史数据失真。 | 代码：`crates/server/examples/baseline.rs:365-385,425-426,449-466`、`benchmarks/stage7.py:152-158`。复核 [阶段 7 原始 JSON](../../benchmarks/results/stage7-2026-11-15-results.json)：87 条运行记录，27 条有消费者，12 条 Fast 记录各报告一次 ERR_OFFSET_EXPIRED；未重跑基准 | L2 | v0.1.5 工作包 1.5：区分预期落后、异常退出和正常消费；v0.3.0 工作包 2.5 沿用 |

## 不列为缺陷的设计选择

以下内容已写入规范，登记册不把它们当作缺陷重复立项。只有「规范与实现不一致」或「规范没有说明」时才立项。

- 默认 noeviction，达到内存预算时拒绝扩大内存的写入。
- 缓存值与 TTL 重启后为空；Reliable 只恢复已提交事件与消费元数据。
- 单连接串行执行；并发靠多连接与普通 Batch。这是 F-01 的前提，F-01 登记的是能力缺口。
- SDK 不自动重放写入；超时不代表写入未执行。
- Fast 溢出时丢弃事件并计数；Reliable 不承诺无限保留，落后于保留位置的消费者得到明确错误。
- 逻辑过期与物理回收是两个时点，回收延迟没有上限承诺。
- 受管内存是逻辑预算，不等于 RSS。P-07、P-08 登记的是校准与文档缺口，不是语义错误。

## 汇总

81 个不同编号：上轮表中已有 74 项，本轮新增 7 项（R-13、R-14、P-14、E-12、E-13、C-13、M-09）。包括优化候选、工程缺口与尚待实测的风险，并非 81 个已复现的缺陷；同一编号跨版不重复计数。

| 类别 | L1 | L2 | L3 | L4 | 合计 |
|---|---:|---:|---:|---:|---:|
| R 稳健性 | 7 | 5 | 2 | 0 | 14 |
| S 安全与供应链 | 0 | 5 | 1 | 0 | 6 |
| P 性能与资源 | 0 | 10 | 4 | 0 | 14 |
| E 工程与发行 | 0 | 8 | 5 | 0 | 13 |
| D 文档 | 0 | 0 | 2 | 5 | 7 |
| F 功能缺失 | 0 | 3 | 3 | 0 | 6 |
| C 客户端 | 0 | 6 | 6 | 0 | 12 |
| M 测量与验收 | 0 | 4 | 5 | 0 | 9 |
| 合计 | 7 | 41 | 28 | 5 | 81 |

| 版本 | 处理的条目（同一项可跨版先测再改） |
|---|---|
| v0.1.4 | R-01 至 R-14（R-03 可观测、R-04 首帧、R-05 标注）；S-01、S-03 至 S-06；P-01、P-08（文档）；E-01 至 E-04、E-08 至 E-10、E-12（首次打包）；D-01 至 D-07 |
| v0.1.5 | P-02 至 P-14（按工作包测量、校准或评估）；E-04、F-05、M-01、M-02、M-07 至 M-09 |
| v0.1.6 | S-02、S-06、E-05、E-06、E-10 至 E-13、M-06 |
| v0.1.7 | C-01 至 C-10、C-12、C-13；必交第三方可用的规范、向量与 CLI 契约；不要求上架官方 SDK |
| v0.2.0 | F-01 至 F-04、P-02、P-05、P-06、P-09 至 P-13、R-03 至 R-05；设计决定与实现分别验收 |
| v0.3.0 | M-03 至 M-05、M-09（消费验收口径）、D-07（运维补完） |
| v1.0.0-rc | E-07、E-09、DEC-08 |
| 不排期 | F-06 |

## 核实后不立项

- N-02（代理报告：Batch 子项提交后编码失败，整批只回一个顶层错误并丢掉已执行项的结果）：读了 `batch.rs:114-139`、`support.rs:120-133`、`codec.rs:410-424,598-654`。Ok 结果的预算口径一致：引擎用 `cap - 1` 校验 Value 再加 1 个状态字节，批处理的实际占用是编码长度减 18，同样是状态字节加 Value；错误项预留按 11 加最长错误码计算，与 protocol.md 的 `u8(1) + String + String` 编码一致。`batch.rs:130` 的 `?` 只在单项响应超过 `max_response_bytes` 时失败，而 `cap` 不大于 `max_response_bytes - 4`，在引擎遵守 `cap` 的前提下不可达；`131-136` 的防御分支返回 ERR_COMMIT_UNKNOWN，符合规范。结论：未发现缺陷，不立项，只在测试缺口里补「结果长度恰为 cap 与 cap + 1 的 LPOP、SPOP、GETSET」。这个结论以「引擎每个命令都遵守 cap」为前提，我只读了 `response_size`，没有逐命令核对，也没有读 `Encoder::response` 的实现。
