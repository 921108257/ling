ling 0.1.2 预发布版本（Prerelease）。本次更新完善七语言命令行帮助与诊断，并增加官网文档和命令浏览器；仍不代表 1.0.0 稳定版验收。[官网与快速上手](https://921108257.github.io/ling/)、[CLI 指南](https://921108257.github.io/ling/docs/cli/)及[安装指南](https://921108257.github.io/ling/docs/install/)提供完整用法。

- `ling --help`、`ling-cli --help` 和命令速查按功能分组，以标题、缩进和示例展示层级。`HELP <命令>` 补充类型与边界、使用示例和相关命令；离线帮助无需连接数据库。
- 服务端和客户端统一展示稳定错误码、本地化说明、使用示例和相关命令；诊断不回显用户输入的 Key、Value、密码或原始配置内容。发送后无法确认执行结果时保留“结果未知”提示，不自动重放请求。
- 帮助和本地诊断支持简体中文、美式英语、韩语、日语、俄语、德语和法语。按显式 `--lang`、语言环境变量及本机语言选择，未匹配时回退美式英语；命令名、类型字面量和协议错误码不翻译。
- 新增官网首页、快速上手、安装与服务、CLI 和规范文档导航，以及可搜索的命令浏览器。内容从仓库规范和 CLI 命令目录构建，由 GitHub Pages 工作流检查并发布；网站文档当前为中文，七语言支持指本地命令行。

沿用 0.1.1 的命名与安装行为：服务端为 `ling`，客户端为 `ling-cli`，Rust 包为 `ling-*`；Java / Go 保留兼容名称。协议和 Journal 格式未变。CLI 优先使用 `LING_PASSWORD`，兼容 `LINGER_PASSWORD`；手动启动服务优先读取 `ling.toml`，兼容旧名 `linger.toml`。

原生安装器默认注册并启动 Windows 服务、macOS LaunchDaemon 或 Linux systemd 服务，可跳过；升级保留已有配置，卸载保留用户配置和状态。

安装包：

- Windows x86_64：安装 EXE、便携 ZIP。
- macOS Intel / Apple Silicon：PKG、便携 tar.gz。
- Linux x86_64 / ARM64：DEB、RPM、AppImage、便携 tar.gz。
- SHA256SUMS 包含全部安装包的 SHA-256 校验值。

Windows 当前仅支持 Fast 事件模式；Reliable 配置会被明确拒绝。Linux / macOS 的 Reliable 仅恢复保留窗口内已提交事件和消费元数据，缓存值与 TTL 不恢复。

便携包及 AppImage 不注册服务。Windows 安装器将 bin 加入系统 PATH。安装器未签名，macOS PKG 未公证。目标部署环境多日压力、物理断电与 Redis 8 等价比较尚未完成，不宣称已具备生产适用性。
