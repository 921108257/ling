# 配置规范

> 状态：v0.3 配置契约，阶段 3 实现下述六组子集。下面的长示例为首版完整候选配置，暂不能作为整体可运行文件。默认文件路径为 `linger.toml`，以 `--config <path>` 指定其他文件。

阶段 3 已实现 `server`、`auth`、`database`、`protocol`、`engine`、`eviction` 六组配置及其下列字段，其他组暂不接受，避免把未支持的能力配置为启用。无 `--config` 时读取工作目录的 `linger.toml`，文件不存在则采用这六组默认值；显式路径读取失败会退出。事件固定禁用；TLS、TTL、日志和指标配置将在对应阶段接入。配置错误不会输出原文件片段，以防泄露密码。

`engine.shard_count` 在启动时固定，0 按系统可用并行度解析，至少为 1；数据库 × 分片的索引控制块超过内部 128 MiB 预算时拒绝启动。每个 (db, shard) 独占有序索引与锁，执行线程不按数据库和分片数量相乘创建。引擎保守统计索引、Key、Value 和元数据的受管字节；`max_memory_bytes` 在全实例生效，`max_keys` 在单数据库生效。默认 noeviction；其他策略先选足够受害者，完整预检成功后才与写入一起发布，无候选或容量仍不足返回 `ERR_OUT_OF_MEMORY`。解码、重复读取响应和 KEYS 复制另有内部 128 MiB 分配预算，KEYS / SCAN 还受内部 64 MiB 结果上限约束；这些均不代表进程 RSS。

当前每连接串行处理请求，在途数实际为 1（不超过配置上限）；`pipeline_enabled` 不改变这一执行基线，连续发送的帧可在 TCP 中排队。数据命令在取得全局有界许可后进入阻塞线程池，最多 min(系统可用并行度, max_connections) 个任务；等待者总量不超过连接数。完整响应受响应上限限制，并按 `write_buffer_bytes` 分块写出；慢写入受 request 超时限制，request 超时为 0 时使用 shutdown 超时约束写入。连接数满后直接关闭新连接；半帧超时后关闭连接且无业务修改，执行许可等待超时返回 `ERR_BUSY` 且尚未执行。取得许可后不以超时假定回滚；关闭排空期限也不能强制停止已开始的阻塞命令。

## 候选默认配置

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
| 超时 | idle/request 为 0 表示不主动超时；shutdown 必须为正；均为非负整数且可安全表示为运行时 Duration |
| tls | enabled 时证书和私钥必须可读且匹配；min_version 只接受 tls1.2 / tls1.3 |
| auth | enabled 时 password 必须非空；未启用认证但提供密码也拒绝，避免配置意图被忽略 |
| database | databases 为 1..65536；default_db 小于 databases |
| engine | v1.0：shard_count 为 0 或 1..u32::MAX，0 按系统可用并行度确定；启动日志记录实际值，拓扑资源不足拒绝启动 |
| engine 限额 | max_memory_bytes / max_keys 为 0 表示不限制；其他大小项必须为正；max_nest_depth 为 1..64 |
| eviction | 仅精确小写 noeviction / lru / lfu / random / ttl-first；sample_size 为正且不超过 128 MiB / (4 × size_of(usize)) |
| ttl | precision_ms 首版固定为 1，不接受其他值；maintenance_budget 为正整数 |
| events | mode 仅 fast / reliable；保留天数、缓冲、在途窗口、组上限、租约和重投间隔均为正整数 |
| event_journal | Reliable 必须给出非空可写目录，max_bytes 至少 1 MiB；group_commit_ms 为 0..1000，0 表示不主动等待组提交 |
| protocol | 帧大小至少 18，且最多 18 + u32::MAX；批量大小为 1..u32::MAX，受请求大小共同限制 |
| log | level 为 trace/debug/info/warn/error；format 为 text/json；output 为 stdout/stderr，首版不自管文件轮转 |
| metrics | enabled 时 host 可绑定、port 为有效非零端口且不与业务监听冲突；path 为以 / 开头的固定路径 |

`events.enabled=false` 时必须使用 mode="fast" 且 Journal dir 为空，订阅返回 `ERR_EVENTS_DISABLED`。Fast 模式也要求 Journal dir 为空，不打开或清理原 Reliable 目录。Reliable 的目录独占、格式兼容与恢复失败规则见 [persistence.md](persistence.md)。

`tls.ca_file` 非空表示启用客户端证书验证，空值表示只验证服务端身份的常规 TLS。客户端仍须验证服务端证书与主机名，不能把可选认证解释为默认跳过证书校验。密码认证与 TLS 独立；远程明文链路不提供密码保密性。

配置文件中的相对证书、私钥和 Journal 路径均相对于配置文件所在目录解析；默认路径选择由入口完成后保持一致。环境变量插值、远程配置中心和命令行覆盖每个字段均不在首版范围。

## 请求、响应与解码预算

`max_request_bytes` 和 `max_response_bytes` 指整个帧 Payload，不含 18 字节头。`max_frame_bytes` 包含头，必须至少容纳 `18 + max(max_request_bytes, max_response_bytes)`，所有相加使用检查算术。

`max_value_bytes` 指单个 Value 的完整协议编码长度，包括类型、长度、字段名和子值，不包括命令封套。64 MiB 的 Value 加上命令名、Key 和选项会超过默认 64 MiB 请求上限；配置的 Value 上限不表示任意封装都一定可发送。超出任何一个限制即拒绝，不截断。

深度限制按每个实际存储 Value 的根计算，不把请求参数 List 或逐项结果 Map 当作用户容器层级。协议解码器另限制封套深度为 max_nest_depth + 4，并对字节、元素数量和分配总量做预算；不能只检查递归层数。

`write_buffer_bytes` 是单连接待发送字节高水位，不是最大单帧：大帧可经受限分块写出，不必在该缓冲中一次完整复制。慢连接达到水位后暂停向该连接交付，不能持续生成无界任务。服务端还须校验命令结果能否完整编码后再发布写入，例如 POP 不得先删除再发现响应超限。

`max_inflight_per_connection` 限制已接收但未完成的请求数；pipeline_enabled=false 时每连接只允许一个普通请求在途。批量中的子请求也占调度预算，不能借一个 Batch 帧绕过全服务有界任务队列。

## 内存和 Key 配额

`engine.max_memory_bytes` 是全实例缓存索引、Key、Value 和条目元数据的受管内存预算；不是每数据库配额，不包括网络缓冲、事件缓冲、Journal 页缓存、分配器碎片及程序本身。必须分别报告受管字节与进程 RSS。

`engine.max_keys` 是单数据库活跃 Key 上限。到期条目不算活跃 Key，但待记账条目仍占受管内存，不能通过逻辑过期绕开内存预算。新写入预留容量，不能只在分配后检查。

FLUSH ASYNC 退役索引在物理释放后才归还受管预算；清空后立即写入仍可能遇到容量不足。一个懒启动回收线程配合容量为 1 的队列，满时同步释放；分配器是否把空闲页归还系统不影响逻辑计费，RSS 可能继续保留。

| 淘汰策略 | 范围与选择 |
|---|---|
| noeviction | 不主动淘汰活跃 Key，超过预算拒绝扩大内存的写入 |
| lru | 全实例候选中采样近似最近最少访问 Key，不承诺精确全局 LRU |
| lfu | 采样近似低频 Key；计数饱和与衰减需要确定且可测试，不影响数据语义 |
| random | 随机选择活跃 Key，不要求密码学随机 |
| ttl-first | 优先最早到期的有 TTL 活跃 Key；无候选时按 noeviction 拒绝 |

内存预算触发时可跨数据库淘汰；max_keys 触发时仅从对应数据库选择候选。所有候选在正式提交前重新验证，淘汰与触发写入整体预留/提交，失败不提前删除其他 Key。Reliable 下淘汰也必须完成 Journal 提交。

受害 Key 所在 Shard 必须加入本次完整执行权集合。先选择候选，再按全局顺序取得目标与受害 Shard 并重验；候选改变时释放后重新取得完整集合，不能持有目标锁再逆序追加受害锁。

阶段 3 先采用保守方案：启用淘汰且存在内存或 Key 上限时，写入在选候选前直接取得全实例执行权，不执行逆序追加。noeviction 下带每库 Key 配额的写入取得全库执行权；普通单 Key 无配额写入只锁目标分片。后续可依据基准优化锁范围，但不降低原子性。LRU / LFU 从活跃候选均匀采样；LFU 为饱和计数、每单调分钟右移一位衰减。普通条目读取（含元数据与条件检查）更新热度，KEYS / SCAN 不加热，热度不改变 revision。ttl-first 选择全部有期限候选中最早的一项；公开 TTL 命令尚未实现，因此当前外部写入无期限候选时按 noeviction 拒绝。

## TTL 与事件容量

precision_ms 表示逻辑比较精度，不是事件投递延迟承诺。maintenance_budget 是每次后台维护最多处理的任务数；读路径始终独立检查到期。

时间轮大小、层数和 tick 是内部算法参数，尚无基准证明需要，首版不作为公开配置。必须覆盖全部合法期限，不能因为时间轮跨度不足静默丢失远期任务。

Fast 缓冲满时丢弃最早的未交付事件并计数，不阻止正常缓存写入；未 ACK 交付也可能因容量或保留超期变成恢复错误，不提供可靠保证。Fast 不运行磁盘 Journal；命名组也只在本次进程内存在。

Reliable 以磁盘 Journal 为事件源，buffer_bytes 限制推送缓存与事件投递元数据，缓冲满先暂停交付。max_pending_events 为单组所有消费者/逻辑流合计的未消除交付窗口，包含未 ACK 及被前序空洞阻挡的乱序 ACK；max_groups 为实例命名组数上限，防止没有删除接口时元数据无界增长。Fast 的同类元数据也计入 buffer_bytes。

Journal 容量包含元数据、事务、检查点和轮转预留。预算不足拒绝事件写入，不能缩短保留时间或自动降级 Fast。ACK / 检查点需要保留独立的提交空间。保留期不是消费者无限落后保证。

## 运行时变更

首版唯一动态配置为 `log.level`。CONFIG SET 使用其实际 String 值；其他项返回 `ERR_CONFIG_IMMUTABLE`。修改只影响当前进程，不回写文件。

CONFIG RELOAD 重新读取当前配置文件，先全部校验，再比较静态项；任何静态项变化则整体拒绝，原配置继续生效。仅 log.level 发生变化才应用，无变化也成功。不能部分应用后再报错。

CONFIG GET 返回标准字段名与当前有效值，包括自动解析后的实际 shard_count。密码固定显示 `<redacted>`，不返回私钥内容；证书和密钥文件路径可显示。INFO / 日志也遵守脱敏要求。

数据库数量、Shard 数量、认证、TLS、限额、保留策略和事件模式均需重启。重启不代表自动恢复缓存数据。此收紧替代 v0.2 中尚未定义安全切换流程的热更新列表。

重启修改数据库数或实际 Shard 数不能复用旧 Reliable Journal；旧目录绑定原拓扑，需新目录与新 stream_id。详见持久化规范。编码允许的大上限不代表可突破机器内存，解析与资源预检失败应明确拒绝启动。
