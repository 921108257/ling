# 二进制通信协议

> 状态：v0.3 实现前设计草案，尚未实现、尚未冻结。协议版本候选值为 1；本文中的字节向量用于评审，不代表已有互操作实现。

范围见 [architecture.md](architecture.md)，值语义见 [data-model.md](data-model.md)，命令参数和完整错误码见 [commands.md](commands.md)。本文只定义传输与封装，不以 Redis 协议为兼容目标。

## 连接与版本

底层为有序 TCP 字节流，可直接使用 TLS 包装；启用 TLS 时先完成 TLS 握手，再发送本协议。首版不支持压缩、明文连接内升级 TLS 或 Embedded 通道。

客户端首个应用帧必须是普通 `REQUEST HELLO`，参数为单个 `UInt(1)`。头部 Version 与 HELLO 参数都必须为 1；首版仅接受这一版本，不宣称支持多版本自动降级。

HELLO 成功返回 Map：`protocol: UInt(1)`、`server_version: String`、`events: Bool`、`reliable_events: Bool`、`stream_id: Bytes(16) | Null`。版本文本取实际构建版本，不能硬编码规划文档版本。两个能力字段说明当前实例已启用的模式；事件禁用时 stream_id 为 Null。重连订阅前 SDK 比较保存的 stream_id，不同则上报 ERR_STREAM_MISMATCH 并等待调用方显式重置，不静默新建同名组。

不支持的帧版本返回 `ERROR ERR_UNSUPPORTED_VERSION` 后关闭。HELLO 前的其他请求拒绝并关闭。完成 HELLO 后，启用认证的实例在认证成功前仅接受 `AUTH`、`PING`、`QUIT`；未认证业务命令返回 `ERR_UNAUTHENTICATED`。

初始化顺序为 TLS（可选）→ HELLO → AUTH（需要时）→ SELECT（需要时）→业务请求。HELLO 仅允许一次。密码和私钥不进入协议诊断日志。

## 固定帧头

全部多字节整数使用大端字节序。帧头固定 18 字节，偏移从 0 开始，不受平台结构体对齐影响。

| 偏移 | 字节数 | 字段 | 契约 |
|---|---|---|---|
| 0 | 2 | Magic | `0x4C47`，即 ASCII `LG` |
| 2 | 1 | Version | `0x01` |
| 3 | 1 | Frame Type | 下表定义 |
| 4 | 2 | Flags | 首版仅定义 bit 0 |
| 6 | 8 | Request ID | 请求关联号；推送和传输心跳为 0 |
| 14 | 4 | Payload Length | 不含帧头的字节数 |
| 18 | 指定长度 | Payload | 必须恰好符合对应帧类型的编码 |

| Frame Type | 值 | 发送方与用途 |
|---|---|---|
| REQUEST | `0x01` | 客户端命令请求 |
| RESPONSE | `0x02` | 服务端命令或 ACK 结果 |
| EVENT | `0x03` | 服务端已提交事件推送 |
| ACK | `0x04` | 客户端确认事件 |
| HEARTBEAT | `0x05` | 任一端发送的传输活跃通知 |
| ERROR | `0x06` | 服务端致命协议错误，随后关闭连接 |

Flags 的 `0x0001` 表示 BATCH，仅允许用于 REQUEST / RESPONSE。其他位必须为 0；首版不得将保留位解释为压缩或其他能力。普通响应、EVENT、ACK、HEARTBEAT 和 ERROR 的 Flags 均为 0。

客户端 REQUEST / ACK 共用同一 Request ID 序列，从 1 开始逐次加 1，连接内不得复用；到达 `u64` 上限后重建连接。RESPONSE 原样带回关联 ID。EVENT / HEARTBEAT 使用 0；ERROR 可使用已知的关联 ID，否则为 0。

## Value 编码

每个 Value 以一个类型标记开头。长度和元素数量均为无符号 `u32`，不能使用宿主语言的可变长整数或本机字节序。

| 类型 | 标记 | 标记之后的内容 |
|---|---|---|
| Null | `0x00` | 无 |
| Bool | `0x01` | `u8`，仅 0 / 1 |
| Int | `0x02` | `i64`，二进制补码 |
| UInt | `0x03` | `u64` |
| Float | `0x04` | 8 字节 IEEE 754 binary64 位模式 |
| String | `0x05` | `u32` 字节长度 + 有效 UTF-8 字节 |
| Bytes | `0x06` | `u32` 字节长度 + 原始字节 |
| List | `0x07` | `u32` 元素数量 + 顺序排列的 Value |
| Map | `0x08` | `u32` 字段数量 + 重复的（String Value 字段名、Value 字段值） |
| Set | `0x09` | `u32` 成员数量 + Scalar Value |

String / Bytes 的长度按字节计，不含类型标记或长度字段。Map 字段名自带 String 标记，非空且唯一；字段重复返回 `ERR_INVALID_VALUE`，不能覆盖前项。Map 与 Set 的编码次序不要求稳定，不通过原始字节相等判断其数据相等性。

Set 成员仅允许标记 `0x00..0x06`，先验证完整输入再按类型敏感相等性去重。NaN 不可存储；正负 Infinity 合法；编码器将 `-0.0` 规范为 `+0.0`，解码器接受两种零并规范化。完整规则以数据模型为准。

`engine.max_nest_depth` 按实际存储的 Value 根计算，根标量深度 0、根容器深度 1，Set 也计层。请求参数 List、响应结果 Map 等协议封套不占用业务 Value 深度。解码器另设 `max_nest_depth + 4` 的递归保护，解封后逐个校验实际 Value；不能为满足业务上限而无限递归。

## 普通请求与结果

普通 REQUEST 的 Payload 为连续两个 Value，无额外分隔符：

```text
String(command_name) + List(arguments)
```

发送端将命令名规范为大写 ASCII；CLI 输入大小写规则见命令规范。Key、字段、pattern、选项名等文本均为 String；参数保持命令规定的类型。CLI 的 `int(1)` 等字面量只在客户端解析，不作为线上字符串传输。

选项使用参数 List 内的 String 选项名及其后续值，例如 SET 的参数为 `[String(key), Value, String("PX"), Int(1000)]`。未知、重复或互斥选项按命令错误处理，不由 SDK 静默忽略。

CONFIG 的线上命令名固定为 String("CONFIG")，参数 List 第一项为 String("GET" / "SET" / "RELOAD")，后接该子命令参数。不把空格和子命令拼入 command_name。

SUBSCRIBE 的变长 pattern 与 GROUP 选项单独消歧：线上参数为 `[List<String>(patterns)]`，或 `[List<String>(patterns), String("GROUP"), String(group)]`，patterns 至少一项。不使用平铺的 pattern 字符串；CLI 仍使用命令规范的文本语法。

普通 RESPONSE 的 Payload 为以下两种之一：

```text
u8(0) + Value(result)
u8(1) + String(error_code) + String(message)
```

成功结果即使为 Null，也必须包含 Null 标记。失败错误码为稳定的 `ERR_*` String；message 为 UTF-8 诊断文本，不作为业务分支依据。未定义的 status 值属于非法帧。

`MGET` / `FGET` 等逐项读取返回 List，项结果使用以下 Map 表达，保留输入顺序及重复位置：

```text
{"ok": Bool(true),  "value": Value}
{"ok": Bool(false), "code": String(error_code), "message": String(message)}
```

不存在使用错误项，合法 Null 使用成功项，不能通过一个 Null 同时表示二者。命令级验证失败仍返回普通错误 RESPONSE，不构造伪造的逐项结果。

## 普通 Batch

BATCH REQUEST 的 Payload 为 `u32(count)`，随后按顺序拼接 count 个普通请求体；各请求体均为 String 命令名 + List 参数，不包含独立帧头、ID 或长度前缀。

BATCH RESPONSE 的 Payload 为 `u32(count)`，随后按原输入顺序拼接 count 个普通响应体，即每项自带 status 和相应结果。整批只有一个非零 Request ID；响应 Flags 为 BATCH。禁止嵌套 Batch。

完整解码和整批准入校验在任何子命令执行之前完成。count 必须为 `1..=protocol.max_batch_size`，各子命令必须属于 [commands.md](commands.md) 的 Batch 白名单。连接状态、订阅和管理命令不能混入 Batch。

整体准入失败返回普通 RESPONSE（Flags 为 0，status 为 1），一项都不执行。通过准入后，各子命令独立执行：错误保留在对应位置，不回滚已执行项。跨 Shard 可以并发；同一 Key 的子命令仍按输入顺序入队。

Batch 不提供事务或整体快照。`MSET` 等命令自身的多 Key 原子性独立于 Batch，不能通过 Batch 的非原子性降低其保证。连接中断可能使部分或全部子命令的结果未知。

整批准入先为 count 及每项最小错误结果预留响应空间，预留按注册表最长错误码和空诊断文本计算；连此空间都不足则整批拒绝。各子命令在发布修改前从共享预算预留完整结果（含封套），无法预留时该项返回 ERR_LIMIT_EXCEEDED 且不修改；诊断文本可截短到预留预算。跨 Shard 并行也使用同一预算，不能执行完所有 POP 后才发现总响应超限。

结构可完整界定的 NaN、重复字段、非法 Set 成员等属于单项业务值错误，记录该项失败而不阻止其他项执行。未知类型标记、畸形结构或硬解码上限导致无法安全界定时，整个帧 ERROR 后关闭；整帧解码完成前不执行任何子命令。

## 请求排序与连接状态

服务端按接收到的请求顺序入队，保证同一连接针对同一 Key 的操作顺序。跨 Shard 请求可以并行，RESPONSE 可乱序；客户端只能按 Request ID 匹配。不同连接之间没有由客户端发送时间推导出的先后保证。

`AUTH`、`SELECT`、`QUIT` 是连接状态屏障：先等待前序请求完成，再处理状态变更，其后请求使用变更后的状态。客户端必须等待屏障响应，再发送依赖新状态的请求。数据库编号在业务请求准入时确定，不在异步执行途中读取可变 SELECT 状态。

多 Key 原子命令按命令规范由服务端协调，不通过协议层拆成若干独立单 Key 请求。SDK 池连接按数据库及认证上下文固定分桶，禁止调用方并发改变共享池连接的 SELECT。

## 事件、ACK 与心跳

订阅使用专用连接。完成 HELLO / AUTH / SELECT 后发送 `SUBSCRIBE pattern ... [GROUP group]`；成功 RESPONSE 必须先于该订阅的 EVENT。订阅状态下仅接受 ACK、UNSUBSCRIBE、PING、QUIT 与 HEARTBEAT，不执行普通数据请求。

EVENT 的 Payload 是 Value::Map，字段与 [events.md](events.md) 一致：

| 字段 | Value 表达 |
|---|---|
| event_id | Map：`stream_id: Bytes(16)`、`db: UInt`、`shard: UInt`、`sequence: UInt` |
| event_type | String，六种事件名称之一 |
| key | String |
| generation | Map：`epoch: Bytes(16)`、`counter: UInt` |
| revision | UInt |
| timestamp_ms | UInt，Unix 毫秒 |

两个 16 字节身份字段均按 `u128` 的大端字节表示。db 的数值必须可表示为 `u16`、shard 为 `u32`，且符合实例范围；线上 UInt 仍统一使用 8 字节编码。event_id 是复合身份，不能压成一个有精度损失的数字。

ACK 帧的 Payload 是单个 event_id Map，使用新非零 Request ID。ACK 绑定连接唯一组或临时会话上下文；服务端以普通 RESPONSE 返回结果。Reliable 命名组成功表示确认状态已 fsync；临时会话只确认内存交付，Fast 不提供跨重启保证。CLI 的 `ACK event_id` 映射为 ACK 帧，不另行定义 REQUEST ACK。

HEARTBEAT 的 Payload 长度必须为 0，接收方不回显，避免相互回显循环。需要请求响应探活时使用 PING / PONG；单纯收到 HEARTBEAT 不代表业务写入或 ACK 已提交。

## 错误与资源边界

ERROR 的 Payload 为连续两个 String：错误码和诊断文本，之后关闭连接。魔数、帧版本、帧类型、保留位、帧长度、Value 标记、结构截断、UTF-8 或 Bool 编码非法属于协议错误；无法安全构造响应时可直接关闭。错误输出必须有界，不能复制整个非法 Payload。

结构有效的未知命令、参数类型错误、NaN、空/重复 Map 字段、Set 中的集合成员、Key 不存在或类型不匹配使用 RESPONSE 错误，不主动断开连接。业务 Value 大小或深度超限返回 ERR_LIMIT_EXCEEDED；无法在硬递归/分配保护内安全界定整个帧时可 ERROR 后关闭。代表性业务错误包括 `ERR_UNKNOWN_COMMAND`、`ERR_INVALID_ARGUMENT`、`ERR_INVALID_VALUE`、`ERR_KEY_NOT_FOUND`、`ERR_TYPE_MISMATCH`、`ERR_OVERFLOW`；完整集合以命令规范为准。

TCP 半包和粘包是正常情况：累积完整 18 字节头，再按长度读取 Payload，消费一帧后继续解析余下字节。不能将一次 socket read 当成一帧，也不能因尚未收齐就认定 Payload 截断；连接 EOF 时未完成帧才属于截断。

`protocol.max_frame_bytes` 包含 18 字节头；请求 / 响应 Payload 还受各自上限约束。所有长度先校验再分配，使用检查算术计算总长。容器 count 必须受剩余字节数及解码内存预算约束，禁止仅凭 count 提前分配任意容量。事件、错误及 Batch 也受帧上限约束。

无法在响应上限内完整返回时，命令在修改前返回限制错误，不先执行会丢失结果的 Pop 再报编码超限。写入结果、事件及元数据封装的额外空间必须纳入检查，不能认为合法最大 Value 一定能装入默认请求或响应。

调用方超时与取消不回滚已经接收的命令。客户端只需保存连续已发送 ID 高水位和有界待响应表；ID 不在表中且不大于高水位时按迟到/重复响应丢弃，不保存无限墓碑。ID 为 0 或大于发送高水位的 RESPONSE 视为连接失步。断线结算全部待响应调用，并明确可能存在写入结果未知，不自动重放非幂等操作。

## 候选字节向量

下列向量使用无 TLS 的应用字节；空格与换行仅供阅读。HELLO 请求 ID 为 1，Payload 为 String("HELLO") + List([UInt(1)])，共 42 字节：

```text
4c 47 01 01 00 00 00 00 00 00 00 00 00 01 00 00 00 18
05 00 00 00 05 48 45 4c 4c 4f 07 00 00 00 01 03 00 00 00 00 00 00 00 01
```

完成初始化后的 PING 请求 ID 为 2，参数为空，共 32 字节：

```text
4c 47 01 01 00 00 00 00 00 00 00 00 00 02 00 00 00 0e
05 00 00 00 04 50 49 4e 47 07 00 00 00 00
```

对应成功 PONG 响应共 28 字节：

```text
4c 47 01 02 00 00 00 00 00 00 00 00 00 02 00 00 00 0a
00 05 00 00 00 04 50 4f 4e 47
```

协议实现前须评审冻结线格式，并将这些向量扩展为所有 Value、错误、Batch、EVENT / ACK 的跨语言共享测试。冻结后不得重分配已使用标记、悄然改变单位或复用保留位；不兼容变更必须提升协议版本。
