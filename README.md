# linger-cache-db

独立部署的 Rust 内存缓存数据库规划：自定义二进制协议、10 种强类型、固定分片、多数据库、毫秒 TTL，以及可选可靠的 Key 生命周期事件。

**当前处于设计阶段，尚无可运行服务、SDK 或测试工程。** 仓库中的配置和命令是 v0.3 设计草案，不是已发布接口或启动教程。

## 从哪里开始

先读 [架构与范围](docs/architecture.md)，再按任务阅读专项规范：

| 主题 | 文档 |
|---|---|
| 值类型与命令 | [数据模型](docs/data-model.md) · [命令契约](docs/commands.md) |
| 网络与客户端 | [协议](docs/protocol.md) · [CLI 与 SDK](docs/sdk.md) |
| 生命周期与可靠性 | [TTL](docs/ttl.md) · [事件](docs/events.md) · [持久化边界](docs/persistence.md) |
| 运维与配置 | [配置规范](docs/config.md) |
| 实施与验收 | [路线图](docs/roadmap.md) · [测试与基准](docs/testing.md) |
| 设计取舍 | [决策记录](docs/decisions.md) |
| 仓库协作规则 | [AGENTS.md](AGENTS.md) |

首个稳定版聚焦单机缓存服务，不包含 Redis 兼容、Embedded API、多节点集群或缓存数据恢复。Reliable 指已落盘事件在保留期内可恢复，**不表示缓存值持久化**。

CLI 优先，并与 Rust SDK 共享实现；Java、Go 实现相同协议。性能提升是待测目标，目前不宣称优于 Redis 或生产可用。

后续代码按 [路线图](docs/roadmap.md) 逐步落地。添加实现时补充实际构建和测试命令；在工程存在前不创建占位安装说明。
