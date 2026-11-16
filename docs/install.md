# 安装、服务与升级

ling 当前为 0.1.0 预发布版本。新产物使用 `ling` 名称；旧 Release 不会被本次源码修改自动更新。下载 [Releases](https://github.com/921108257/ling/releases) 中与系统、CPU 架构匹配的包，并对照同页 `SHA256SUMS` 校验。Windows 使用 `Get-FileHash <文件> -Algorithm SHA256`，Linux 使用 `sha256sum`，macOS 使用 `shasum -a 256`。

安装器默认注册并启动后台服务，默认监听 `127.0.0.1:6380`，事件模式为 Fast。服务使用专用工作目录，不读取用户终端当前目录的配置。需要自定义配置或已有实例占用端口时，先跳过自动启动，修改配置后再启动。Reliable 限制见 [architecture.md](architecture.md) 和 [persistence.md](persistence.md)。

## Windows x86_64

运行 `ling_<版本>_windows_x86_64_setup.exe`，需要管理员权限。默认安装到 `%ProgramFiles%\ling`，将其 `bin` 加入系统 PATH；新开终端后可直接使用 `ling` 和 `ling-cli`。安装向导默认勾选后台服务任务，取消可只安装程序；无人值守安装使用 `/TASKS=""` 跳过服务任务。只安装程序后需要服务时，修改工作配置，再重新运行安装器并勾选后台服务任务（无人值守使用 `/TASKS="service"`）；已有配置会保留。

服务名为 `ling`，通过随包固定版本 WinSW 托管控制台程序：

| 内容 | 路径 |
|---|---|
| 服务端、CLI | `%ProgramFiles%\ling\bin\ling.exe`、`ling-cli.exe` |
| 工作配置 | `%ProgramData%\ling\ling.toml` |
| 工作目录 | `%ProgramData%\ling\state` |
| 服务日志 | `%ProgramData%\ling\logs` |

在管理员 PowerShell 中管理服务：

```powershell
Get-Service ling
Stop-Service ling
Start-Service ling
Restart-Service ling
ling-cli -c 'PING'
```

只有日志级别支持运行时修改，其他配置需要重启。Windows 目前明确拒绝 Reliable 模式，不会静默降级。

升级前停止服务并备份工作配置和需要保留的 Journal。运行新版本安装器，使用同一安装目录；程序文件更新，已有工作配置保留。通过系统“已安装的应用”卸载会先停用、注销服务，再移除程序和安装器添加的 PATH 项；配置、状态和日志保留，确认不再需要后自行删除。旧版 `Linger Cache DB` 安装不会自动迁移成新服务：先退出旧进程，将配置合并到新的工作配置，再安装和验证。

ZIP 是便携包：解压后使用 `bin\ling.exe --config <绝对路径>` 和 `bin\ling-cli.exe`。它不会修改 PATH 或自动注册服务；需要自动注册时使用 Setup。安装器目前未签名。

## Linux x86_64 / ARM64

DEB 用于 Debian / Ubuntu，RPM 用于支持 RPM 的发行版。默认安装程序到 `/usr/bin/ling` 和 `/usr/bin/ling-cli`，注册 systemd 单元 `ling.service`，以 `ling` 系统用户运行。

```sh
sudo dpkg -i ./ling_<版本>_linux_<架构>.deb
# RPM 系发行版选择其一
sudo rpm -Uvh ./ling_<版本>_linux_<架构>.rpm
systemctl status ling
ling-cli -c 'PING'
```

工作配置在 `/etc/ling/ling.toml`，工作目录在 `/var/lib/ling`。日志进入 systemd Journal：

```sh
sudo systemctl stop ling
sudo systemctl start ling
sudo systemctl restart ling
journalctl -u ling -n 100 --no-pager
```

安装前设置 `LING_SKIP_SERVICE=1` 可跳过本次服务启用和启动；持续跳过可预先创建 `/etc/ling/no-autostart`：

```sh
sudo env LING_SKIP_SERVICE=1 dpkg -i ./ling_<版本>_linux_<架构>.deb
# 或在安装、升级前设置持久标记
sudo mkdir -p /etc/ling
sudo touch /etc/ling/no-autostart
```

配置就绪后删除该标记，再执行 `sudo systemctl enable --now ling`。没有运行 systemd 的容器/主机可使用便携包并由自己的进程管理器启动；安装脚本不会假装服务已经运行。

升级时使用新的 DEB / RPM 覆盖安装，工作配置不会被样例覆盖。卸载使用 `sudo dpkg -r ling` 或 `sudo rpm -e ling`；会停止、禁用服务，保留工作配置、状态目录和服务用户。包内示例与文档位于 `/usr/share/ling/`。

tar.gz 解压后运行包内 `ling --config <路径>` 与 `ling-cli`。AppImage 先 `chmod +x <文件>.AppImage`；默认启动服务端，追加 `--cli -c 'PING'` 使用客户端。缺少 FUSE 时使用 `--appimage-extract-and-run`。便携包不注册服务。

## macOS Intel / Apple Silicon

运行匹配架构的 `ling_<版本>_macos_<架构>.pkg`，或使用：

```sh
sudo installer -pkg ./ling_<版本>_macos_<架构>.pkg -target /
```

程序位于 `/usr/local/bin/ling` 和 `/usr/local/bin/ling-cli`。安装器注册系统 LaunchDaemon `io.ling.cache`，以 `_ling` 用户运行。工作配置位于 `/usr/local/etc/ling/ling.toml`，工作目录位于 `/usr/local/var/lib/ling`。示例与文档位于 `/usr/local/share/ling/`。

安装前需要跳过自动服务时创建标记：

```sh
sudo mkdir -p /usr/local/etc/ling
sudo touch /usr/local/etc/ling/no-autostart
```

配置就绪后删除标记并载入服务：

```sh
sudo launchctl bootstrap system /Library/LaunchDaemons/io.ling.cache.plist
sudo launchctl kickstart -k system/io.ling.cache
sudo launchctl print system/io.ling.cache
ling-cli -c 'PING'
```

停止使用 `sudo launchctl bootout system /Library/LaunchDaemons/io.ling.cache.plist`，重新启动使用 `bootstrap`。升级重新运行 PKG，保留现有工作配置；卸载执行 `sudo /usr/local/bin/ling-uninstall`，先注销服务再移除程序。配置、状态和 `_ling` 用户保留。tar.gz 是便携包，不注册 LaunchDaemon。PKG 尚未签名/公证。

## 配置和迁移边界

服务启动后用 `ling-cli -c 'PING'` 验证真实协议响应，应为 `"PONG"`。`ling --help` 只验证帮助入口。CLI 可通过 `--port`、`--db` 和 TLS 参数连接自定义实例，参见 [CLI 指南](cli.md)。

前台运行 `ling` 时优先读取当前目录 `ling.toml`，没有则兼容 `linger.toml`，均不存在时使用默认值。显式 `--config` 路径不存在或非法时退出。服务安装始终使用上面的固定配置路径。配置中的相对 TLS / Journal 路径按配置文件所在目录解析；移动配置时必须相应调整。服务用户必须有读取证书、私钥及写入 Journal 目录的权限。

从旧项目名迁移时，旧的 `linger-server` / `linger-cli` 和安装目录不会自动转换；脚本请改用 `ling` / `ling-cli`。Rust 包改为 `ling-*`；Java `io.linger.cache`、Go `linger.local/cache` 和现有 Prometheus `linger_*` 指标保留兼容名称。CLI 优先读取 `LING_PASSWORD`，缺失时兼容 `LINGER_PASSWORD`。

更名未改变协议或 Journal 格式。升级和重启后缓存值与 TTL 仍为空；仅 Reliable 已提交事件及消费元数据在保留边界内恢复。更换 Journal 目录可能改变流身份，不要把同名消费组视为原进度。多日压力、目标部署环境安装验收及正式版签名仍需在 1.0.0 前完成，见 [路线图](roadmap.md)。
