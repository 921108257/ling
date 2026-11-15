0.1.0 预览版本：独立 Rust 缓存服务端与 CLI。下载与安装说明见仓库 README。

- Windows x86_64：安装 EXE、便携 ZIP。
- macOS Intel / Apple Silicon：PKG、便携 tar.gz。
- Linux x86_64 / ARM64：DEB、RPM、AppImage、便携 tar.gz。
- SHA256SUMS 包含全部安装包的 SHA-256 校验值。

Windows 当前仅支持 Fast 事件模式；Reliable 配置会被明确拒绝。Linux / macOS 的 Reliable 仅恢复保留窗口内已提交事件和消费元数据，缓存值与 TTL 不恢复。

安装器未签名，macOS PKG 未公证。服务以前台进程运行，安装不会自动启动或注册系统服务。
