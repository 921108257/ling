# 安装、服务与升级

本页按 0.2.0 Preview 准备，标签构建与发布验收尚待完成；指定版本的示例需在该版本提供下载后使用。项目自有代码采用 [MIT 许可证](../LICENSE)，发行包附带项目许可和第三方声明。下载 [Releases](https://github.com/921108257/ling/releases) 中与系统、CPU 架构匹配的包，并对照同页 `SHA256SUMS` 校验。Windows 使用 `Get-FileHash <文件> -Algorithm SHA256`，Linux 使用 `sha256sum`，macOS 使用 `shasum -a 256`；来源证明验证步骤见下文。

安装器默认注册并启动后台服务，默认监听 `127.0.0.1:6380`，事件模式为 Fast。服务使用专用工作目录，不读取用户终端当前目录的配置。需要自定义配置或已有实例占用端口时，先跳过自动启动，修改配置后再启动。Reliable 限制见 [architecture.md](architecture.md) 和 [persistence.md](persistence.md)。

| 运行平台 | Fast | Reliable | 验收边界 |
|---|---|---|---|
| Windows x86_64 | 支持 | 不支持，启动前明确拒绝 | 目录同步、原子替换与恢复全路径未实现和验收；不会降级模式 |
| Linux x86_64 / ARM64 | 支持 | 支持满足同步语义的文件系统 | 现有 CI / 本机恢复契约，不能代替目标硬件断电验收 |
| macOS Intel / ARM64 | 支持 | 支持满足同步语义的文件系统 | 现有 CI 恢复契约，设备写缓存与物理断电仍未验收 |

0.2.0 保留上述支持矩阵。一次目录同步 API 调用成功不足以证明断电后的目录项持久化，不以此开放 Windows Reliable。

## 认证与暴露面

默认回环监听适合本机访问。远程服务应同时配置认证和 TLS，只向受控客户端放行业务端口；Prometheus 建议保持回环监听，通过受控采集链路访问。启动告警只提示暴露组合，不代替网络访问限制。

认证是实例级权限，已认证连接可以执行 `SHUTDOWN`、`CONFIG SET` 与 `FLUSHALL`，数据库编号不隔离权限。口令比较、失败延迟和断开规则见[配置规范](config.md#认证与暴露面)，管理命令的契约见[命令规范](commands.md)。

## 命令行更新

0.1.6 及更新版本的更新器要求 GitHub CLI **2.97.0 或更高版本**。先从 GitHub CLI 官方渠道安装 `gh` 并确认 `gh --version`；离线 bundle 验证不要求登录 GitHub。发行包默认包含更新器；源码部署可用 `cargo build --release -p ling-server --no-default-features` 关闭，关闭后更新命令明确报 `LOCAL_UPDATE_DISABLED`。

从 0.1.3 起，可使用 `ling update` 更新服务端和客户端。0.1.2 及更早版本请先按下方平台说明安装 0.1.3。

```sh
ling update --check
ling update
ling update --version 0.2.0
ling update --stable --check
ling help update
```

`--check` 仅检查版本，不下载或安装，从 0.1.6 起还会报告证明文件及验证器是否可用；显示“已提供”不代表完成密码学验证。默认选择最新的已发布版本，包含预发布版本；`--stable` 仅选择稳定版。`--version` 固定目标版本，不允许降级；没有更新时不会重新安装。版本按语义版本比较。0.1.6 及更新版本的更新器下载官方 GitHub Releases 中与系统、架构、安装方式匹配的资产，先核对大小及 `SHA256SUMS`，再用内置公共 Sigstore 根验证 `PROVENANCE.sigstore.json`：仓库、工作流、目标标签、托管 runner、签名和资产摘要必须匹配。没有证明、验证失败或缺少 `gh` 都停止安装，不降级为仅检查 SHA-256。网络或验证失败不会修改已安装程序。遇到 GitHub 请求限制时，可稍后重试或设置 `LING_GITHUB_TOKEN`；令牌仅用于固定 GitHub API 请求，不会传入验证子进程、下载 CDN、更新计划或诊断。0.1.5 及以前的更新器仍只校验 SHA-256，首次从旧版升级时建议按下节独立验证目标版本。

Linux / macOS 的系统安装需要管理员权限：

```sh
sudo ling update
```

系统安装使用原生安装器，保留已有工作配置和状态目录，并按安装器约定停止、更新和启动服务。从 0.1.6 起，便携包更新 `ling`、`ling-cli`、`LICENSE` 和 `THIRD-PARTY-NOTICES.txt` 四个固定成员；AppImage 更新原始 AppImage 文件。便携进程由你自行停止和重新启动，文件占用时更新会失败。Unix 中断后可能保留安装目录旁的 `.ling-update-recovery-*`：停止两个程序，按错误输出运行该目录的 `restore.sh`，恢复完整旧版后再更新；保留该目录及备份直至确认恢复。此机制不构成物理断电保证。未识别为官方安装包或便携包的安装方式，请按其原安装方式升级。

Windows 在下载校验后退出当前命令，再继续安装，以释放程序文件。出现管理员权限提示时请确认。命令输出结果文件路径；使用 `Get-Content '<结果文件路径>'` 查看 `status`、`installer_started`、`helper_pid`、`installer_pid` 与 `updated_at_unix_ms`。`success` 表示完成，未启动安装器的取消／失败为 `failed`；启动后的失败或等待超过十分钟记为 `unknown`，保留 `.pending.json` 防止盲目重试。助手不会终止已启动的安装器。确认安装器与助手都已结束，再核对两个程序版本，读取结果文件同目录 `plan.json` 的 `lock_path`，检查该路径加 `.pending.json` 后缀的文件，必要时人工恢复或清理；不要只凭超时再次安装。交接成功不等于安装完成。便携更新失败且 `rollback_complete` 为 `false` 时，请保留结果文件所在目录，并重新安装官方发行包。

更新可能重启服务。缓存值与 TTL 在重启后不恢复；Reliable 仅恢复保留窗口内已提交事件及消费元数据。

Windows 更新锁与 pending 标记按用户隔离；对同一安装目录的操作应由同一管理员用户串行执行。恢复脚本也应在停止相关程序后顺序执行。

## 独立验证 0.2.0 下载

从官方 Release 下载选定包、`SHA256SUMS`、`PROVENANCE.sigstore.json` 和同名 `.cdx.json`（CycloneDX 1.6 SBOM）。先核对包的 SHA-256，再用可信来源的 GitHub CLI 验证。以下命令在 PowerShell、Linux 和 macOS 均适用，替换为实际文件名：

```sh
gh attestation verify ling_0.2.0_windows_x86_64.zip --bundle PROVENANCE.sigstore.json --repo 921108257/ling --cert-identity https://github.com/921108257/ling/.github/workflows/release.yml@refs/tags/v0.2.0 --source-ref refs/tags/v0.2.0 --deny-self-hosted-runners
```

这条独立命令默认通过 GitHub CLI 的 TUF 机制获取可信根；离线时，额外传入 `--custom-trusted-root <可信副本路径>`。固定根的来源与轮换见 [DEC-02](decisions.md)。不要把与待验证文件一起取得的任意根当作独立信任依据。验证包、SBOM、`SHA256SUMS` 时分别替换命令中的文件名；同一 bundle 绑定所有资产，bundle 本身不自签。分支构建的证书身份是分支 ref，不能冒充标签发行。

任何摘要、来源、标签或签名验证失败都不要运行安装器；保留失败文件与诊断，从可信渠道重新获取并核查。来源证明不会消除 Windows SmartScreen、macOS Gatekeeper 提示，平台签名与公证仍未提供。

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

更名未改变协议或 Journal 格式。升级和重启后缓存值与 TTL 为空；仅 Reliable 已提交事件及消费元数据在保留边界内恢复。更换 Journal 目录可能改变流身份，不要把同名消费组视为原进度。
