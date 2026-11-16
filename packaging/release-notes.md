ling 0.1.3 预发布版。

- 新增 `ling update`：默认更新到最新发行版（包含预发布），支持 `--check`、`--version` 和 `--stable`。按本机安装方式选择官方包，下载后验证 SHA-256；系统安装保留配置，便携更新保留用户文件。Windows 在命令退出后继续安装，通过结果文件确认完成状态。
- `HELP` 改为简短目录，支持按分组、子组和命令章节查询，例如 `HELP containers list`、`HELP SET examples`。`ling-cli help`、`ling-cli --help <主题>` 和交互式帮助使用相同内容；`--cheatsheet` 提供完整速查。
- 官网和文档以安装、使用和运维为主线，增加命令行更新说明；帮助与诊断继续支持七种语言。

[快速上手](https://921108257.github.io/ling/docs/quickstart/) · [安装与更新](https://921108257.github.io/ling/docs/install/) · [CLI 指南](https://921108257.github.io/ling/docs/cli/)

0.1.2 及更早版本请先手动安装 0.1.3，之后即可使用内置更新。升级可能重启服务，缓存值与 TTL 不恢复。Windows 当前仅支持 Fast；Linux / macOS 的 Reliable 恢复保留窗口内已提交事件和消费元数据。协议与 Journal 格式不变。

提供 Windows x86_64、Linux x86_64 / ARM64、macOS Intel / Apple Silicon 的安装包和便携包，以及覆盖全部 14 个包的 `SHA256SUMS`。安装器未签名，macOS PKG 未公证。
