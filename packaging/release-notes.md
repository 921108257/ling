这是预发布版本。项目自有代码采用 MIT 许可证，发行包附带 `LICENSE` 与 `THIRD-PARTY-NOTICES.txt`；第三方组件保留各自许可。安装器未签名，macOS PKG 未公证。

提供 Windows x86_64、Linux x86_64 / ARM64、macOS Intel / Apple Silicon 的安装包与便携包，以及覆盖全部 14 个包的 `SHA256SUMS`。下载后应核对 SHA-256。

Windows 仅支持 Fast。Linux / macOS 的 Reliable 在保留窗口内恢复已提交事件和消费元数据；缓存值与 TTL 在重启后为空。升级可能重启服务。协议 v1、类型标签与 Journal v1 保持不变。

CI 验证不能替代目标部署环境评估；尚未执行多日压力、物理断电、Redis 8 对比和签名/公证验收。每版的实现与未完成项如下。

[快速上手](https://921108257.github.io/ling/docs/quickstart/) · [安装与更新](https://921108257.github.io/ling/docs/install/) · [CLI 指南](https://921108257.github.io/ling/docs/cli/)
