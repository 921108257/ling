# 配置规范

默认配置文件为 `ling.toml`，使用 `ling --config <path>` 指定其他文件。下面的默认配置可整体使用。

配置包含 `server`、`tls`、`auth`、`database`、`engine`、`eviction`、`ttl`、`events`、`event_journal`、`protocol`、`log`、`metrics` 十二组。无 `--config` 时优先读取工作目录的 `ling.toml`，没有时兼容旧名 `linger.toml`，均不存在则采用默认值；显式路径读取失败会退出。默认事件 Fast；Reliable 必须配置目录。配置错误不会输出原文件片段，以防泄露密码。

Windows 当前仅支持 Fast 模式；设置 `events.mode="reliable"` 会在打开 Journal 或监听端口前返回 `ERR_CONFIG_INVALID`，提示该平台尚不支持 Reliable。不会自动改用 Fast，也不会为此创建 Journal 目录。Linux / macOS 可按 [persistence.md](persistence.md) 的文件系统要求启用 Reliable。

`engine.shard_count` 在启动时固定，0 按系统可用并行度解析，至少为 1；数据库 × 分片的索引控制块超过内部 128 MiB 预算时拒绝启动。每个 (db, shard) 独占有序索引与锁，执行线程不按数据库和分片数量相乘创建。引擎保守统计索引、Key、Value 和元数据的受管字节；`max_memory_bytes` 在全实例生效，`max_keys` 在单数据库生效。默认 noeviction；其他策略先选足够受害者，完整预检成功后才与写入一起发布，无候选或容量仍不足返回 `ERR_OUT_OF_MEMORY`。解码、重复读取响应和 KEYS 复制另有内部 128 MiB 分配预算，KEYS / SCAN 还受内部 64 MiB 结果上限约束；这些均不代表进程 RSS。

当前每连接串行处理请求，在途数实际为 1（不超过配置上限）；`pipeline_enabled` 不改变这一执行基线，连续发送的帧可在 TCP 中排队。数据命令在取得全局有界许可后进入阻塞线程池，最多 min(系统可用并行度, max_connections) 个任务；请求等待者总量不超过连接数，另有最多一个后台维护等待者。完整响应受响应上限限制，并按 `write_buffer_bytes` 分块写出；慢写入受 request 超时限制，request 超时为 0 时使用 shutdown 超时约束写入。连接数满后直接关闭新连接；半帧超时后关闭连接且无业务修改，执行许可等待超时返回 `ERR_BUSY` 且尚未执行。取得许可后不以超时假定回滚；关闭排空期限也不能强制停止已开始的阻塞命令。

## 认证与暴露面

认证是实例级权限；已认证连接可执行 `SHUTDOWN`、`CONFIG SET` 与 `FLUSHALL`，数据库编号不是授权边界，见[命令规范](commands.md)。默认保持回环监听；远程接入应同时启用认证和 TLS，并通过防火墙只放行业务客户端。Prometheus 是独立明文端点，建议保留回环地址，通过受控采集链路访问。

AUTH 对输入与配置口令分别计算 SHA-256，再使用 `subtle` 常量时间比较两个固定长度摘要；不按原口令长度分支，也不逐字符提前返回。这不改变配置中的口令存储方式，摘要计算本身仍需处理完整输入。失败重新认证立即撤销已有认证状态。

连接内连续错误口令的应答延迟依次为 100、200、400、800、1600 ms；实际延迟不超过启用的 request / idle 超时中较短者的一半。延迟是异步等待，不持有引擎执行许可。第五次连续失败仍返回原 `ERR_AUTH_FAILED` 响应，随后关闭该连接；成功认证清零次数。次数仅随连接保存，不建立源地址表，重新连接会重新计数。实例的失败告警最多每 60 秒一次，不记录口令或源地址，不增加 AUTH 专属 INFO / Prometheus 字段。

启动时对「业务监听非回环且未认证」「启用认证但未启用 TLS」「Prometheus 监听非回环」分别告警；这些告警不会拒绝合法配置或改变默认设置。

## 默认配置

```toml
[server]
host = "127.0.0.1"
port = 6380
backlog = 1024
max_connections = 10000
idle_timeout_ms = 300000
request_timeout_ms = 30000
max_request_bytes = 67108864
max_response_bytes = 67108864
shutdown_timeout_ms = 5000
max_inflight_per_connection = 1024
write_buffer_bytes = 8388608

[tls]
enabled = false
cert_file = ""
key_file = ""
ca_file = ""
min_version = "tls1.2"

[auth]
enabled = false
password = ""

[database]
databases = 16
default_db = 0

[engine]
shard_count = 0
max_memory_bytes = 0
max_keys = 0
max_key_bytes = 1024
max_value_bytes = 67108864
max_nest_depth = 8

[eviction]
policy = "noeviction"
sample_size = 5

[ttl]
precision_ms = 1
maintenance_budget = 1024

[events]
enabled = true
mode = "fast"
retention_days = 7
buffer_bytes = 67108864
max_pending_events = 10000
max_groups = 1024
consumer_timeout_ms = 30000
retry_interval_ms = 5000

[event_journal]
dir = ""
max_bytes = 1073741824
group_commit_ms = 2

[protocol]
max_frame_bytes = 134217728
max_batch_size = 1000
pipeline_enabled = true

[log]
level = "info"
format = "text"
output = "stdout"

[metrics]
enabled = false
host = "127.0.0.1"
port = 9090
path = "/metrics"
```

所有 `bytes` 计字节，时间项按后缀计量，`retention_days` 每天按 86,400 秒计算。示例数字分别使用二进制 MiB / GiB 换算，不把 MiB 写成十进制 MB。

## 字段语义与校验

解析器拒绝未知字段、错误类型、重复字段、范围越界和无效组合，不能静默忽略拼写错误。先完成配置与资源校验，再监听业务端口。未指定字段使用上面的默认值。

| 配置组 | 规则 |
|---|---|
| server | host 是可绑定 IP；port 为 1..65535；backlog、连接数、在途数、缓冲字节必须为正；请求/响应 Payload 上限至少 1024 字节 |
| 超时 | HELLO 首帧从等待首字节起共用 request 期限，为 0 时沿用 idle；后续 idle/request 为 0 分别禁用帧间/半帧读超时；shutdown 必须为正；均为非负整数且可安全表示为运行时 Duration |
| tls | enabled 时证书和私钥必须可读且匹配；min_version 只接受 tls1.2 / tls1.3 |
| auth | enabled 时 password 必须非空；未启用认证但提供密码也拒绝，避免配置意图被忽略 |
| database | databases 为 1..65536；default_db 小于 databases |
| engine | shard_count 为 0 或 1..u32::MAX，0 按系统可用并行度确定；启动日志记录实际值，拓扑资源不足拒绝启动 |
| engine 限额 | max_memory_bytes / max_keys 为 0 表示不限制；其他大小项必须为正；max_nest_depth 为 1..64 |
| eviction | 仅精确小写 noeviction / lru / lfu / random / ttl-first；sample_size 为正且不超过 128 MiB / (4 × size_of(usize)) |
| ttl | precision_ms 当前版本固定为 1，不接受其他值；maintenance_budget 为正整数 |
| events | mode 仅 fast / reliable；保留天数、缓冲、在途窗口、组上限、租约和重投间隔均为正整数 |
| event_journal | Reliable 必须给出非空可写目录，max_bytes 至少 1 MiB；group_commit_ms 为 0..1000 的最大主动等待预算；当前逐事务同步，不主动等待 |
| protocol | 帧大小至少 18，且最多 18 + u32::MAX；批量大小为 1..u32::MAX，受请求大小共同限制 |
| log | level 为 trace/debug/info/warn/error；format 为 text/json；output 为 stdout/stderr，当前版本不自管文件轮转 |
| metrics | enabled 时 host 可绑定、port 为有效非零端口且不与业务监听冲突；path 为以 / 开头的固定路径 |

`events.enabled=false` 时必须使用 mode="fast" 且 Journal dir 为空，订阅返回 `ERR_EVENTS_DISABLED`。Fast 模式也要求 Journal dir 为空，不打开或清理原 Reliable 目录。Reliable 的目录独占、格式兼容与恢复失败规则见 [persistence.md](persistence.md)。

`tls.ca_file` 非空表示启用客户端证书验证，空值表示只验证服务端身份的常规 TLS。客户端仍须验证服务端证书与主机名，不能把可选认证解释为默认跳过证书校验。密码认证与 TLS 独立；远程明文链路不提供密码保密性。

TLS 使用 rustls；开启后该端口只接受 TLS。证书与私钥在绑定业务端口前载入校验；握手占用连接许可，使用 `server.shutdown_timeout_ms` 作为独立期限，并响应关闭信号。

日志使用配置的最低级别、text / JSON 格式及 stdout / stderr 输出；启动信息包含实际分片数、监听地址与事件模式，维护错误只记录稳定错误码，不输出请求参数。日志级别改变不增加当前未实现的逐命令追踪能力。

Prometheus 是独立的明文 HTTP GET 端点，默认关闭。最多 16 个并发请求，头部最多 4096 字节、单次处理期限 5 秒；仅匹配 `metrics.path`。采样可能等待引擎执行权，超时会关闭连接。输出请求 / 错误 / 连接计数、请求时延固定桶、受管内存、TTL 队列、事件分类与聚合、Journal 字节与错误，不包含密码、Key、Value 或组名标签。此端点不承载业务协议的 AUTH / TLS，按配置监听地址部署。

配置文件中的相对证书、私钥和 Journal 路径均相对于配置文件所在目录解析；默认路径选择由入口完成后保持一致。环境变量插值、远程配置中心和命令行覆盖每个字段均不在当前版本范围。

## 请求、响应与解码预算

`max_request_bytes` 和 `max_response_bytes` 指整个帧 Payload，不含 18 字节头。`max_frame_bytes` 包含头，必须至少容纳 `18 + max(max_request_bytes, max_response_bytes)`，所有相加使用检查算术。

`max_value_bytes` 指单个 Value 的完整协议编码长度，包括类型、长度、字段名和子值，不包括命令封套。64 MiB 的 Value 加上命令名、Key 和选项会超过默认 64 MiB 请求上限；配置的 Value 上限不表示任意封装都一定可发送。超出任何一个限制即拒绝，不截断。

深度限制按每个实际存储 Value 的根计算，不把请求参数 List 或逐项结果 Map 当作用户容器层级。协议解码器另限制封套深度为 max_nest_depth + 4，并对字节、元素数量和分配总量做预算；不能只检查递归层数。

`write_buffer_bytes` 是单连接待发送字节高水位，不是最大单帧：大帧可经受限分块写出，不必在该缓冲中一次完整复制。慢连接达到水位后暂停向该连接交付，不能持续生成无界任务。服务端还须校验命令结果能否完整编码后再发布写入，例如 POP 不得先删除再发现响应超限。

`max_inflight_per_connection` 限制已接收但未完成的请求数；pipeline_enabled=false 时每连接只允许一个普通请求在途。批量中的子请求也占调度预算，不能借一个 Batch 帧绕过全服务有界任务队列。

## 内存和 Key 配额

`engine.max_memory_bytes` 是全实例缓存索引、Key、Value、条目元数据及到期索引预留空间的受管内存预算；不是每数据库配额，不包括网络缓冲、事件缓冲、Journal 页缓存、分配器碎片及程序本身。必须分别报告受管字节与进程 RSS。每个条目预留一个定时项及其树节点的保守空间，即使尚无 TTL；设置或移除 TTL 不改变该预留计费。

`engine.max_keys` 是单数据库活跃 Key 上限。到期条目不算活跃 Key，但待记账条目仍占受管内存，不能通过逻辑过期绕开内存预算。新写入预留容量，不能只在分配后检查。

FLUSH ASYNC 退役索引在物理释放后才归还受管预算；清空后立即写入仍可能遇到容量不足。一个懒启动回收线程配合容量为 1 的队列，满时同步释放；分配器是否把空闲页归还系统不影响逻辑计费，RSS 可能继续保留。

| 淘汰策略 | 范围与选择 |
|---|---|
| noeviction | 不主动淘汰活跃 Key，超过预算拒绝扩大内存的写入 |
| lru | 全实例候选中采样近似最近最少访问 Key，不承诺精确全局 LRU |
| lfu | 采样近似低频 Key；计数使用饱和与衰减，不影响数据语义 |
| random | 随机选择活跃 Key，不要求密码学随机 |
| ttl-first | 优先最早到期的有 TTL 活跃 Key；无候选时按 noeviction 拒绝 |

内存预算触发时可跨数据库淘汰；max_keys 触发时仅从对应数据库选择候选。所有候选在正式提交前重新验证，淘汰与触发写入整体预留/提交，失败不提前删除其他 Key。Reliable 下淘汰也必须完成 Journal 提交。

受害 Key 所在 Shard 必须加入本次完整执行权集合。先选择候选，再按全局顺序取得目标与受害 Shard 并重验；候选改变时释放后重新取得完整集合，不能持有目标锁再逆序追加受害锁。

当前采用保守方案：启用淘汰且存在内存或 Key 上限时，写入在选候选前直接取得全实例执行权，不执行逆序追加。noeviction 下带每库 Key 配额的写入取得全库执行权；普通单 Key 无配额写入只锁目标分片。LRU / LFU 从活跃候选均匀采样；LFU 为饱和计数、每单调分钟右移一位衰减。普通条目读取（含元数据与条件检查）更新热度，KEYS / SCAN 不加热，热度不改变 revision。ttl-first 选择全部有期限候选中最早的一项；期限由已开放的 TTL 命令设置，无期限候选时按 noeviction 拒绝。

## TTL 与事件容量

precision_ms 固定为 1；它表示逻辑比较精度，不是回收或事件投递延迟承诺。maintenance_budget 默认为 1024 且必须为正，是每轮后台维护最多检查的候选数；读路径始终独立检查到期。一个后台循环每轮先等待 1 毫秒，再取得与请求共享的有界阻塞执行许可；批次完成后才派发下一轮。关闭时停止派发，已经开始的阻塞维护不能被强制回滚。

后台调度算法不提供时间轮大小、层数或 tick 配置。合法期限均遵循相同的逻辑到期规则。

Fast 队列占 buffer_bytes 至多一半，其余预留消费元数据；队列满时丢弃最早事件并计数，不阻止正常缓存写入；未 ACK 交付也可能因容量或保留超期变成恢复错误，不提供可靠保证。Fast 不运行磁盘 Journal；命名组也只在本次进程内存在。

Reliable 以磁盘 Journal 为事件源，buffer_bytes 限制推送缓存与事件投递元数据，缓冲满先暂停交付。max_pending_events 为单组所有消费者/逻辑流合计的未消除交付窗口，包含未 ACK 及被前序空洞阻挡的乱序 ACK；max_groups 为实例命名组及临时会话合计上限，防止没有删除接口时元数据无界增长。Fast 的同类元数据也计入 buffer_bytes。

当前 buffer_bytes 范围为 4 KiB..1 GiB，max_pending_events ≤ buffer_bytes / 256，max_groups ≤ buffer_bytes / 512；真实可建立的窗口和组还受合计内存及 Journal 控制预留约束。元数据不足暂停新交付，仍允许 ACK。事件启用时 64 × databases × shards ≤ buffer_bytes / 4；max_key_bytes + 768 ≤ max_response_bytes，且 2 × max_key_bytes + 256 ≤ buffer_bytes / 4，确保单事件可投递。事件候选每事务保守上限 buffer_bytes / 4；大型 MSET / FLUSH / SWAPDB 在复制事件 Key 前预检，超限返回 ERR_LIMIT_EXCEEDED 并保留状态。逻辑预算不等于 RSS。

Journal 容量包含元数据、事务、检查点和轮转预留。预算不足拒绝事件写入，不能缩短保留时间或自动降级 Fast。ACK / 检查点需要保留独立的提交空间。保留期不是消费者无限落后保证。

## 运行时变更

当前版本唯一动态配置为 `log.level`。CONFIG SET 使用其实际 String 值；其他项返回 `ERR_CONFIG_IMMUTABLE`。修改只影响当前进程，不回写文件。

CONFIG RELOAD 重新读取当前配置文件，先全部校验，再比较静态项；任何静态项变化则整体拒绝，原配置继续生效。仅 log.level 发生变化才应用，无变化也成功。不能部分应用后再报错。

启动加载时固定配置来源的绝对路径，相对资源路径按其父目录解析；未加载文件的纯默认启动没有 RELOAD 来源，返回 `ERR_CONFIG_INVALID`。静态比较使用真实凭据，不能因 GET 脱敏而漏过密码变化。

CONFIG GET 返回标准字段名与当前有效值，包括自动解析后的实际 shard_count。密码固定显示 `<redacted>`，不返回私钥内容；证书和密钥文件路径可显示。INFO / 日志也遵守脱敏要求。

数据库数量、Shard 数量、认证、TLS、限额、保留策略和事件模式均需重启。重启不代表自动恢复缓存数据。

重启修改数据库数或实际 Shard 数不能复用旧 Reliable Journal；旧目录绑定原拓扑，需新目录与新 stream_id。详见持久化规范。编码允许的大上限不代表可突破机器内存，解析与资源预检失败应明确拒绝启动。
