# CLI 与官方 SDK

> 状态：阶段 6 已实现 Rust / Java / Go 原生客户端、CLI、固定库连接池、TLS、Batch 与专用订阅，仍为预览版本。单连接串行执行；稳定版压力与性能验收留在阶段 7。

`sdks/rust` 提供 Tokio 异步 `Client::connect / execute / batch / ping / select / close`，连接时依次完成 HELLO、可选 AUTH、SELECT。每个 Client 由调用方独占，使用 `&mut self` 串行请求。`Client::subscribe` 消耗普通连接，返回固定数据库的 `Subscriber`，提供 `next_event / ack / ping / close`；交错事件进入有界队列，不隐式丢弃或确认。空闲读取发送 HEARTBEAT，取消等待关闭连接，命名组由调用方显式重连。重连后先调用 `Client::require_event_stream(expected)` 校验保存的流身份；变化时关闭连接并报告 ERR_STREAM_MISMATCH，不创建替代组。`Options.tls` 使用 `ClientTls` 设置服务端名称、可选 CA 及成对客户端身份。`Pool::new / acquire / execute / close` 和 `PoolOptions` 提供有界容量与排队期限，`PooledClient` 持有固定库租约。

`crates/cli` 直接复用 Rust SDK，支持单命令或标准输入逐行执行已开放命令；连接会先自动初始化，交互中再次 HELLO 会得到连接状态错误。认证密码可由 `LINGER_PASSWORD` 提供，不支持密码命令行开关。String 使用双引号转义显示；任意 Value 位置使用显式类型字面量，索引、计数和增减步长按照命令签名解析成 Int / UInt。COPY 的 DB、MOVE / SWAPDB 的库号、SCAN 的 cursor / COUNT 均编码为 UInt。EXPIRE 系列的时间参数和 SET 到期选项的数值以十进制输入，编码为 Int（例如 `SET session null PX 5000 NX`）；不做 UInt 或 String 隐式转换。TTL 查询按 Int 展示，保留 -1 / -2 哨兵值；SCAN 续页须在同一连接保留筛选参数，不可每页启动新 CLI 进程。十种 Value 均可通过 CLI 存取，示例见 [README](../README.md)。未知命令交给服务端返回错误，不在 CLI 内模拟执行。`-c 'SUBSCRIBE ...'` 默认逐条从 stdin 读取 ACK；仅加 `--auto-ack` 时在输出刷新后自动确认。`--count N` 限制处理数量，事件首列显示无损 ID，随后为类型化 Map。`--batch` 从 stdin 收集最多 1000 条、64 MiB 文本后提交单个 Batch，不能与 `-c` 共用；保留逐项错误并使退出码非零。TLS 通过 `--tls-server-name`、`--tls-ca`、`--tls-cert`、`--tls-key` 配置。

`Options` 的连接和请求超时必须大于零。`Error::Server` 保留服务端错误码；`Error::Local` 区分连接、超时、协议、本地参数和关闭错误，`outcome_unknown()` 表达发送后的不确定性。超时、断线或取消网络等待会关闭该连接；默认不重放请求。成功认证和 SELECT 逐次等待响应，业务请求不越过连接屏障。已发送 ID 不复用，旧 ID 响应丢弃，0 或未发送 ID 的普通响应使连接失步并关闭。

## Java 与 Go 入口

Java 源码在 `sdks/java/src/main/java/io/linger/cache`，要求 JDK 17，不依赖 JNI 或第三方库。`Client.connect(Options)`、`execute`、`batch` 使用阻塞 API；`Client.Failure.code / outcomeUnknown` 保留稳定错误码和不确定性。`Pool.acquire()` 返回 `Pool.Lease`，支持 try-with-resources；`Pool.execute` 为一次调用租用连接。`Subscriber.next / ack / ping / close` 使用专用连接，线程中断会取消网络等待并关闭连接。共享守护调度器约束写入与响应期限，客户端 reader 队列容量为一。TLS 使用 JSSE 主机名校验；`Tls.context(caPem, identityPkcs12, password)` 载入可选自签根及客户端身份，未提供 CA 时使用系统信任。

Go 模块 `sdks/go` 的路径为 `linger.local/cache`，最低 Go 1.22，仅使用标准库。`Connect(ctx, Options)`、`Client.Execute / Batch / Subscribe` 及 `Subscriber.Next / Ack` 接受 context；`Client.Close` 可以并发取消等待。`NewPool / Acquire / Execute / Close` 配合 `Lease.Execute / Close` 固定数据库及凭据。`*Error.Code` 和 `OutcomeUnknown()` 区分稳定服务端错误与本地失败。`Options.TLS` 为标准 `*tls.Config`，拒绝 `InsecureSkipVerify`，使用其 RootCAs、ServerName 和 Certificates 完成验证。Go Value 明确保存 Kind 与 Data，不能把普通 Go 数值当作无标签 Value。

Java / Go 默认连接超时 5 秒、请求超时 30 秒；Java 显式设置必须为正，Go 的零值采用默认、负值拒绝。两者固定限制 Payload / Value 为 64 MiB、解码分配预算 128 MiB、业务容器深度 8、协议封套深度 12，服务端配置更大不会自动放宽客户端限制。订阅交错队列最多 1024 项且不超过 32 MiB；超限报错并关闭，不静默丢事件。

在仓库根目录编译 Java、在模块内检查 Go：

```sh
mkdir -p target/java-sdk
javac -Xlint:all -Werror -d target/java-sdk sdks/java/src/main/java/io/linger/cache/*.java
cd sdks/go
go vet ./...
go test ./...
```

没有 `LINGER_TEST_ADDR` 时独立语言测试会跳过真实服务用例；完整验收入口为仓库根目录的 `python3 tests/sdk_contracts.py`，自动运行 TCP 与双向 TLS 两种模式、Go race 和 Java 契约。测试证书与私钥只供本地验收，不用于部署。

## 范围与职责

首版交付 CLI、Rust、Java、Go 客户端；TypeScript 留待后续版本。所有客户端通过 [protocol.md](protocol.md) 连接独立服务，不提供 Embedded API。

CLI 优先交付，并直接复用 Rust SDK。Java 与 Go 使用各自语言的原生网络实现，不通过 JNI、cgo 或其他 FFI 包装 Rust。它们共享类型、命令、协议和验收用例，不要求共享可执行代码。

规范的职责分别是：[architecture.md](architecture.md) 定义范围，[commands.md](commands.md) 定义命令，[data-model.md](data-model.md) 定义值，[protocol.md](protocol.md) 定义传输。CLI 是可操作的验收参考；遇到实现差异时，修正实现或正式修订规范，不能以 CLI 的偶然行为覆盖契约。

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

后续 TypeScript SDK 应使用 `bigint` 表达 Int / UInt，不能经 JavaScript `number` 中转 64 位整数。所有 SDK 不得通过通用 JSON 编解码丢失类型或精度。

## 成功、空值与错误

Rust 的单值读取在语义上为 `Result<Value, Error>`。成功的 `Value::Null` 与 `Err(KeyNotFound)` 不同；Java、Go 提供等价且可区分的结果表示。

`MGET` 在语义上为按输入顺序排列的 `List<Result<Value, Error>>`。重复 Key 保留重复位置，单项不存在不阻止返回其余项。`MSET` 是原子多 Key 命令，其保证不能通过多个独立 `SET` 模拟。

服务端错误保留稳定错误码和必要上下文；诊断文本用于说明，不作为业务分支条件。客户端连接、编解码、取消和超时错误必须与服务端命令错误区分。

无法确认请求是否执行时，错误需暴露“结果未知”状态，而不能笼统映射为 Key 未修改。错误不得携带密码、TLS 私钥或完整敏感 Value。

## 连接与请求生命周期

单连接通过 Request ID 匹配请求与响应。当前三种 SDK 均限制一条普通请求在途；多线程 / 协程调用排队串行执行或通过独占借用约束，不提供单连接并行多路复用。协议可表达乱序响应，SDK 丢弃已结束旧请求的迟到响应，拒绝零 ID 或未发送 ID 的普通响应。

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

读请求的重试策略可以由 API 明确配置，但必须受整体超时约束。SDK 不承诺 Exactly-Once，也不生成未经协议支持的幂等保证。

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

CLI 覆盖首版公开命令与订阅能力，负责文本解析、类型展示和错误展示；它不实现独立于 Rust SDK 的网络或数据库语义。

Value 使用显式类型字面量，如 `int(1)`、`uint(1)`、`float(1.0)`、`bytes(0x0102)`、`"text"`、`null`、`true`、`list([...])`、`map({...})`、`set([...])`。具体参数及完整语法见命令规范。

输出区分 Null、不存在、空字符串、空 Bytes 及空容器。显示 64 位整数时保持精度；String 与 Bytes、Int 与 UInt 必须可辨识。非交互调用通过退出状态区分成功和失败，错误文本不能伪装为正常 Value。

命令行与日志不得回显认证密码。CLI 的方便用法不能绕过服务器资源限制，也不建立隐藏的默认值转换。

## 验收要求

各语言共享协议字节用例、10 种 Value 的往返用例、命令成功与错误用例。CLI 与 Rust 共用传输实现，Java / Go 通过同一服务端契约验证，不能只做各自的自编码自解码测试。

重点覆盖：Null 与不存在、最大 UInt、NaN 拒绝、Float 正负零、Bytes 集合成员、响应乱序、同 Key 顺序、选库屏障、迟到响应、写入结果未知和关闭后任务释放。

事件测试覆盖显式 ACK、重复投递、断线恢复、回调失败、过期 offset 和订阅取消。能力完成度按命令与模式分别记录；任何语言尚未支持的能力要明确标注，不允许以空实现宣称覆盖。
