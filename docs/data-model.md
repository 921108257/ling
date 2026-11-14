# 数据模型

> 状态：v0.3 数据契约。`linger-types` 已实现 10 种 Value、相等/哈希、大小/深度校验及稳定错误码；阶段 3 引擎已实现固定分片存储、运算、generation / revision、数据库隔离与跨库迁入身份。条目保留单调时钟和 Unix 毫秒期限字段，开放 TTL 命令及调度留到阶段 4。

项目边界见 [architecture.md](architecture.md)，命令行为见 [commands.md](commands.md)，二进制编码见 [protocol.md](protocol.md)。三者使用本文定义的 Value，不另行推导语言特有的类型语义。

## 数据库与 Key

服务默认提供 16 个数据库，编号从 `0` 开始。数据库数量配置范围为 `1..=65536`，协议中的数据库编号为 `u16`；实际编号必须小于配置数量。

每个数据库拥有独立命名空间。同名 Key 可以存在于不同数据库，值与 TTL 互不影响。数据库编号不是权限隔离机制。

Key 是非空 UTF-8 文本，默认最大长度为 1024 字节，按 UTF-8 编码后的字节计数。Key 大小写敏感，不进行 Unicode 归一化；视觉相同但字节不同的文本可以是不同 Key。Bytes 不能直接作为 Key。

新建条目的 generation 由随机进程 epoch 和全引擎创建计数组成，revision 从 1 开始；状态实际变化才递增，失败和无变化操作不消耗版本，删除重建使用新 generation。COPY / MOVE / RENAME 及 SWAPDB 的迁入条目取得新 generation 与 revision=1，保留源期限；清空不重置身份计数。Key 的 Value、版本与可选过期时间属于同一条记录。过期与删除使记录不可见，不改变 Value 中 Null 的定义。TTL 规则见 [ttl.md](ttl.md)。

## Value 类型

所有 10 种类型均可作为顶级 Value。不存在只能出现在集合内部的类型，也不存在隐式数值或文本转换。

| 类型 | 范围或内容 | 约束 |
|---|---|---|
| Null | 显式空值 | 与 Key 不存在不同 |
| Bool | `true` / `false` | 与整数 `0` / `1` 不同 |
| Int | 有符号 64 位整数 | `-2^63..=2^63-1` |
| UInt | 无符号 64 位整数 | `0..=2^64-1` |
| Float | IEEE 754 binary64 | 禁止 NaN，允许正负 Infinity |
| String | UTF-8 文本 | 允许空字符串，长度按字节计 |
| Bytes | 任意字节序列 | 允许空序列，不自动转成 String |
| List | 有序 Value 序列 | 允许混合类型及嵌套 |
| Map | String 到 Value 的映射 | 字段名非空且唯一 |
| Set | 唯一 Scalar 集合 | Scalar 为上述 7 种非集合类型 |

`TYPE` 返回小写名称：`null`、`bool`、`int`、`uint`、`float`、`string`、`bytes`、`list`、`map`、`set`。

Map 字段名与 Key 使用相同的 UTF-8、大小写及不归一化规则。Map 不是任意 JSON 对象的别名：它允许 UInt、Bytes、Set、Infinity 等 JSON 无法原样表达的类型。

## Null 与不存在

`SET key null` 创建一个存在的 Key，随后 `GET key` 成功返回 `Value::Null`，`EXISTS key` 计入存在数量。

不存在或已逻辑过期的 Key，`GET` 返回 `ERR_KEY_NOT_FOUND`。SDK 必须将成功空值和错误分开，不能同时映射为 `None`、`nil` 或 `null`。

`MGET` 按输入顺序返回逐项结果。合法 Null 是成功项；不存在是错误项；某项失败不使其他项的结果丢失。Map 字段读取的缺失行为由命令规范定义，不能通过填充 Null 隐藏缺失。

## 相等性与哈希

相等判断首先比较类型，然后比较该类型的内容：

- `Int(1)`、`UInt(1)`、`Float(1.0)` 两两不相等，`Bool(true)` 也不与它们相等。
- String 按 UTF-8 字节比较，Bytes 按原始字节比较；即使字节相同，String 与 Bytes 也不相等。
- Float 的 `+0.0` 与 `-0.0` 是同一个值，存储及协议编码统一为 `+0.0`；正负 Infinity 分别只与自身相等。
- List 的长度、元素顺序及各项 Value 均相等时，两个 List 相等。
- Map 按字段名与字段值比较，字段插入顺序不影响相等性。
- Set 按 Scalar 成员比较，成员插入顺序不影响相等性。

`LREM`、`SADD`、`SREM`、成员查询及集合运算必须使用上述规则。Scalar 哈希包含类型标识，并与相等性一致。协议不要求不同语言产生相同的运行时哈希值。

Set 输入中的重复成员去重；必须先验证完整输入，再执行去重和写入。序列化 Map 中字段名重复属于无效值，返回 `ERR_INVALID_VALUE`，不能静默选择第一个或最后一个值。

Map 和 Set 的遍历、编码及显示顺序不承诺稳定。测试应按数据语义比较；需要可复现的展示时可在客户端排序，但排序不成为服务端存储契约。

## 深度与大小限制

标量深度为 `0`。每个 List、Map 或 Set 容器增加一层，容器深度为 `1 + max(子值深度)`，空容器深度为 `1`。Map 的字段名不计层数；Set 只容纳标量，因此其自身深度始终为 `1`。

示例：

| 值 | 深度 |
|---|---|
| `int(1)` | 0 |
| `list([])` | 1 |
| `set([int(1)])` | 1 |
| `map({"items": list([int(1)])})` | 2 |
| `list([set([int(1)])])` | 2 |

默认最大容器深度为 8，适用于完整值及每次修改后的值。向已有容器写入子值时，不能只校验输入子树而忽略其父层级。

Key、Value、请求帧与响应帧有各自的大小限制，见 [config.md](config.md)。可编码为合法 Value 不表示任意命令封装后仍能通过请求上限。解码器必须在分配内存前检查长度，在递归前检查深度。

## 数值运算

`INCR` / `DECR` 的步长固定为 1，仅作用于已有 Int 或 UInt。`INCRBY` / `DECRBY` 的参数是 Int 范围内的有符号整数，可以为负数。实现使用足够宽的中间值检查结果，不允许中间溢出或环绕。

结果必须保留原类型：Int 不能溢出到 UInt；UInt 结果小于 0 或大于 `2^64-1` 均返回 `ERR_OVERFLOW`。`DECRBY` 的负参数先参与有符号计算，不能在 i64 内直接取反最小负数。

`INCRFLOAT` 要求已有 Float 和 Float 参数。输入中的 NaN 或计算产生 NaN 返回 `ERR_INVALID_VALUE`。两个有限操作数计算得到 Infinity 属于 `ERR_OVERFLOW`；显式写入或传入 Infinity 合法。失败不修改原值。

## 写入与容器生命周期

`SET`、`GETSET`、`MSET` 接受任意 Value，不专属于 String。`SET` 未指定 TTL 时清除旧 TTL；`GETSET` 替换值并清除旧 TTL。

数值、String、Bytes 及容器专用命令要求 Key 已存在且类型匹配。缺失返回 `ERR_KEY_NOT_FOUND`，类型错误返回 `ERR_TYPE_MISMATCH`。需要计数器或容器时，先通过 `SET` 显式初始化，避免由命令隐式猜测类型。

对已有 Value 的原地修改保留 TTL。删除最后一个 List / Map / Set 成员后，Key 仍然存在，值为空容器，类型与 TTL 保持不变。需要删除 Key 时使用 `DEL`。

失败操作不得留下部分字段、部分成员或部分数值更新。自然到期导致的不可见变化独立于命令是否成功。多个 Key 的一致性和原子性范围按 [commands.md](commands.md) 执行，普通 Batch 不是事务。

## CLI 与 SDK 表达

CLI 使用能保留类型的 Value 字面量，例如 `int(1)`、`uint(1)`、`float(1.0)`、`bytes(0x0102)`、`"text"`、`null`、`true`、`list([...])`、`map({...})`、`set([...])`。

上述为类型表达示意，完整命令语法以 [commands.md](commands.md) 为准。协议传输类型标记后的二进制值，不传输 CLI 文本；SDK 不得通过 JSON 中转而丢失整数精度、Bytes、Set 或 Infinity。

## 验收要求

实现本契约时，使用跨语言共享用例覆盖 10 种类型、整数边界、Infinity、零归一化、Unicode 原始字节、嵌套深度和空集合。

必须额外覆盖 Null 与缺失的区分、类型敏感去重、重复 Map 字段拒绝、嵌套修改后的大小与深度校验、溢出失败无副作用及空容器保留。具体运行方式在测试设施存在后记录，不在规划期虚构测试通过状态。
