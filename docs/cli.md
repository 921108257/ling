# ling-cli 使用与命令速查

`ling` 启动数据库服务，`ling-cli` 是通过 Rust SDK 连接服务的命令行客户端。当前版本为 0.1.1 预发布版本。安装、后台服务、升级与卸载见 [安装指南](install.md)；本页介绍客户端的实际用法。命令的完整错误、原子性和返回值契约见 [commands.md](commands.md)。

## 先查帮助

以下入口在本地显示帮助，不要求启动数据库或提供密码：

```sh
ling --help
ling-cli --help
ling-cli --cheatsheet
ling-cli -c 'HELP'
ling-cli -c 'HELP SET'
ling-cli --lang zh-CN --cheatsheet
```

`HELP` 显示命令速查，`HELP <命令>` 显示该命令的语法和说明。进入 `ling-cli` 后也可输入 `HELP` 或 `HELP SCAN`；帮助由客户端处理，不发给服务器，不改变数据库。普通标准输入可包含帮助行，`--batch` 中不允许 `HELP`，发现后整批不提交。

`ling --help` 和 `ling-cli --help` 的帮助语言支持美式英语 `en-US`、简体中文 `zh-CN`、韩语 `ko-KR`、日语 `ja-JP`、俄语 `ru-RU`、德语 `de-DE`、法语 `fr-FR`。选择顺序为 `--lang`、`LING_LANG`、`LC_ALL`、`LC_MESSAGES`、`LANG`、操作系统语言；没有支持的语言时使用 `en-US`。`C` / `POSIX` 使用英语；繁体中文不自动转换为简体中文。命令名、选项名和类型字面量不翻译，帮助语言不改变协议、数值或数据内容。

```sh
ling --lang ja-JP --help
ling-cli --lang de-DE --help
```

## 连接与执行

服务默认监听 `127.0.0.1:6380`，客户端默认选库 0。在一个终端运行 `ling`，另一个终端执行：

```sh
ling-cli -c 'PING'
ling-cli --host 127.0.0.1 --port 6380 --db 1 -c 'SET greeting "hello"'
ling-cli --db 1 -c 'GET greeting'
```

`PING` 输出 `"PONG"`，设置成功输出 `true`，读取输出 `"hello"`。`--host` 当前接受 IP 地址；使用 TLS 时验证的域名通过 `--tls-server-name` 单独指定。

本页包含双引号的 `-c` 示例适用于 POSIX shell 或 PowerShell 7.3+。Windows PowerShell 5.1 的外部程序参数转义不同，建议将命令写入 UTF-8 文件，再按下文通过标准输入执行，保留 Value 的双引号。

| 选项 | 用途与默认值 |
|---|---|
| `--host <IP>` | 服务器 IP，默认 `127.0.0.1` |
| `--port <PORT>` | TCP 端口，默认 `6380` |
| `--db <DB>` | 连接初始化后的数据库，默认 `0` |
| `--timeout-ms <MS>` | 每次请求总时限，默认 `30000`，必须大于零 |
| `-c, --command <TEXT>` | 执行一条命令；省略则逐行读取标准输入 |
| `--batch` | 收集标准输入后提交一个普通 Batch，不能与 `-c` 共用 |
| `--count <N>` | 订阅处理 N 条后退出，默认 `0` 表示持续消费 |
| `--auto-ack` | 订阅输出并刷新后自动确认，默认不自动确认 |
| `--tls-server-name <NAME>` | 启用 TLS 并验证服务器证书名称 |
| `--tls-ca <FILE>` | 自定义可信 CA，要求启用 TLS |
| `--tls-cert <FILE>`、`--tls-key <FILE>` | 成对提供的客户端证书和私钥，用于双向 TLS |
| `--lang <LOCALE>` | 指定本地帮助语言 |
| `--cheatsheet` | 显示本地命令速查并退出 |
| `-h, --help` | 显示命令行帮助并退出 |
| `-V, --version` | 显示版本并退出 |

CLI 自动完成 HELLO、可选 AUTH 和 SELECT。省略 `-c` 可在同一连接连续输入命令，以 `QUIT` 结束。不要在初始化后的连接重新执行 HELLO；它是协议初始化命令，会得到连接状态错误。

也可传入 UTF-8 文本文件：

```sh
ling-cli < commands.txt
```

PowerShell 中使用 `Get-Content -Encoding utf8 commands.txt | ling-cli`。普通多行输入逐条请求；某行失败仍保留此前成功结果，最终退出码非零。每条输入最多 64 MiB。

## 密码与 TLS

开启服务器认证时，在客户端进程环境中设置 `LING_PASSWORD`。旧的 `LINGER_PASSWORD` 仍兼容；两者同时存在时以 `LING_PASSWORD` 为准。CLI 没有密码启动开关，避免密码出现在进程参数中。不要把真实密码写入命令历史、示例文件或共享脚本。

PowerShell 当前会话可通过 `$env:LING_PASSWORD` 设置密码，POSIX shell 可用 `export LING_PASSWORD`；使用自己的安全方式读取并赋值。服务器密码仍在其配置文件的 `[auth]` 中设置，环境变量只供 CLI 使用。

```sh
ling-cli --host 127.0.0.1 --tls-server-name cache.example.com --tls-ca certs/ca.pem -c 'PING'
ling-cli --host 127.0.0.1 --tls-server-name cache.example.com --tls-ca certs/ca.pem --tls-cert certs/client.pem --tls-key certs/client-key.pem -c 'PING'
```

省略 `--tls-ca` 时使用客户端内置公共根。服务器证书的名称必须与 `--tls-server-name` 匹配；不会跳过证书验证。服务端设置及证书路径规则见 [config.md](config.md)。

## 十种 Value 的输入

Key、字段、pattern 和选项可使用无空白裸词或双引号文本；Value 必须保留类型。下列例子可放入文本文件，交给同一个 `ling-cli` 逐行执行：

```text
SET nothing null
SET enabled true
SET counter int(-2)
SET visits uint(18446744073709551615)
SET ratio float(1.5)
SET greeting "hello 世界"
SET payload bytes(0x0001ff)
SET queue list(["first", int(2), null])
SET profile map({"name": "Lin", "visits": uint(1)})
SET tags set(["cache", uint(1), bytes(0x01)])
```

String 使用 JSON 双引号转义，但 Value 不经通用 JSON 传输。`bytes(b64"AAH/")` 与 `bytes(0x0001ff)` 等价。Int / UInt 为十进制 64 位数；Float 接受指数、`inf`、`-inf`，拒绝 NaN。Set 只允许标量成员，List / Map 可以嵌套，默认最多 8 层。空容器为 `list([])`、`map({})`、`set([])`。

任意 Value 位置的 `1` 不会自动变成 Int；应写 `int(1)`。命令固定的索引、计数、数据库、TTL 与增减步长使用十进制数字，例如 `INCRBY counter 2`、`LRANGE queue 0 -1`；`INCRFLOAT ratio float(0.5)` 的增量仍为 Float。String、Bytes、Int、UInt 和 Float 不相互隐式转换。

## 通用与迁移命令

表中方括号表示可选参数，`...` 表示可重复参数，`|` 表示择一；不要输入这些说明符号。所有命令名和选项名大小写不敏感，Key 和值大小写敏感。

| 语法 | 行为 |
|---|---|
| `SET key value [EX seconds \| PX ms \| EXAT unix_sec \| PXAT unix_ms] [NX \| XX]` | 写入任意 Value；NX 仅不存在时写入，XX 仅存在时写入；未给 TTL 会清除旧 TTL |
| `GET key` | 读取原 Value；缺失为错误 |
| `GETSET key value` | 返回旧值并替换，清除 TTL；缺失不创建 |
| `MGET key [key ...]` | 按输入顺序返回每项的值或错误 |
| `MSET key value [key value ...]` | 原子写入多个 Key，清除各自 TTL；重复 Key 非法 |
| `DEL key [key ...]` | 删除并返回实际删除数 |
| `EXISTS key [key ...]` | 返回存在的不同 Key 数量 |
| `TYPE key` | 返回类型名称；缺失为错误 |
| `RENAME key newkey` | 重命名，可覆盖目标；保留源到期期限 |
| `RENAMENX key newkey` | 仅目标不存在时重命名 |
| `COPY key dest [DB db] [REPLACE]` | 复制到当前库或指定库，REPLACE 允许覆盖；保留期限 |
| `MOVE key db` | 移到指定库；目标存在时返回 false |
| `KEYS pattern` | 返回全部匹配活跃 Key；响应过大则失败 |
| `SCAN cursor [MATCH pattern] [COUNT hint] [TYPE type]` | 按游标遍历，返回含 `cursor`、`keys` 的 Map |
| `RANDOMKEY` | 返回随机活跃 Key；空库为错误 |

`DEL` / `EXISTS` 的重复 Key 只计一次。`MGET` 保留重复输入位置；某项缺失不阻止返回其他项。多 Key 原子命令的保证来自服务端协调，不能用若干客户端请求代替。

## TTL

```text
SET session null PX 5000 NX
PTTL session
PEXPIRE session 10000
PERSIST session
```

| 语法 | 行为 |
|---|---|
| `EXPIRE key seconds` / `PEXPIRE key milliseconds` | 设置相对秒 / 毫秒期限 |
| `EXPIREAT key unix_seconds` / `PEXPIREAT key unix_milliseconds` | 设置绝对 Unix 秒 / 毫秒期限 |
| `PERSIST key` | 移除期限；没有期限或不存在时返回 false |
| `TTL key` / `PTTL key` | 返回剩余秒 / 毫秒 |
| `EXPIRETIME key` / `PEXPIRETIME key` | 返回到期 Unix 秒 / 毫秒 |

四个查询均返回 Int；`-1` 表示无期限，`-2` 表示缺失或已过期。EXPIRE 系列对存在 Key 设置已到的期限会使其立即过期，缺失返回 false。SET 的 TTL 参数必须表示将来到期，规则与 EXPIRE 系列不同。修改 List / Map / Set 等局部内容保留 TTL。逻辑到期立即不可见，不等待后台物理回收。

## 数值、String 与 Bytes

| 语法 | 行为 |
|---|---|
| `INCR key` / `DECR key` | 已有 Int / UInt 增减 1，返回原类型新值 |
| `INCRBY key n` / `DECRBY key n` | 已有 Int / UInt 按有符号步长 n 增减 |
| `INCRFLOAT key float(delta)` | 已有 Float 加指定增量 |
| `APPEND key "text"` | 已有 String 追加文本，返回新字节数 |
| `STRLEN key` | String 的 UTF-8 字节数，不是字符数 |
| `BLEN key` | Bytes 字节数 |

这些命令不自动创建缺失 Key。先 `SET counter int(0)`，再 `INCR counter`；算术越界不会修改旧值。

## List

先用 `SET queue list([])` 创建 List。负索引 `-1` 表示最后一项，区间为闭区间。

| 语法 | 行为 |
|---|---|
| `LPUSH key value [value ...]` / `RPUSH key value [value ...]` | 从头 / 尾逐项插入，返回新长度 |
| `LPOP key [count]` / `RPOP key [count]` | 从头 / 尾弹出，始终返回 List，默认 1 项 |
| `LRANGE key start end` | 读取区间，越界截到有效范围 |
| `LINDEX key index` | 读取单项；越界为错误 |
| `LSET key index value` | 替换单项；不扩容 |
| `LLEN key` | 返回长度 |
| `LREM key count value` | 按类型敏感相等性删除；正数从头，负数从尾，0 删除全部匹配 |
| `LTRIM key start end` | 仅保留区间；空结果仍是存在的空 List |

`LPUSH queue "a" "b"` 将 `"b"` 放在 `"a"` 前面。POP 的 count 为非负整数，0 返回空列表而不修改。空容器不等于缺失 Key。

## Map

先用 `SET profile map({})` 创建 Map。

| 语法 | 行为 |
|---|---|
| `FSET key field value [field value ...]` | 新增或覆盖字段，返回新增数；同请求重复字段非法 |
| `FSETNX key field value` | 仅字段不存在时写入 |
| `FGET key field [field ...]` | 始终按顺序返回逐项值或错误 |
| `FDEL key field [field ...]` | 删除字段并返回实际删除数 |
| `FEXISTS key field` | 判断字段存在，Null 字段也算存在 |
| `FKEYS key` / `FVALS key` | 返回字段名 / 值列表 |
| `FGETALL key` | 返回完整 Map |
| `FLEN key` | 返回字段数 |
| `FMERGE key map({...})` | 浅合并，返回新增字段数；同名字段整体替换 |

FGET 的字段缺失为逐项 `ERR_FIELD_NOT_FOUND`；整个 Key 缺失或类型错误使整条命令失败。FKEYS 与 FVALS 的不同调用不保证顺序对应，需要配对时用 FGETALL。

## Set

先用 `SET tags set([])` 创建 Set。成员只能是 Null、Bool、Int、UInt、Float、String 或 Bytes，比较时保留类型。

| 语法 | 行为 |
|---|---|
| `SADD key member [member ...]` / `SREM key member [member ...]` | 添加 / 删除并返回实际数量 |
| `SISMEMBER key member` | 返回是否存在 |
| `SMISMEMBER key member [member ...]` | 按输入顺序返回 Bool 列表 |
| `SMEMBERS key` | 返回 Set |
| `SCARD key` | 返回成员数 |
| `SUNION key [key ...]` | 返回并集 |
| `SINTER key [key ...]` | 返回交集 |
| `SDIFF key [key ...]` | 返回第一个集合减去其余集合的差集 |
| `SUNIONSTORE dest key [key ...]` | 原子存储并集，返回结果成员数 |
| `SINTERSTORE dest key [key ...]` | 原子存储交集，返回结果成员数 |
| `SDIFFSTORE dest key [key ...]` | 原子存储差集，返回结果成员数 |
| `SRANDMEMBER key [count]` | 返回随机成员 List；默认 1，正数不重复，负数允许重复 |
| `SPOP key [count]` | 随机删除并返回 List；默认 1，非负 count，不重复 |

集合代数的所有源必须存在且为 Set；STORE 的目标可同时是源，替换后清除目标 TTL。空结果存为空 Set。count 为 0 返回空列表；空 Set 的随机操作也返回空列表。

## 数据库、连接与运维

| 语法 | 行为 |
|---|---|
| `HELLO 1` | 协议初始化；CLI 自动执行，不应在普通会话重复调用 |
| `AUTH password` | 协议认证；CLI 建连时使用密码环境变量 |
| `SELECT db` | 切换当前连接数据库 |
| `PING [message]` | 检查连接，默认返回 `"PONG"`，可回显文本 |
| `QUIT` | 关闭当前连接 |
| `DBSIZE` | 当前库活跃 Key 数量 |
| `FLUSHDB [ASYNC]` | 清空当前库 |
| `FLUSHALL [ASYNC]` | 清空所有库 |
| `SWAPDB db1 db2` | 交换两个库的全部缓存数据与期限 |
| `INFO [section]` | 返回运行统计，section 为 server / memory / stats / clients / keyspace / events / all |
| `CONFIG GET pattern` | 查询配置，隐藏密码与密钥内容 |
| `CONFIG SET log.level level` | 动态修改日志级别 |
| `CONFIG RELOAD` | 重读启动配置；静态项变化使整次重载失败 |
| `SHUTDOWN` | 请求服务停止接入并排空 |

FLUSH 会删除相应范围内的缓存数据；ASYNC 仅延后物理释放，不延后逻辑删除，也不保证立即降低 RSS。SWAPDB 不交换事件历史或消费组。重启后缓存和 TTL 为空；Reliable Journal 只恢复已提交事件及消费元数据。

## 同一连接遍历

运行 `ling-cli` 后输入：

```text
SCAN 0 MATCH user:* COUNT 100 TYPE String
```

从响应 Map 中取 `cursor` 的 UInt 数值，在**同一会话**作为下一条 SCAN 的首参数继续，MATCH / TYPE 保持一致。返回 0 才结束，空 keys 列表不一定结束；COUNT 可调整，但只是工作量提示。不要在每页使用新的 `ling-cli -c` 进程，也不要重用已消费的游标。遍历期间变更的 Key 可能重复或缺失，多页不是全库快照。

pattern 是 UTF-8 字节匹配：`*` 为任意字节序列，`?` 为一个字节，`\` 转义后一个字节；没有正则或字符组。

## 普通 Batch

把若干普通数据命令写入 `commands.txt`：

```text
SET first null
GET first
GET missing
```

```sh
ling-cli --batch < commands.txt
```

PowerShell 使用 `Get-Content -Encoding utf8 commands.txt | ling-cli --batch`。最多收集 1000 条命令、64 MiB 原始文本；Batch 保留每项结果，某项业务错误不回滚其他项，最终退出码非零。需要多 Key 原子写入时用 MSET。

Batch 仅允许普通 Key、TTL、数值、String、Bytes、List 和 Set / Map 数据操作；不允许 HELP、KEYS、SCAN、RANDOMKEY、连接、数据库管理、运维和订阅命令，也不允许 PING / DBSIZE。含不允许的命令时整批不执行。发送后断线或响应不完整时，不能推断哪些项已提交，CLI 不自动重放。

## 订阅与确认

在第一个终端订阅，第二个终端写入：

```sh
ling-cli -c 'SUBSCRIBE order:* GROUP workers'
ling-cli -c 'SET order:1 null PX 60000'
```

订阅输出首列为 `32位小写十六进制stream_id/db/shard/sequence`，后面是类型化事件 Map。业务处理成功后，在订阅终端输入 `ACK <首列ID>`；尖括号占位符替换为实际 ID。该连接还接受 PING、UNSUBSCRIBE 或 QUIT；UNSUBSCRIBE 不删除命名组。SUBSCRIBE 通过 `-c` 启动专用订阅模式，普通逐行输入不切换为该模式。

| 语法 | 行为 |
|---|---|
| `SUBSCRIBE pattern [pattern ...] [GROUP group]` | 订阅一个或多个 pattern；省略 GROUP 为临时会话 |
| `ACK event_id` | 确认当前订阅组或临时会话已收到的事件 |
| `UNSUBSCRIBE` | 结束本次订阅，保留命名组 |

`GROUP` 作为 pattern 的字面值时须加双引号。组名为 1–128 字节 UTF-8 文本。只有显式使用 `--auto-ack` 才在输出刷新后确认，例如 `ling-cli --count 1 --auto-ack -c 'SUBSCRIBE order:* GROUP workers'`；输出成功不等于下游业务处理成功。

默认 Fast 缓冲有界，可能丢事件并产生 Offset 过期；Reliable 保证已 fsync 的事件在保留期内可恢复、可重投，可能重复。断线后重新订阅不能忽略 `ERR_OFFSET_EXPIRED` 或 `ERR_STREAM_MISMATCH`。Windows 当前不支持 Reliable；完整边界见 [events.md](events.md) 和 [persistence.md](persistence.md)。

## 输出、退出码和排错

成功值写入 stdout，错误写入 stderr。成功的 Null 输出 `null`；不存在的 Key 输出 `ERR_KEY_NOT_FOUND` 并失败。空字符串、空 Bytes 和空容器均有独立显示，64 位整数不经浮点转换。全部成功时退出码为 0；语法、连接、服务器错误或任一执行项失败时非零。帮助属于本地文本输出，不是数据库 Value。

| 现象 | 检查方式 |
|---|---|
| 连接失败 | 确认服务在运行，IP / 端口与配置一致，参见 [安装指南](install.md) 的服务状态操作 |
| `ERR_AUTH_FAILED` | 检查客户端 `LING_PASSWORD` 与服务器 `[auth]`；不要打印密码排错 |
| `ERR_TYPE_MISMATCH` | 通过 TYPE 检查现有类型，确认字面量标签 |
| `ERR_KEY_NOT_FOUND` | Key 缺失或已过期；类型专用写命令不会自动创建 Key |
| `ERR_INVALID_CURSOR` | 在同一连接重新从 SCAN 0 开始，保留筛选条件 |
| `ERR_OUT_OF_MEMORY` / `ERR_LIMIT_EXCEEDED` | 检查配置预算与 INFO；不要认为失败允许部分修改 |
| `ERR_CONFIG_INVALID` | 检查配置组合、文件路径；Windows 上不能启用 Reliable |
| `ERR_OFFSET_EXPIRED` / `ERR_STREAM_MISMATCH` | 显式处理消费位置或流身份变化，不自动跳过 |
| `ERR_COMMIT_UNKNOWN`、发送后的超时 / 断线 | 结果可能已提交；不要自动重放增减、弹出等写操作 |

完整错误注册表见 [commands.md](commands.md#错误码)。CLI 语法错误不回显原始命令，避免泄露 AUTH 输入；错误说明文字不应用于程序分支，程序应使用 SDK 的稳定错误码和结果未知标记。
