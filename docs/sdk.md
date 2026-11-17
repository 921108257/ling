# 客户端接入、CLI 与仓内 SDK

ling 提供 Rust、Java、Go 原生客户端和 CLI，支持固定数据库连接池、TLS、普通 Batch 与专用订阅。单连接串行执行，并发请求通过多个连接提供。

## 第三方客户端从哪里开始

不需要安装官方 SDK、链接 Rust 引擎，或等项目发布语言包。可以用任意语言直接实现线协议；只实现命令客户端也可以，不必实现连接池、自动重连或订阅。请声明支持的协议版本和能力。仓内客户端是可运行的参考，不是规范：有差异时以[协议](protocol.md)、[命令](commands.md)与[数据模型](data-model.md)为准。独立实现自行维护和发布；若复制本项目代码、文档或夹具，需遵守这些材料适用的授权。

| 步骤 | 实现与自查 |
|---|---|
| 1. 字节和类型 | 从[合法 Value](../tests/fixtures/protocol/values.tsv)、[非法 Value](../tests/fixtures/protocol/invalid-values.tsv)和[完整帧](../tests/fixtures/protocol/frames.tsv)开始，逐字节比较预期，不只做自己的编码器/解码器往返。保留 String/Bytes、Int/UInt/Float 标签与完整 64 位整数。 |
| 2. 第一次真实连接 | TLS 如启用先验证身份；首帧 HELLO 带 `UInt(1)`、ID 从 1 开始。核对返回的协议版本及能力，再按配置依次完成 AUTH、SELECT，每步等响应。在空的隔离测试库按文件顺序执行[命令夹具](../tests/fixtures/protocol/commands.tsv)：GET 已存 Null 应成功，GET 缺失应是错误，溢出后旧值不变。 |
| 3. 会话与断线 | REQUEST/ACK 按连接共用 ID 序列。已结束的非零 ID 的迟到 RESPONSE 丢弃，零或未发送 ID 的 RESPONSE 表示失步；**ERROR 可以用 ID=0**，保留服务端错误码并关闭。发送后超时、取消或断线不能宣称写入未执行，默认不重放。见[排错手册](operations.md)。 |
| 4. 自选能力 | Batch 按输入顺序逐项返回，不是事务；支持订阅时用选好库的专用连接，在等待 ACK 响应时也要能收 EVENT。按完整 event_id 显式确认；流身份变化或 Offset 超期应交应用处理，不能跳过。见[事件规范](events.md)。 |

[测试说明](testing.md)区分离线字节、真实 TCP 和双向 TLS；报告用例通过、失败及跳过的数量，未执行不算通过。下文介绍的是已有实现，不要求第三方使用相同 API 或包名。

逐帧入门见[客户端实现示例](client-examples.md)：握手、强类型读写、订阅与显式 ACK、连接中断分别链接到共享输入和预期。响应匹配及传输失败另有[会话夹具](../tests/fixtures/protocol/sessions.tsv)，无需将仓内 SDK 当作预期结果生成器。

## 连接选项与本地预算

以下是仓内参考客户端的默认值，不是协议要求第三方采用的 API。地址、凭据、库号在建连时确定；TLS 不允许关闭身份校验。超时是一次等待的上限，不是服务端回滚期限。

| 选项 | Rust | Java | Go |
|---|---|---|---|
| 地址与数据库 | `Client::connect(address, password, database, options)` | `Options.host / port / database`，简便构造默认库 0 | `Options.Address / Database`，零值库 0 |
| 认证 | 可选 `password` | 可选 `Options.password` | `Options.Password` 为空时不发 AUTH |
| 连接超时 | `Options.connect_timeout`，5 秒，必须大于零 | `connectTimeoutMs`，5000 毫秒，必须大于零 | `ConnectTimeout`，零值采用 5 秒，负值拒绝 |
| 请求超时 | `Options.request_timeout`，30 秒，必须大于零 | `requestTimeoutMs`，30000 毫秒，必须大于零 | `RequestTimeout`，零值采用 30 秒，负值拒绝 |
| TLS | `Options.tls: Option<ClientTls>`；名称、CA、成对客户端证书与私钥 | `SSLContext` 与 `serverName`；客户端身份为 PKCS#12 | `*tls.Config` 的 RootCAs、ServerName、Certificates；拒绝 `InsecureSkipVerify` |
| 编解码限额 | `Options.limits: ling_protocol::Limits` 可设置本地上限 | 固定本地限额 | 固定本地限额 |
| TCP_NODELAY | 显式启用 | 显式启用 | 使用 Go TCP 的默认设置；不另暴露开关 |
| 恢复与重放 | 无自动重连或重放 | 无自动重连或重放 | 无自动重连或重放 |

没有本轮同机 A/B 测量证明套接字选项的性能收益；不同语言的设置不升级为协议要求。连接池容量和排队约定见[连接与请求生命周期](#连接与请求生命周期)。

| 本地资源边界 | Rust 默认 | Java / Go 固定值 |
|---|---|---|
| 完整帧（含 18 字节头） | 128 MiB，方向负载另限 64 MiB | 方向负载限 64 MiB，另加 18 字节头 |
| 请求或响应 Payload | 64 MiB | 64 MiB |
| 单个业务 Value 的编码大小 | 64 MiB | 64 MiB |
| 单帧解码分配预算 | 128 MiB | 128 MiB |
| 独立业务 Value 的容器深度 | 8，根标量为 0，根容器为 1 | 8 |
| 协议封套递归保护 | 业务深度上限 + 4，默认 12 | 12 |
| Batch 子请求数 | 1000 | 1000 |
| 订阅交错事件队列 | 1024 项，保守计费不超过 `max_decode_bytes / 2`，默认 64 MiB | 1024 项，事件 Payload 合计不超过 32 MiB |

长度、计数与预算必须在分配前校验；容器元素数还受剩余输入字节限制。业务深度不能直接应用到请求参数 List 或事件封套，解封后仍须单独校验业务 Value。超出结构保护导致无法安全解码时关闭连接，不能当作合法空值。服务端资源配置和客户端本地预算独立，放大服务端上限不会放宽客户端；合法的最大 Value 加上命令、Key 或响应封套后也可能超过单帧预算。

表中的业务深度约束用于独立 Value 编解码与服务端存储值校验；响应帧中的 List / Map 还可能是多项结果或事件封套，仓内读取路径先用封套递归保护，不能把外层结果一律当成业务 Value 根。Rust 的订阅队列按每项 `512 + 2 × Key UTF-8 字节数` 保守计费，Java / Go 按事件 Payload 长度计费；这些本地预算不是相同的 RSS 承诺。

## Rust 与 CLI 入口

`sdks/rust` 提供 Tokio 异步 `Client::connect / execute / batch / ping / select / close`，连接时依次完成 HELLO、可选 AUTH、SELECT。每个 Client 由调用方独占，使用 `&mut self` 串行请求。`Client::subscribe` 消耗普通连接，返回固定数据库的 `Subscriber`，提供 `next_event / ack / ping / close`；交错事件进入有界队列，不隐式丢弃或确认。空闲读取发送 HEARTBEAT，取消等待关闭连接，命名组由调用方显式重连。重连后先调用 `Client::require_event_stream(expected)` 校验保存的流身份；变化时关闭连接并报告 ERR_STREAM_MISMATCH，不创建替代组。`Options.tls` 使用 `ClientTls` 设置服务端名称、可选 CA 及成对客户端身份。`Pool::new / acquire / execute / close` 和 `PoolOptions` 提供有界容量与排队期限，`PooledClient` 持有固定库租约。

`crates/cli` 的 `ling-cli` 直接复用 Rust SDK，支持单命令或标准输入逐行执行已开放命令；连接会先自动初始化，交互中再次 HELLO 会得到连接状态错误。认证密码由 `LING_PASSWORD` 提供，未设置时兼容 `LINGER_PASSWORD`，不支持密码命令行开关。String 使用双引号转义显示；任意 Value 位置使用显式类型字面量，索引、计数和增减步长按照命令签名解析成 Int / UInt。COPY 的 DB、MOVE / SWAPDB 的库号、SCAN 的 cursor / COUNT 均编码为 UInt。EXPIRE 系列的时间参数和 SET 到期选项的数值以十进制输入，编码为 Int（例如 `SET session null PX 5000 NX`）；不做 UInt 或 String 隐式转换。TTL 查询按 Int 展示，保留 -1 / -2 哨兵值；SCAN 续页须在同一连接保留筛选参数，不可每页启动新 CLI 进程。十种 Value 均可通过 CLI 存取，操作与速查见 [cli.md](cli.md)。未知数据库命令交给服务端返回错误，不在 CLI 内模拟执行；本地 `HELP` 单独处理。`-c 'SUBSCRIBE ...'` 默认逐条从 stdin 读取 ACK；仅加 `--auto-ack` 时在输出刷新后自动确认。`--count N` 限制处理数量，事件首列显示无损 ID，随后为类型化 Map。`--batch` 从 stdin 收集最多 1000 条、64 MiB 文本后提交单个 Batch，不能与 `-c` 共用；保留逐项错误并使退出码非零。TLS 通过 `--tls-server-name`、`--tls-ca`、`--tls-cert`、`--tls-key` 配置。

Rust 包更名为 `ling-*`；Java 包 `io.linger.cache` 与 Go 模块 `linger.local/cache` 保留兼容名称。产品更名不重分配协议类型、错误码或磁盘标签。

`Options` 的连接和请求超时必须大于零。`Error::Server` 保留服务端错误码；`Error::Local` 区分连接、超时、协议、本地参数和关闭错误，`outcome_unknown()` 表达发送后的不确定性。超时、断线或取消网络等待会关闭该连接；默认不重放请求。成功认证和 SELECT 逐次等待响应，业务请求不越过连接屏障。已发送 ID 不复用，旧 ID 响应丢弃，0 或未发送 ID 的普通响应使连接失步并关闭。

## Java 与 Go 入口

Java 源码在 `sdks/java/src/main/java/io/linger/cache`，要求 JDK 17，不依赖 JNI 或第三方库。`Client.connect(Options)`、`execute`、`batch` 使用阻塞 API；`Client.Failure.code / outcomeUnknown` 保留稳定错误码和不确定性。`Pool.acquire()` 返回 `Pool.Lease`，支持 try-with-resources；`Pool.execute` 为一次调用租用连接。`Subscriber.next / ack / ping / close` 使用专用连接，线程中断会取消网络等待并关闭连接。共享守护调度器约束写入与响应期限，客户端 reader 队列容量为一。TLS 使用 JSSE 主机名校验；`Tls.context(caPem, identityPkcs12, password)` 载入可选自签根及客户端身份，未提供 CA 时使用系统信任。

Go 模块 `sdks/go` 的路径为 `linger.local/cache`，最低 Go 1.22，仅使用标准库。`Connect(ctx, Options)`、`Client.Execute / Batch / Subscribe` 及 `Subscriber.Next / Ack` 接受 context；`Client.Close` 可以并发取消等待。`NewPool / Acquire / Execute / Close` 配合 `Lease.Execute / Close` 固定数据库及凭据。`*Error.Code` 和 `OutcomeUnknown()` 区分稳定服务端错误与本地失败。`Options.TLS` 为标准 `*tls.Config`，拒绝 `InsecureSkipVerify`，使用其 RootCAs、ServerName 和 Certificates 完成验证。Go Value 明确保存 Kind 与 Data，不能把普通 Go 数值当作无标签 Value。

Java / Go 默认连接超时 5 秒、请求超时 30 秒；Java 显式设置必须为正，Go 的零值采用默认、负值拒绝。两者固定限制 Payload / Value 为 64 MiB、解码分配预算 128 MiB、业务容器深度 8、协议封套深度 12，服务端配置更大不会自动放宽客户端限制。订阅交错队列最多 1024 项且不超过 32 MiB；超限报错并关闭，不静默丢事件。

在仓库根目录编译 Java SDK：

```sh
mkdir -p target/java-sdk
javac -Xlint:all -Werror -d target/java-sdk sdks/java/src/main/java/io/linger/cache/*.java
```

## 客户端与服务端

所有客户端通过 [通信协议](protocol.md) 连接独立服务，不提供 Embedded API 或 TypeScript SDK。

CLI 直接使用 Rust SDK。Java 与 Go 使用各自语言的原生网络实现，不依赖 JNI、cgo 或其他 FFI 包装。

命令、类型和返回值见 [命令参考](commands.md) 与 [数据模型](data-model.md)。

各 SDK 负责连接生命周期、TLS 与认证、值编解码、请求匹配、超时、批量提交、事件订阅和 ACK。当前三种 SDK 均不自动重连或重放请求；池中的失效连接丢弃后，下次租用可以新建连接。服务端负责数据语义、原子修改、TTL、事件提交及资源限制，SDK 不以读改写模拟服务端原子命令。

## 类型映射

公开 API 提供带标签的 Value / Scalar 类型，精确保留数据库类型。以下是其内部内容的语言映射，不意味着可以用普通语言值替代所有类型标签。

| 数据库类型 | Rust | Java | Go |
|---|---|---|---|
| Null | `Value::Null` | 显式 Null 变体 | 显式 Null 变体 |
| Bool | `bool` | `boolean` / `Boolean` | `bool` |
| Int | `i64` | `long` / `Long` | `int64` |
| UInt | `u64` | 有范围校验的 `BigInteger` | `uint64` |
| Float | `f64` | `double` / `Double` | `float64` |
| String | `String` | `String` | `string` |
| Bytes | 字节缓冲区 | `byte[]` | `[]byte` |
| List | `Vec<Value>` | `List<Value>` | `[]Value` |
| Map | String 到 Value 的映射 | `Map<String, Value>` | `map[string]Value` |
| Set | Scalar 集合包装 | Scalar 集合包装 | 专用 Set 包装 |

Java 的 UInt 必须验证范围 `0..=2^64-1`。Go 的 Scalar 包装必须包含类型标记，并为 Bytes 与 Float 实现规范中的相等性；含 `[]byte` 的接口值不能直接安全用于 Go map 的键。

Rust 的 Float 也不能直接假设具备适用的 `Eq` / `Hash`，需在 Scalar 层实现 NaN 拒绝和正负零归一化。所有语言使用 [data-model.md](data-model.md) 的类型敏感相等规则，不能继承语言默认数值提升规则。

与其他系统交换数据时，应使用可精确表示 64 位整数的类型，避免通过通用 JSON 数值丢失 Int / UInt 的精度。Bytes、Set 等类型也需要保留类型标签。

## 成功、空值与错误

Rust 的单值读取在语义上为 `Result<Value, Error>`。成功的 `Value::Null` 与 `Err(KeyNotFound)` 不同；Java、Go 提供等价且可区分的结果表示。

`MGET` 在语义上为按输入顺序排列的 `List<Result<Value, Error>>`。重复 Key 保留重复位置，单项不存在不阻止返回其余项。`MSET` 是原子多 Key 命令，其保证不能通过多个独立 `SET` 模拟。

服务端错误保留稳定错误码和必要上下文；诊断文本用于说明，不作为业务分支条件。客户端连接、编解码、取消和超时错误必须与服务端命令错误区分。

无法确认请求是否执行时，错误需暴露“结果未知”状态，而不能笼统映射为 Key 未修改。错误不得携带密码、TLS 私钥或完整敏感 Value。

## 连接与请求生命周期

单连接通过 Request ID 匹配请求与响应。当前三种 SDK 均限制一条普通请求在途；多线程 / 协程调用排队串行执行或通过独占借用约束，不提供单连接并行多路复用。协议可表达乱序响应，SDK 丢弃已结束旧请求的迟到响应，拒绝零 ID 或未发送 ID 的普通响应。

订阅空闲读取沿用相同高水位检查：丢弃旧非零 RESPONSE，保留合法 ERROR 的服务端码并关闭，拒绝未来 ERROR 及零或未来 RESPONSE。空闲读取没有发送新的业务请求，本地协议错误的结果未知标记为 false；等待 ACK 或控制响应时，发送后失败仍按结果未知处理。

服务端单连接按接收顺序执行；不同连接可在不同 Shard 并发，客户端不承诺多个连接之间的先后关系。

`AUTH`、`SELECT`、`QUIT` 是连接状态屏障。SDK 必须等待其前序请求及屏障结果，再发送依赖新状态的请求，不能在切库期间混发业务命令。

连接池按认证身份与数据库固定分桶；客户端数据库句柄绑定数据库编号，普通调用不共享可变 `SELECT` 状态。连接重建后重新完成协议初始化、认证与选库，再交给业务请求。

各池不允许 HELLO / AUTH / SELECT / QUIT / SUBSCRIBE / UNSUBSCRIBE / ACK 改变租约上下文。连续 SCAN 必须在同一个独占租约内执行；逐次 `Pool.execute` 不保证取得同一连接。Go / Java 容量为 1..65536；Go 排队受 context 约束，Java 排队受请求超时及线程中断约束，Rust 使用 `PoolOptions.acquire_timeout`。归还 Go / Java 租约前须结束其操作。

关闭池会唤醒等待者、取消建连并关闭空闲连接。Go / Java 同时关闭在用连接；Rust 取消租约的在途等待，尚由调用方持有但闲置的租约在下一次调用或归还时释放 socket。关闭不代表撤销已发送的服务端变更，不删除订阅组。

请求 ID 在连接内唯一；超时后的迟到响应不得匹配到另一个新请求。连接关闭时结算该连接全部待响应项，释放定时器、读写任务和缓冲区。

## 超时、取消与重连

超时只表示调用方没有及时获得结果，不撤销已经送达服务端的命令。取消等待也不等于回滚写入。

当前连接建立尝试一次，调用方可在整体期限内自行重试；客户端关闭或调用取消时应及时停止。禁止无限重试、无界任务积压以及在后台静默吞掉持续连接失败。

连接重建不等于请求重放。写入发送后遇到超时或断线，结果可能未知；默认不自动重放该写入，尤其是增减、Push / Pop、删除及其他可能产生重复效果或重复事件的命令。

仓内实现不提供发送后只读重试白名单，也不重放读请求。第三方如主动增加策略，必须显式启用、有界退避及总期限，并按命令表逐条列出白名单与共享用例；不能按命令名称推断安全性。SCAN 的游标绑定连接，写命令、订阅、ACK、SELECT、AUTH、HELLO 均不能列入发送后自动重放。读取可能更新 LRU / LFU 热度，再执行会再次影响热度，但不改变 revision（见 [config.md](config.md)）。SDK 不承诺 Exactly-Once，也不生成未经协议支持的幂等保证。

调用前的参数校验失败、已关闭连接拒绝调用、尚未开始应用请求发送时的建连失败属于已知未发送。只要开始尝试写出请求，部分写入、等待响应时断线或响应被截断都保守报告结果未知；操作系统接受字节不证明服务端提交。失效租约应丢弃；下次租用新建连接重新完成 HELLO / AUTH / SELECT，不能把失败请求带到新连接执行。订阅恢复由调用方发起，须先比较 stream_id；ERR_STREAM_MISMATCH 或 ERR_OFFSET_EXPIRED 都停止恢复，不能自动换组或跳尾。

## Batch 与原子命令

普通 Batch 用于减少网络往返，每个子请求独立执行和返回结果，整体不保证原子性。SDK 保留输入顺序的结果映射，不能因为收到乱序响应而打乱调用方对应关系。

批量中的前项失败不自动回滚已执行项。版本 1 使用一个完整批量响应帧，三种 SDK 收到完整帧后按输入顺序返回逐项 Value / 错误；帧未完整收到时无法确认任何项，整个调用暴露结果未知且不重放。协议未提供部分帧的可确认结果。

`MGET`、`MSET` 等服务端命令与 Batch 是不同 API。其原子性、逐项错误及跨 Shard 规则以 [commands.md](commands.md) 为准，SDK 不能自行加强保证。

## 事件订阅

事件使用专用连接，避免订阅生命周期、消费背压与普通命令共享同一池内连接。专用连接仍需遵守初始化、认证及选库屏障。

`SUBSCRIBE pattern ... [GROUP group]` 指定组时使用稳定组身份，省略组时为临时订阅。`ACK event_id` 绑定该连接唯一的组上下文，不能确认其他组收到的事件。

事件 ID 是复合对象：`stream_id: u128`、`db: u16`、`shard: u32`、`sequence: u64`。SDK 保留各字段精度，不将整体或字段转换为浮点数；显示格式不替代协议编码。

业务成功处理事件后显式 ACK。默认不在回调开始或消息到达时自动 ACK；回调抛错或取消时，保留未确认状态，并按 [events.md](events.md) 的规则处理重投。

可靠模式允许重复投递，业务使用稳定事件 ID 实现幂等。SDK 必须暴露模式、事件类型和恢复错误，不能把 Fast 模式包装成可靠订阅。

断线恢复以服务端确认的消费进度为准。保留窗口之外的 offset、流身份变化等错误必须交给调用方处理，不静默跳到最新位置。关闭订阅时停止拉取、回调与重连，并释放专用连接。

首次订阅保存 HELLO 返回的 stream_id；重连先比较流身份再恢复组。同名组在新日志目录不代表原消费历史，流身份变化须显式确认重置，不能自动创建新组并从尾部开始。

## CLI 约定

CLI 覆盖当前版本公开命令与订阅能力，负责文本解析、类型展示和错误展示；它不实现独立于 Rust SDK 的网络或数据库语义。

`ling --help` 与 CLI 帮助均在连接服务器前处理。`HELP` / `ling-cli help` 显示简短主题目录，`HELP containers` 显示容器分类，`HELP containers list` 列出 List 命令及简述；`HELP SET` 显示完整命令说明。`HELP SET syntax|options|examples|related|description|types` 可查看单独部分，`ling-cli help SET examples`、`ling-cli --help SET examples` 与 `ling-cli -c 'HELP SET examples'` 等价。`ling-cli --cheatsheet` 显示完整命令参考。普通逐行输入的 HELP 也在本地处理；Batch 拒绝本地 HELP，且整批不提交。HELP 不是协议命令，不改变服务端公开命令清单。

帮助支持 `en-US`、`zh-CN`、`ko-KR`、`ja-JP`、`ru-RU`、`de-DE`、`fr-FR`；按 `--lang`、`LING_LANG`、`LC_ALL`、`LC_MESSAGES`、`LANG`、操作系统语言依次选择，未匹配回退 `en-US`。`C` / `POSIX` 使用英语，繁体中文不自动映射简体中文。帮助选项、说明与速查可本地化；协议命令名、类型字面量、数值与存储内容不受影响。详细操作见 [cli.md](cli.md)。

Value 使用显式类型字面量，如 `int(1)`、`uint(1)`、`float(1.0)`、`bytes(0x0102)`、`"text"`、`null`、`true`、`list([...])`、`map({...})`、`set([...])`。具体参数及完整语法见命令规范。

输出区分 Null、不存在、空字符串、空 Bytes 及空容器。显示 64 位整数时保持精度；String 与 Bytes、Int 与 UInt 必须可辨识。非交互调用通过退出状态区分成功和失败，错误文本不能伪装为正常 Value。

命令行与日志不得回显认证密码。CLI 的方便用法不能绕过服务器资源限制，也不建立隐藏的默认值转换。
