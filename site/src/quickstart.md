# 快速上手

从安装到存取第一条数据。这一页使用本机默认地址 `127.0.0.1:6380` 和数据库 `0`。

## 安装并连接

前往 [下载页](https://github.com/921108257/ling/releases/tag/v0.1.2)，选择与你的系统和 CPU 架构匹配的安装包。可以先用同页的 `SHA256SUMS` 校验文件。

| 系统 | 安装方式 | 服务 |
| --- | --- | --- |
| Windows x86_64 | 运行 Setup EXE | 默认注册并启动 Windows 服务 |
| macOS Intel / Apple Silicon | 运行对应架构的 PKG | 默认注册并启动 LaunchDaemon |
| Linux x86_64 / ARM64 | 使用 DEB 或 RPM | 默认注册并启动 systemd 服务 |
| 便携包 / AppImage | 解压或赋予执行权限 | 手动启动服务 |

安装器已经启动服务时，直接打开新终端连接，不必再次运行 `ling`。服务管理、跳过自动启动、升级和卸载的完整步骤见 [安装与服务](../../docs/install.md)。

如果选择便携包，在一个终端启动服务，再打开另一个终端使用客户端：

```sh
ling
```

```sh
ling-cli -c 'PING'
# "PONG"
```

`ling` 是服务端，`ling-cli` 是客户端。若提示无法找到命令，请确认已打开新终端，或使用解压目录中可执行文件的完整路径。

## 写入第一条数据

先进入交互客户端。以下命令直接在 `ling>` 提示符后输入，不受 shell 引号转义影响。

```sh
ling-cli
```

```text
SET greeting "Hello, ling"
GET greeting
SET counter int(0)
INCRBY counter 2
GET counter
```

两次 GET 分别返回 `"Hello, ling"` 和 `int(2)`。SET 成功返回 `true`。数值需要明确类型，`int(0)`、`uint(0)` 与 `float(0)` 表示不同的值，不会自动互相转换。

## 为缓存设置期限

`PX` 的单位是毫秒，`EX` 的单位是秒。下面先设置 5 秒期限，再查询剩余时间并移除期限：

```text
SET session null PX 5000 NX
PTTL session
PERSIST session
```

`NX` 表示仅在 Key 不存在时写入；`PTTL` 返回剩余毫秒。`PERSIST` 移除期限。TTL 查询返回 `int(-1)` 表示没有期限，`int(-2)` 表示 Key 不存在或已经过期。

## 使用容器

可以直接保存 List、Map、Set，再由服务器原子修改单个 Key：

```text
SET profile map({"name": "Lin", "visits": uint(1)})
FGET profile name visits
SET queue list(["first"])
RPUSH queue "second"
LRANGE queue 0 -1
SET tags set(["cache", "rust"])
SISMEMBER tags "rust"
```

String、Bytes、Int、UInt、Float 都保持自己的类型。所有十种值的语法及容器边界见 [数据模型](../../docs/data-model.md) 和 [CLI 指南](../../docs/cli.md)。

## 不离开终端查帮助

```text
HELP
HELP SET
HELP SCAN
QUIT
```

帮助在本地显示，即使服务没有启动也可使用。终端外可执行 `ling-cli --cheatsheet`；用 `ling-cli --lang zh-CN --help` 显式选择简体中文。CLI 支持七种帮助语言，未匹配的系统语言回退到美式英语。

出现错误时，先读错误码和说明，再参考随附的用法示例与相关命令。稳定错误码用于定位问题；涉及写入的超时或断线可能意味着结果未知，不应直接重试。

## 接下来做什么

- [安装与服务](../../docs/install.md)：配置路径、升级、日志和卸载。
- [CLI 指南](../../docs/cli.md)：类型语法、Batch、TLS 和订阅。
- [命令参考](../../docs/commands.md)：返回值、原子性、限制和错误码。
- [配置](../../docs/config.md)：认证、内存、淘汰、事件与指标。
- [SDK](../../docs/sdk.md)：Rust、Java 和 Go 的源码入口与连接方式。

当前版本仍为预发布版。重启后缓存值与 TTL 为空；Linux / macOS 的 Reliable 仅恢复保留窗口内已提交事件和消费元数据。Windows 当前支持 Fast 模式。
