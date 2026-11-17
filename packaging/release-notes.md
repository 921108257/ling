这是预发布版本。项目自有代码采用 MIT 许可证，发行包附带 `LICENSE` 与 `THIRD-PARTY-NOTICES.txt`；第三方组件保留各自许可。安装器未签名，macOS PKG 未公证。

提供 Windows x86_64、Linux x86_64 / ARM64、macOS Intel / Apple Silicon 的 14 个安装包与便携包、每包 CycloneDX SBOM、依赖审计附件、`SHA256SUMS` 和 `PROVENANCE.sigstore.json`。下载后应核对 SHA-256 并按安装指南验证来源证明。内置更新验证要求 GitHub CLI 2.97.0 或更高版本；来源证明不替代平台代码签名。

Windows 仅支持 Fast。Linux / macOS 的 Reliable 在保留窗口内恢复已提交事件和消费元数据；缓存值与 TTL 在重启后为空。升级可能重启服务。协议 v1、类型标签与 Journal v1 保持不变。

CI 验证不能替代目标部署环境评估。每版实际运行的压力、故障恢复与 Redis 8 测量、失败结果及未完成项以下列版本记录和[测试记录](https://921108257.github.io/ling/docs/testing/)为准；WSL 崩溃恢复不能替代物理断电，平台签名与 macOS 公证尚未验收。

[快速上手](https://921108257.github.io/ling/docs/quickstart/) · [安装与更新](https://921108257.github.io/ling/docs/install/) · [CLI 指南](https://921108257.github.io/ling/docs/cli/)
