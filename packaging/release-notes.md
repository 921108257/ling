ling 0.1.1 预发布版本（Prerelease）。项目由 linger-cache-db 正式更名为 ling，本次更新聚焦命令行使用与安装体验，仍不代表 1.0.0 稳定版验收。完整用法见仓库 README、docs/cli.md 和 docs/install.md。

- 服务端命令改为 `ling`，客户端改为 `ling-cli`，Rust 包统一为 `ling-*`。协议和 Journal 格式不变；Java / Go 的兼容名称保留。
- `ling --help` 与 `ling-cli --help` 按本机语言自动选择简体中文、美式英语、韩语、日语、俄语、德语或法语，未支持的语言回退美式英语；可用 `--lang` 显式选择。
- 新增离线 `--cheatsheet`、`HELP` 与 `HELP <命令>`，补齐完整 CLI 指南。帮助无需连接数据库，普通交互会提示速查入口。
- 原生安装器默认注册并启动 Windows 服务、macOS LaunchDaemon 或 Linux systemd 服务，可跳过；升级保留已有配置，卸载保留用户配置和状态。
- CLI 优先使用 `LING_PASSWORD`，兼容 `LINGER_PASSWORD`；手动启动服务优先读取 `ling.toml`，兼容旧名 `linger.toml`。

安装包：

- Windows x86_64：安装 EXE、便携 ZIP。
- macOS Intel / Apple Silicon：PKG、便携 tar.gz。
- Linux x86_64 / ARM64：DEB、RPM、AppImage、便携 tar.gz。
- SHA256SUMS 包含全部安装包的 SHA-256 校验值。

Windows 当前仅支持 Fast 事件模式；Reliable 配置会被明确拒绝。Linux / macOS 的 Reliable 仅恢复保留窗口内已提交事件和消费元数据，缓存值与 TTL 不恢复。

便携包及 AppImage 不注册服务。Windows 安装器将 bin 加入系统 PATH。安装器未签名，macOS PKG 未公证。目标部署环境多日压力、物理断电与 Redis 8 等价比较尚未完成，不宣称已具备生产适用性。
