# 命令契约

> 状态：v0.3 命令契约。阶段 6 已完成本文公开命令、普通 Batch 和运维入口；阶段 7 已完成本机性能与资源验收，当前为 0.1.2 预发布版本，稳定发布需目标环境评估。类型见 [data-model.md](data-model.md)，编码见 [protocol.md](protocol.md)，客户端操作见 [cli.md](cli.md)。名称相似不意味着兼容 Redis。

## 当前支持范围

| 类别 | 阶段 6 已开放 |
|---|---|
| 连接 | HELLO、AUTH、SELECT、PING、QUIT |
| 通用 | SET（含 EX / PX / EXAT / PXAT，可选 NX 或 XX）、GET、GETSET、MGET、MSET、DEL、EXISTS、TYPE、DBSIZE |
| TTL | EXPIRE、PEXPIRE、EXPIREAT、PEXPIREAT、PERSIST、TTL、PTTL、EXPIRETIME、PEXPIRETIME |
| 迁移与查询 | RENAME、RENAMENX、COPY、MOVE、KEYS、SCAN、RANDOMKEY |
| 数据库管理 | FLUSHDB、FLUSHALL（均含 ASYNC）、SWAPDB |
| 运维 | INFO、CONFIG GET / SET / RELOAD、SHUTDOWN |
| 事件 | SUBSCRIBE、UNSUBSCRIBE、ACK 帧 |
| 数值与文本 | INCR、DECR、INCRBY、DECRBY、INCRFLOAT、APPEND、STRLEN、BLEN |
| List | 本文 List 表中的全部命令 |
| Map | 本文 Map 表中的全部命令 |
| Set | 本文 Set 表中的全部命令，含跨分片集合代数与 STORE |

已开放的多 Key 和跨库命令按 (db, shard) 固定顺序取得执行权后原子发布；整体数据库操作允许阻塞相关数据库。事件默认 Fast，可配置 Reliable。普通 Batch 按白名单准入、顺序执行并逐项返回；CLI 只有显式 `--batch` 才提交协议 Batch，默认多行输入逐行执行，均不等于事务。

## 通用规则

命令名及选项名大小写不敏感；客户端在线上规范化为大写 ASCII。Key、字段、组名和值大小写敏感。Key 与字段为非空 String；其余 String 可以为空。

表中 `Bool` / `UInt` / `Int` / `Value` 指协议类型；“列表”均为 List。计数成功返回 UInt，TTL 的负哨兵值返回 Int。无业务返回值的成功操作统一返回 Bool(true)。

不存在或已过期的 Key 默认返回 `ERR_KEY_NOT_FOUND`；类型不匹配返回 `ERR_TYPE_MISMATCH`。下表明确指定计数、Bool 或逐项结果的命令除外。类型专用写命令不隐式创建 Key，调用方先 `SET` 初始化。空容器持续存在。

所有参数及最终值在修改前校验。参数、类型、容量或条件检查失败不得部分修改；到期导致不可见独立于命令成败。提交中断与写盘结果不确定的错误见下文，不能按普通校验失败理解。

普通协议参数：Key / 字段 / pattern / 选项为 String，值为实际 Value，数据库编号和 SCAN cursor 为 UInt，索引与时间为 Int。非负 count 和正整数 hint 为 UInt；`LREM` 与 `SRANDMEMBER` 的有符号 count 为 Int。数字不通过 String 传输。

## 原子性与顺序

单 Key 操作原子执行。多 Key 命令在单机内通过协调器取得全部相关 Shard 执行权：

| 操作 | 保证 |
|---|---|
| `MGET`、`EXISTS`、集合读运算、`DBSIZE`、`KEYS` | 对命令涉及的 Key 在同一观察时点读取 |
| `MSET`、`DEL`、`RENAME`、`RENAMENX`、`MOVE`、`COPY`、集合 `*STORE` | 校验全部完成后原子发布，无部分成功 |
| `FLUSHDB`、`FLUSHALL`、`SWAPDB` | 对所涉及数据库整体生效，允许阻塞这些库 |
| `SCAN` | 单页合法；多页间为弱一致遍历，不是全库快照 |
| 普通 Batch | 子命令各自遵守契约，整体不原子、不回滚 |

同一连接同 Key 的操作按接收顺序执行，多 Key 操作作为涉及各 Shard 的顺序屏障。不同连接的竞争以服务端实际序列化顺序为准。命令开始执行并取得执行权时采样时间；等待期间已经过期的值不可用。

多 Key 读取中的 missing 可逐项返回；`MSET` 等写命令某项不合法则整条命令失败。不能以自动拆成若干单 Key 调用实现其原子性。

Batch 白名单是下文通用 Key、TTL、数值、String、Bytes、List、Map、Set 章节中的命令，但不含 KEYS、SCAN、RANDOMKEY。数据库、服务器、管理、订阅及连接命令均不允许进入 Batch，包括 PING、DBSIZE、INFO。协议对整批做准入校验，出现非白名单命令时一项都不执行。

Batch 在执行前预留每项稳定错误码的响应空间，随后按剩余预算执行子项；例如 POP 在无法返回完整 Value 时不得先删除。前项业务错误不阻止后项执行；诊断文本可为空，稳定错误码保留。整批只发送一个结果帧，发送后断线或未收齐响应时客户端不能推断哪些项已提交。

## 通用 Key 命令

| 命令 | 返回与行为 |
|---|---|
| `SET key value [EX seconds \| PX ms \| EXAT unix_sec \| PXAT unix_ms] [NX \| XX]` | Bool：实际设置 true，条件不满足 false；允许替换成任意类型。未给 TTL 清除旧 TTL |
| `GET key` | 原 Value；缺失报错 |
| `GETSET key value` | 返回旧 Value，再整体替换并清除 TTL；缺失报错，不创建 |
| `MGET key [key ...]` | 按输入顺序返回逐项成功 Value 或错误；重复 Key 保留位置；所有类型适用 |
| `MSET key value [key value ...]` | Bool(true)；原子整体写入，每个 Key 清除 TTL；重复 Key 拒绝 |
| `DEL key [key ...]` | 实际删除的 UInt 数量；缺失忽略，重复 Key 只计一次 |
| `EXISTS key [key ...]` | 存在的不同 Key 的 UInt 数量，重复 Key 只计一次 |
| `TYPE key` | 类型名称 String；缺失报错 |
| `RENAME key newkey` | Bool(true)；保留源 Value 与到期期限，目标存在则替换；源缺失报错；同名为成功无操作 |
| `RENAMENX key newkey` | 仅当目标不存在时重命名；Bool 表示是否发生，源缺失报错；同名返回 false |
| `COPY key dest [DB db] [REPLACE]` | Bool 表示是否复制；默认当前库，不覆盖已有目标；源缺失报错；同库同名非法 |
| `MOVE key db` | Bool 表示是否移动；目标同名存在或目标为当前库时 false，源缺失报错 |
| `KEYS pattern` | 所有匹配活跃 Key 的 String 列表，无顺序保证；超过响应上限则失败，不返回截断结果 |
| `SCAN cursor [MATCH pattern] [COUNT hint] [TYPE type]` | Map：`cursor: UInt, keys: List<String>`；见遍历约定 |
| `RANDOMKEY` | 活跃 Key 的 String；空库报 `ERR_KEY_NOT_FOUND`，不保证密码学随机 |

COPY、MOVE、RENAME 复制的是原单调期限及用于查询的 Unix 期限，不能重新开始 TTL，也不能按当前墙钟重新换算；到期源视为不存在。目标新生命周期分配新 generation，来源移动/重命名终结旧生命周期。覆盖目标的修订与事件规则见 [ttl.md](ttl.md) 和 [events.md](events.md)。

`SET` 四种 TTL 选项互斥，NX / XX 互斥；相对间隔必须为正、绝对期限必须晚于执行时间。参数错误先报错，不能因条件不满足而掩盖非法值。状态完全相同的重复设置成功但不产生新事件。

`MDEL`、`MEXISTS` 不列为额外命令，使用多参数 DEL、EXISTS。批量读取不局限于 String。

## TTL 命令

| 命令 | 返回与行为 |
|---|---|
| `EXPIRE key seconds` | Bool；存在则设置相对秒期限，否则 false |
| `PEXPIRE key milliseconds` | Bool；存在则设置相对毫秒期限，否则 false |
| `EXPIREAT key unix_seconds` | Bool；设置绝对秒期限 |
| `PEXPIREAT key unix_milliseconds` | Bool；设置绝对毫秒期限 |
| `PERSIST key` | 移除了 TTL 才返回 true；缺失或无 TTL 返回 false |
| `TTL key` / `PTTL key` | Int；剩余秒 / 毫秒；无 TTL 为 -1，缺失为 -2 |
| `EXPIRETIME key` / `PEXPIRETIME key` | Int；到期 Unix 秒 / 毫秒；无 TTL 为 -1，缺失为 -2 |

EXPIRE 系列返回 true 表示接受对存在 Key 的操作，设为相同期限仍可返回 true，但不增加修订号。非正相对间隔或不晚于现在的绝对时间立即过期，产生 EXPIRED 而非 DELETED；缺失 Key 返回 false。秒值向下取整。详细时钟与回收规则见 [ttl.md](ttl.md)。

## 数值、String 与 Bytes

| 命令 | 类型与结果 |
|---|---|
| `INCR key` / `DECR key` | 已有 Int / UInt，步长 1，返回原类型的新值 |
| `INCRBY key n` / `DECRBY key n` | 已有 Int / UInt；n 为有符号 Int，返回原类型新值 |
| `INCRFLOAT key delta` | 已有 Float，delta 为 Float；返回新 Float |
| `APPEND key string` | 已有 String；返回追加后的 UInt 字节数 |
| `STRLEN key` | 已有 String；UInt 字节数 |
| `BLEN key` | 已有 Bytes；UInt 字节数 |

整数越界、有限浮点数相加溢出返回 `ERR_OVERFLOW`；NaN 输入或结果返回 `ERR_INVALID_VALUE`。Infinity、正负零及跨类型相等规则见数据模型。局部修改保留 TTL，失败不修改。Bytes 没有隐式 String 操作。

## List

| 命令 | 返回与行为 |
|---|---|
| `LPUSH key val [val ...]` | UInt 新长度；按参数顺序逐个推入头部，`a b` 的结果以 `b a` 开头 |
| `RPUSH key val [val ...]` | UInt 新长度；按参数顺序追加尾部 |
| `LPOP key [count]` / `RPOP key [count]` | 始终返回 List，默认 count=1；按实际弹出顺序返回，最多 count 项 |
| `LRANGE key start end` | List；闭区间，支持负索引，越界截到有效范围，空区间返回空列表 |
| `LINDEX key index` | 单个 Value；越界报 `ERR_INDEX_OUT_OF_RANGE` |
| `LSET key index value` | Bool(true)；越界报错，无隐式扩容 |
| `LLEN key` | UInt 长度 |
| `LREM key count value` | UInt 删除数；正数从头、负数从尾、0 表示全部，按 Value 相等性删除 |
| `LTRIM key start end` | Bool(true)；保留闭区间，其余删除；空区间使 Key 保留为空 List |

负索引 `-1` 表示最后一项。POP count=0 返回空列表且不修改；超过长度弹出全部。Key 缺失报错，存在的空 List 返回空列表或零。所有修改先验证完整输入和最终深度、大小。

## Map

| 命令 | 返回与行为 |
|---|---|
| `FSET key field value [field value ...]` | UInt 新增字段数；已有字段覆盖；同一请求重复字段非法 |
| `FSETNX key field value` | Bool；仅字段不存在时插入 |
| `FGET key field [field ...]` | 始终按参数顺序返回逐项结果列表；缺失字段为 `ERR_FIELD_NOT_FOUND` |
| `FDEL key field [field ...]` | UInt 实际删除数；缺失字段忽略，重复字段只计一次 |
| `FEXISTS key field` | Bool；字段值 Null 也算存在 |
| `FKEYS key` | String 列表 |
| `FVALS key` | Value 列表 |
| `FGETALL key` | Map |
| `FLEN key` | UInt 字段数 |
| `FMERGE key map_value` | UInt 新增字段数；浅合并，已有同名字段整体替换，不递归合并 |

FGET 的整个 Key 缺失或类型错误时整条命令失败，不伪造成字段缺失。FKEYS / FVALS 不保证跨调用顺序对应，需要配对时用 FGETALL。所有 Map 修改保留 TTL。

## Set

| 命令 | 返回与行为 |
|---|---|
| `SADD key member [member ...]` | UInt 实际新增数，仅 Scalar，输入重复去重 |
| `SREM key member [member ...]` | UInt 实际删除数，缺失成员忽略 |
| `SISMEMBER key member` | Bool |
| `SMISMEMBER key member [member ...]` | Bool 列表，保留参数顺序 |
| `SMEMBERS key` | Set |
| `SCARD key` | UInt |
| `SUNION key [key ...]` / `SINTER key [key ...]` / `SDIFF key [key ...]` | Set；分别为并、交、以第一个为基准的差集 |
| `SUNIONSTORE dest key [key ...]` / `SINTERSTORE ...` / `SDIFFSTORE ...` | UInt 结果基数；原子写入 dest，整体替换并清除目标 TTL |
| `SRANDMEMBER key [count]` | 始终返回 List；默认 1；正数不重复、最多基数；负数允许重复、返回绝对值项 |
| `SPOP key [count]` | 始终返回 List；默认 1；随机删除不重复成员、最多基数 |

集合运算任何源缺失或类型不符则整条失败；空 Set 作为合法输入参与运算。STORE 的 dest 可同时是源，先读取原状态后整体发布；空结果仍存为空 Set。count=0 返回空列表；空 Set 的随机读取/弹出均返回空列表，即使负 count 也不制造成员。随机结果无顺序或密码学强度保证。

## 数据库与服务器

| 命令 | 返回与行为 |
|---|---|
| `HELLO version` | 协议能力 Map；连接初始化，首版只支持版本 UInt(1) |
| `AUTH password` | Bool(true)；失败 `ERR_AUTH_FAILED`，密码不回显 |
| `SELECT db` | Bool(true)；切换连接数据库，越界报错 |
| `PING [message]` | 默认 String("PONG")；携带 String 时原样返回 |
| `QUIT` | Bool(true)，然后关闭连接 |
| `INFO [section]` | Map；section 为 server/memory/stats/clients/keyspace/events/all，默认 all |
| `DBSIZE` | 当前库活跃 Key 的 UInt 数量 |
| `FLUSHDB [ASYNC]` / `FLUSHALL [ASYNC]` | Bool(true)；原子逻辑清空，ASYNC 仅延后物理释放 |
| `SWAPDB db1 db2` | Bool(true)；原子交换全部活跃数据及期限，同库为无操作 |
| `CONFIG GET pattern` | Map；匹配配置项名，隐藏密码与密钥内容 |
| `CONFIG SET key value` | Bool(true)；首版仅 log.level 可动态修改 |
| `CONFIG RELOAD` | Bool(true)；全部校验后仅应用允许动态修改项；出现静态项变化则整次拒绝 |
| `SHUTDOWN` | 确认已接受关闭请求后停止接入并排空，受关闭期限约束 |

INFO 返回以 section 名为顶层键的 Map；即使只请求一个 section，也保留这层封套。未知 section 或错误参数返回 `ERR_INVALID_ARGUMENT`。各字段按顺序采样，不是全实例原子快照；keyspace 逐库统计活跃 Key。memory 的 managed_bytes 是受管预算，不冒充 RSS。

| section | 字段 |
|---|---|
| server | version、uptime_ms、shards_per_database |
| memory | managed_bytes、limit_bytes、scheduled_expirations |
| stats | requests、errors（已完成请求，不包含当前 INFO） |
| clients | connected |
| keyspace | db0、db1 等，对应活跃 Key 数 UInt |
| events | committed、dropped、redelivered、journal_bytes、journal_errors、groups、pending、oldest_retained_ms、by_kind、consumer_groups |

事件统计可用时返回 events；by_kind 按 CREATED / UPDATED / EXPIRED / DELETED / EVICTED / TTL_CHANGED 顺序排列。consumer_groups 为 Map 列表，包含 name、db、expired、pending、oldest_pending_ms 和 `[shard, sequence]` watermarks；无可用时间时返回 Null。结果超过响应预算返回 `ERR_LIMIT_EXCEEDED`。

CONFIG GET 的 pattern 按字节匹配标准扁平字段名，支持 `*` 与 `?`，其他字节按字面匹配；密码固定返回 `<redacted>`。CONFIG 子操作和 log.level 的规则见 [config.md](config.md#运行时变更)。SHUTDOWN 在尝试写出 Bool(true) 后广播关闭信号，随后停止接入并排空；应答失败也执行已接受的关闭，已开始的阻塞命令不因关闭期限而回滚。

SWAPDB 仅交换缓存数据，不交换事件流、组、历史或数据库编号。迁入目标库的每条数据取得目标 generation，原期限不变；事件按各目标最终状态描述：活跃目标替换为 UPDATED，空目标为 CREATED，只移出为 DELETED，已到期条目保留 EXPIRED 原因；期限实际变化另有 TTL_CHANGED。这与 events 的目标覆盖规则一致，不把同名替换拆为删除和创建。Reliable 在一个事务中提交这些事件，允许该操作耗时或因容量限制拒绝。

当前阶段的 FLUSH ASYNC 在返回前移出全部索引；待释放条目持续计入受管内存，直至后台物理释放，期间新写入可能因预算不足被拒绝。回收队列满或线程不可用时同步释放，不承诺 ASYNC 一定低延迟。Reliable 在逻辑移除前同步完整清库事务；ASYNC 仅延后物理释放。

首版没有 SAVE、SHUTDOWN NOSAVE、DEBUG SLEEP / JMAP。调试钩子如测试需要可在测试环境实现，不作为公开生产协议。

AUTH、SELECT、QUIT、HELLO 与管理命令的连接权限、屏障和订阅限制见协议规范。认证是实例级权限，首版没有按数据库或命令的 ACL。

## 订阅

`SUBSCRIBE pattern [pattern ...] [GROUP group]` 返回 Bool(true) 后进入专用订阅状态，随后推送 EVENT。一个连接只能有一个组上下文；省略 GROUP 是临时会话。`UNSUBSCRIBE` 结束本会话并返回 Bool(true)，不删除命名组。

CLI 将未引用的 GROUP 识别为末尾组选项；作为 pattern 的 GROUP 必须加双引号。线上把 patterns 打包为第一个 List 参数，避免丢失引号信息后歧义；具体见协议。组名为 1..128 字节的 UTF-8 String。

CLI 以 `-c 'SUBSCRIBE ...'` 进入订阅模式，默认逐条从 stdin 读取 ACK，可输入 PING / UNSUBSCRIBE / QUIT；`--auto-ack` 才会在输出刷新后确认，`--count N` 在 N 条处理后退出（0 为持续消费）。当前不在普通逐行输入中切换为异步订阅。

`ACK event_id` 在 CLI / SDK 中确认复合事件 ID，在线上映射为 ACK 帧；成功 Bool(true)。没有订阅上下文、错误 stream、未曾交付给当前组或临时会话的事件不得确认。Reliable 命名组的成功 ACK 表示状态已持久化；临时会话只确认内存交付。组与保留语义见 [events.md](events.md)。首版不支持部分 UNSUBSCRIBE。

## 遍历与匹配

pattern 使用 UTF-8 字节匹配：`*` 匹配任意字节序列，`?` 匹配一个字节，`\` 转义下一字节。无正则、无字符组。末尾单独反斜杠为非法 pattern。对非 ASCII 文本需要注意一个字符可能占多个字节。

SCAN cursor 是不透明 UInt，初始输入 0，返回 0 表示完成。cursor 绑定当前连接、数据库及 MATCH / TYPE 过滤；非法、重连后的或已失效 cursor 返回 `ERR_INVALID_CURSOR`。每连接最多一条扫描，新的 cursor=0 替换旧扫描，断线释放。

COUNT 默认 100，为正数工作量提示，单页最多处理 10,000 个工作单位（条目或空分片），不保证页大小；允许空页但非零 cursor。COUNT 可在续页时调整，MATCH / TYPE 必须保持一致。每个成功的非末页返回新票号，旧票号失效；成功切换数据库也释放游标。持续存在且匹配的 Key 在完整扫描中至少出现一次，可能重复；扫描期间变化的 Key 无完整性保证。当前用每分片有序 Key 的排他下界推进，不保存全库快照。超过单项响应容量返回错误，不截断 Key，响应失败不消耗游标进度。

## CLI 值语法

客户端本地 `HELP` / `HELP command` 和 `--cheatsheet` 提供速查，不发送数据库请求，不扩展协议命令。单命令帮助无需建立连接；普通逐行输入的 HELP 由 CLI 本地处理，`--batch` 中出现 HELP 则整批不提交。帮助语言与使用示例见 [cli.md](cli.md#先查帮助)。

命令行分词保留引号与括号内空白。Key / 字段 / 选项可用无空白裸词或双引号 String；Value 位置只接受以下带类型语法，不猜测裸数字类型：

```text
value  := null | true | false | string
        | int(signed-decimal) | uint(unsigned-decimal)
        | float(decimal-or-inf) | bytes(0xHEX) | bytes(b64"BASE64")
        | list([value, ...]) | map({string: value, ...}) | set([scalar, ...])
```

string 使用 JSON 双引号转义规则，拒绝无效 UTF-8 / 未配对代理项；这仅是文本输入规则，不代表通过 JSON 序列化 Value。Int / UInt 只接受十进制，Float 接受十进制小数或指数及 `inf` / `-inf`，拒绝 NaN；Hex 必须偶数位，Base64 使用标准带填充格式。重复 Map 字段拒绝。

固定数值参数按命令签名解析：索引、时间、计数、数据库编号、版本和 INCRBY / DECRBY 的 n 可直接输入十进制整数；INCRFLOAT 的 delta 输入 `float(...)`。任意 Value 位置仍必须显式保留类型。空列表、空 Map 和空 Set 分别写作 `list([])`、`map({})`、`set([])`。

CLI 将事件 ID 显示为 `32位小写十六进制stream_id/db/shard/sequence`，后三项为无符号十进制；ACK 接受这个文本并转为协议 Map，不把它当作普通 Key。

```text
SET "profile" map({"name": "Lin", "visits": uint(1)}) PX 60000
SET "counter" int(0)
INCRBY "counter" 2
SET "nothing" null
GET "nothing"
```

客户端输出必须区分类型、Null 与错误；空 String / Bytes / 容器不能显示成缺失。批处理执行的进程退出码成功为 0，任何命令或客户端失败为非零；不把部分成功隐藏为整体成功。

## 错误码

错误码是稳定 String，说明文本不作为业务判断条件。以下为首版注册表；实现不得自行添加同义错误码：

| 错误码 | 含义 |
|---|---|
| `ERR_UNKNOWN_COMMAND` / `ERR_INVALID_ARGUMENT` | 命令不存在；参数数量、选项或范围非法 |
| `ERR_KEY_NOT_FOUND` / `ERR_FIELD_NOT_FOUND` | Key 缺失；Map 字段缺失 |
| `ERR_TYPE_MISMATCH` / `ERR_INVALID_VALUE` | 类型不符；Value 编码语义或内容非法 |
| `ERR_OVERFLOW` / `ERR_INDEX_OUT_OF_RANGE` | 算术越界；单项索引越界 |
| `ERR_LIMIT_EXCEEDED` / `ERR_OUT_OF_MEMORY` | 请求/值/响应等限制；缓存预算不足 |
| `ERR_DB_OUT_OF_RANGE` / `ERR_INVALID_CURSOR` | 库号非法；游标无效 |
| `ERR_UNAUTHENTICATED` / `ERR_AUTH_FAILED` | 未认证；认证失败 |
| `ERR_PROTOCOL` / `ERR_UNSUPPORTED_VERSION` | 致命帧错误；版本不支持 |
| `ERR_CONNECTION_STATE` / `ERR_BUSY` | 当前连接状态不允许；过载或正在关闭 |
| `ERR_CONFIG_INVALID` / `ERR_CONFIG_IMMUTABLE` | 配置非法；静态配置不允许热改 |
| `ERR_EVENTS_DISABLED` / `ERR_GROUP_MISMATCH` | 未启用事件；组定义与请求不同 |
| `ERR_INVALID_ACK` / `ERR_OFFSET_EXPIRED` / `ERR_STREAM_MISMATCH` | ACK非法；消费位置超期；事件流身份改变 |
| `ERR_JOURNAL_UNAVAILABLE` / `ERR_JOURNAL_FULL` | Journal 不可写；日志容量不足，明确在提交前拒绝 |
| `ERR_COMMIT_UNKNOWN` | 提交结果无法确认，不保证没有发生，禁止自动重放写入 |

本地超时、取消与连接断开由 SDK 用独立客户端错误表达，也可能标记结果未知。服务端请求期限到达只能在尚未开始时明确拒绝；开始提交后不以超时伪装回滚。

FGET / MGET 逐项结果在协议中使用 `{ok: true, value: ...}` 或 `{ok: false, code: ..., message: ...}` 的 Map；这是响应包装，不是存储 Value 的新类型。顶层错误与协议致命错误的承载方式见 [protocol.md](protocol.md)。
