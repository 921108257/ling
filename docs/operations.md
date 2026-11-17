# ling 排错手册

适用基线：0.1.3 预发布。以下是当前能做的检查，不是后续版本的修复承诺。示例使用本机默认地址；实际排查须指定与应用相同的实例、数据库、认证和 TLS 设置。先记录两个程序的版本，再决定是否需要改配置或停机。

```sh
ling --version
ling-cli --version
ling-cli -c 'PING'
ling-cli -c 'INFO events'
```

`PING` 成功只证明这条连接可用。`INFO events` 的 `committed`、`dropped`、`redelivered` 是实例统计，不等于某组已收到或确认；`consumer_groups` 才有各组的 `pending` 和分片 `watermarks`。在故障前后各取一次，比只看单个数值更有用。共享日志时去除密码、令牌、私钥、业务 Key/Value。

## 消费者没有收到事件，或收到重复事件

| 观测 | 怎么区分 | 下一步 |
|---|---|---|
| 建组成功，首条迟迟不到 | 核对 SELECT 的库、pattern、建组时点。新组从当时流尾开始，不补发建组前的事件。已有匹配提交且组的 `pending=0` 时，不能据此认定没有事件：小 `events.buffer_bytes` 可能容得下空组，却容不下第一笔交付记账（R-14） | 在隔离实例先建组，再产生一个已知匹配的事件；对照同组窗口和较宽预算，不要重建生产组。修改事件预算须按[配置](config.md)安排重启；Fast 组不跨进程，缓存重启后为空。 |
| 已有 pending，但新交付停滞 | 同组所有消费者共享窗口；后序 ACK 可被早先的空洞挡住。Fast 的 `dropped` 增长与 Reliable 的磁盘容量不足是两件事 | 查最早未确认的业务处理是否完成，只对确实处理完成且交付给该组的 ID 做 ACK。不要跳水位或确认猜出的 ID。 |
| CLI 已打印事件、退出却又收到同一 ID | 打印不是 ACK。0.1.3 显式 ACK 模式下 stdin EOF 可能以成功状态结束且未确认（C-10）；ACK 成功前断线也可重投 | 保留可读 stdin 并显式确认；下游按完整 event_id 幂等处理。`--auto-ack` 在输出刷新后即确认，只适用于把「成功输出」视为处理完成的场景。 |
| 订阅结果与 SCAN MATCH 不同 | 0.1.3 已观察到重叠星号模式在订阅与遍历结果不一致（R-10）；先确认同库、同字面 pattern、同一个 Key | 在隔离库用不敏感示例比较；修复发布前优先用精确 Key 或已验证的简单 pattern，不用订阅过滤代替访问控制。 |
| `ERR_OFFSET_EXPIRED` / `ERR_STREAM_MISMATCH` | 前者表示需要的历史不再可用；后者表示流身份变了。二者都不是「没消息」 | 停止自动重连循环，核对实例、旧 stream_id、组及业务检查点。由业务明确决定补偿或从新位置开始；不要自动换组名、跳到流尾或删除 Journal。 |

## 写入超时、日志满或恢复失败

- **发送后断线/超时，或 `ERR_COMMIT_UNKNOWN`：** 结果未知。保存时间、命令类别、数据库和业务侧去重标识；重新建连，只读核对业务状态或已知事件。一次 GET 的当前值不能证明那次 INCR、弹出操作或 Batch 的每一项都执行过或没执行。没有足够证据就保留未知状态，不整批重放。协议的 Request ID 不是幂等键。
- **`ERR_JOURNAL_FULL`：** 先看 `INFO events` 的 `journal_bytes`、最老保留时间、组状态，再看磁盘可用空间与配置的 `max_bytes`。清理受保留期约束；ACK 并不保证立即释放文件空间。不要为恢复写入而删日志文件或切换到 Fast。
- **`ERR_JOURNAL_UNAVAILABLE` 或启动报告损坏：** 先确认平台、实际目录、服务用户、磁盘状态与独占使用情况。怀疑记录损坏时停止新的读写/自动重启，按运维流程停服后保留**整个目录的副本**、错误位置和版本。0.1.3 的运行期读取与启动恢复共用可修尾的遍历路径（R-09）；不要反复用唯一原件试启动或手工截断。只在副本上分析，既不能保证挽回所有事件，也不能用 Journal 恢复缓存。见[持久化](persistence.md)。

## TTL 看似没变，修订号却在增加

在同一数据库对照 `PEXPIRETIME key`（Unix 毫秒）和 `PTTL key`（剩余毫秒；-1 无期限、-2 不存在）。不要用两次网络查询之间的自然耗时代替单调时钟证据。若业务反复给同一 Key 设置同一绝对毫秒、期限看起来没变却出现额外 TTL 变更，记录事件的 generation、revision 和两个期限。R-13 已在注入时钟下确认修订号会额外增加；0.1.3 排查时不要只凭 revision 推断业务值被改写。见[TTL](ttl.md)。

## 启动或升级后服务不可用

| 场景 | 先查什么 | 如何处置 |
|---|---|---|
| Linux 的 Reliable 前台可用，装成服务后起不来 | `systemctl status ling`、`journalctl -u ling -n 100 --no-pager`、`systemctl cat ling`；Journal 相对路径是相对于**配置文件目录**，不是 WorkingDirectory。随包单元只放行 `/var/lib/ling` | 使用[随包示例](../packaging/linux/ling.toml)里的绝对路径 `/var/lib/ling/journal` 并确认服务用户可写；不要放宽整个系统沙箱。若已有历史目录，不要直接指向一个空目录冒充恢复。 |
| Windows 启用 Reliable 得到 `ERR_CONFIG_INVALID` | 0.1.3 在监听前明确拒绝此组合，不是创建目录或改 ACL 就能修好 | 需要持久事件时选择满足同步要求的 Linux/macOS 环境；只有业务能接受不持久化时才显式选 Fast。 |
| 更新没有稳定候选，或者 Windows 助手返回后版本没变 | `ling update --stable --check` 只找正式版；只有预发布时无候选并不表示下载失败。Windows 看输出路径下的 `result.json`：`scheduled` 不等于 `success` | 需要预览版时显式用 `ling update --check` 比较；`unknown` 或中断时先保留结果文件、备份和安装日志，比对 `ling --version`、`ling-cli --version`。版本不一致时暂停使用，按[安装指南](install.md)恢复同一套程序并保留配置/状态，不要盲目重复更新。 |

对照依据：[命令与错误码](commands.md)、[事件与保留](events.md)、[客户端与结果未知](sdk.md)。版本工作包及证据编号见[基线审查](version/baseline-0.1.3.md)。
