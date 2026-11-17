# 独立客户端逐帧示例

本文使用[协议 v1](protocol.md)的类型表示法说明一条连接上的输入和预期。`String("GET")` 表示编码后的 String Value，不是把这段文本原样发到 socket。完整字节向量位于 [frames.tsv](../tests/fixtures/protocol/frames.tsv)，Value 标签和边界位于 [values.tsv](../tests/fixtures/protocol/values.tsv) 与 [invalid-values.tsv](../tests/fixtures/protocol/invalid-values.tsv)。第三方可选自己的语言和 API，不依赖仓内 SDK。

## 握手与数据库

启用 TLS 的端口先完成 TLS 握手，验证服务端名称、信任链以及按需提供客户端身份。接下来读取和发送均按完整帧处理；一次 socket read 不保证是一帧。

| 顺序 | 方向与帧 | Payload 与预期 |
|---|---|---|
| 1 | 客户端 REQUEST，ID=1，Flags=0 | `String("HELLO") + List([UInt(1)])`；完整固定输入是 `frames.tsv` 的 `hello` |
| 2 | 服务端 RESPONSE，ID=1，Flags=0 | `status=0 + Map`；核对 `protocol=UInt(1)`、`server_version=String`、`events=Bool`、`reliable_events=Bool`、`stream_id=Bytes(16) 或 Null`。事件关闭时流 ID 为 Null，Reliable 必须同时启用事件 |
| 3 | 客户端 REQUEST，新 ID | 需要认证时发送 `String("AUTH") + List([String(password)])`，等待成功 `Bool(true)`；不把密码放进日志 |
| 4 | 客户端 REQUEST，下一个 ID | `String("SELECT") + List([UInt(0)])`，等待成功 `Bool(true)`，然后才允许业务请求 |

请求 ID 在每条连接上从 1 顺次递增，REQUEST 与 ACK 共用序列，不复用。AUTH 不需要时省略整步；库号 0 也可以显式 SELECT。`frames.tsv` 的 `ping` / `pong` 是 HELLO 后直接 PING 的独立最短序列，ID=2；若已执行 AUTH 或 SELECT，必须使用当前连接的下一个 ID，不能照抄 2。

能力 Map 由实例生成，不要求字段编码次序固定；不把服务端产品版本等同于协议版本。HELLO 仅执行一次，初始化失败即释放连接。

## 强类型读写：Null、缺失与完整 UInt

在空的隔离数据库按 [commands.tsv](../tests/fixtures/protocol/commands.tsv) 的顺序运行。每条普通请求的 Payload 是 `String(command) + arguments`；夹具第三列已经是参数 List 的完整字节。第四列是成功 Value 字节或稳定错误码，不包括 RESPONSE 的 status 字节。

| 夹具 ID | 语义输入 | RESPONSE 的预期 |
|---|---|---|
| `set_null` | `SET [String("shared"), Null]` | `status=0 + Bool(true)` |
| `get_null` | `GET [String("shared")]` | `status=0 + Null`；成功结果必须带 `00` 类型标记 |
| `missing` | `GET [String("absent")]` | `status=1 + String("ERR_KEY_NOT_FOUND") + String(message)`；不能返回成功 Null |
| `set_uint_max` / `get_uint_max` | 存取 `UInt(18446744073709551615)` | 保留 `03 ff ff ff ff ff ff ff ff`，不可经通用 JSON Number 或有符号 long 截断 |
| `uint_overflow` / `unchanged_after_failure` | 对最大 UInt 执行 INCR，再 GET | INCR 返回 `ERR_OVERFLOW`；GET 仍是原最大 UInt，失败不留下部分修改 |

`values.tsv` 的十种 Value 可逐一用于 SET / GET；String 与 Bytes 即使字节相同也不同类型，Int、UInt 与 Float 也不隐式转换。Map 与 Set 不要求编码顺序相同，比较解码后的类型和值。尺寸和深度上限见 [SDK 预算表](sdk.md#连接选项与本地预算)。

## 普通 Batch

`frames.tsv` 的 `batch_request` 是单项 `GET k`：Flags=1，ID=3，Payload 以 `u32(1)` 开始；`batch_response` 是同 ID、同 Flags 的单项成功 Null。整批只有一个 ID，各结果按输入顺序排列。准入失败可用 Flags=0 的普通错误 RESPONSE；不能把该错误误当作缺失的 Batch 子项。

实际使用可提交 SET、GET 已存 Null、GET 缺失三项，分别检查成功 Bool、成功 Null、错误码。前项成功不会因后项失败回滚。连接在完整 Batch 响应收齐前中断时，不能凭已到达的半帧确认其中任何项，更不能自动重发整批。

## 专用订阅与显式 ACK

新建独立连接，完成 HELLO / AUTH / SELECT；保留 HELLO 中的 stream_id。下表中的 N 表示该连接尚未使用的下一个 ID。

| 顺序 | 方向与帧 | Payload 与预期 |
|---|---|---|
| 1 | 客户端 REQUEST，ID=N | `String("SUBSCRIBE") + List([List([String("k")]), String("GROUP"), String("example-workers")])`。patterns 是内层 List，不能平铺 |
| 2 | 服务端 RESPONSE，ID=N | 成功 `Bool(true)`，先于本次订阅的 EVENT |
| 3 | 服务端 EVENT，ID=0 | `frames.tsv` 的 `event` 给出完整 Map 字节，含 event_id、类型、Key、generation、revision、时间；应用完成处理前不 ACK |
| 4 | 客户端 ACK，ID=N+1 | Payload 仅为该事件的完整 event_id Map。`frames.tsv` 的 `ack` 与 `event` 对应同一 ID；ACK 帧不是 `REQUEST "ACK"` |
| 5 | 服务端 RESPONSE，ID=N+1 | 成功 `Bool(true)`；Reliable 命名组此时确认状态已同步提交，Fast 不提供跨重启恢复 |

等待第 5 步时仍可能收到 EVENT，先放入有界队列，不能当作 ACK 结果或丢弃；HEARTBEAT 只表示传输活跃，不表示 ACK 成功。[sessions.tsv](../tests/fixtures/protocol/sessions.tsv) 的订阅控制用例给出事件与心跳交错的预期。帧向量里的数字身份仅用于固定输入，真实 ACK 必须取收到的事件，不能自行生成或只确认 sequence。

第 2 步之后尚未发送 ACK 时属于空闲订阅读取：再次收到 ID=N 的旧 RESPONSE 应丢弃，继续等待事件；不把它作为新调用结果。零或未来 ID 的 RESPONSE、未来 ID 的 ERROR 则关闭连接并报告协议错误，此时没有新业务请求在途，结果未知标记为 false。

关闭订阅不隐式 ACK。未确认事件可按组的租约、重投和保留规则再次投递；业务使用完整 event_id 去重。重连时先比较保存的 stream_id，变化报 `ERR_STREAM_MISMATCH` 并停止；遇 `ERR_OFFSET_EXPIRED` 也交给调用方处理，不能换组或跳到尾部。恢复边界见 [events.md](events.md)。

CLI 对应操作：

```sh
ling-cli -c 'SUBSCRIBE k GROUP example-workers' --count 1
# 读取打印的完整 ID，再从同一进程 stdin 输入 ACK <完整ID>。
# Read the complete printed ID, then send ACK <complete-ID> to the same process stdin.
```

显式模式在显示事件后遇 stdin EOF 会提示未确认并非零退出；Ctrl-C 是主动取消，仍不会隐式 ACK。只有 `--auto-ack` 在输出成功刷新后自动确认。详见 [CLI 订阅](cli.md)。

## 断开与排错

会话夹具给出普通请求、Batch、订阅控制及本地对端断线预期。实现者须区分：

| 可观察事实 | 对调用方的结果 |
|---|---|
| 参数校验失败、客户端已关闭后拒绝调用、应用请求发送前建连失败 | 已知未发送；不伪称服务端执行错误 |
| 开始写出请求后断线、响应头或 Payload 未完整到达 | 结果未知、关闭连接、不重放；服务端可能已经提交 |
| 完整成功 RESPONSE | 返回带类型 Value；Null 也是成功 |
| 完整业务失败 RESPONSE | 保留稳定服务端错误码；`ERR_COMMIT_UNKNOWN` 必须继续暴露未知状态 |
| 合法 ERROR（可用 ID=0） | 保留服务端码后关闭；不能因 ID=0 丢失原码 |
| RESPONSE 使用 ID=0 或尚未发送的 ID | 本地协议错误并关闭，不交给其他请求 |

`request_count` 检查测试对端实际收到的业务请求数，保证一次发送没有变成隐式重放。已有的非零旧 RESPONSE 丢弃后继续等待当前 ID。完整规则、三种控制等待及空闲订阅读取见 [协议连接状态表](protocol.md#请求排序与连接状态)。

仓内验证入口为 `tests/sdk_contracts.py`，分别报告三语言真实 TCP、双向 TLS 和 local-peer 会话用例；它是回归工具，第三方可用自己的工具消费同一夹具。SDK 的模块路径、包坐标和产品版本不构成线协议，未在公共仓库发布的参考源码不应写成可直接下载安装的包。
