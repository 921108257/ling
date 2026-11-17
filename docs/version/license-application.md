# DEC-01：项目级开源许可确认单

**这不是软件著作权登记。** 维护者于 2026-11-17 明确决定「许可证使用 MIT」。项目根目录的 [LICENSE](../../LICENSE) 提供完整条款；第三方组件仍按各自许可分发。独立实现协议不需要领取「官方 SDK 许可证」。

| 确认事项 | 决定 |
|---|---|
| 项目 / 生效版本或标签 | ling / v0.1.4 |
| 版权署名 | `Copyright (c) 2026 ling contributors`；不从 Git 作者名推定个人权利人 |
| 选用许可 | [MIT](https://opensource.org/license/mit) |
| 覆盖范围 / 例外 | 本项目自有服务端、CLI、仓内客户端源码、文档与测试夹具；第三方组件维持原许可 |
| 决定来源 / 日期 | 维护者在本次发布会话中明确指定 MIT / 2026-11-17 |

**制品落实（[0.1.4 工作包 6](v0.1.4-preview.md)）：** 根目录保存 MIT 全文，九个工作区 Rust 包继承该许可元数据。各平台安装包与便携包均带项目 `LICENSE`；打包时通过 `packaging/generate-notices.py` 读取相应目标的已锁定服务端 / CLI 普通依赖与构建依赖，保留上游许可、版权与 NOTICE 原文，生成 `THIRD-PARTY-NOTICES.txt`。声明同时包含当前编译器的标准库许可清单与全文；Linux 静态包包含构建主机所安装 musl 的版权声明。Windows Setup 额外保留实际随包 WinSW 的 MIT 文本，便携包不含 WinSW。安装与便携制品中的可读性由打包验证核对；便携升级后的一致性仍属于 0.1.6 的验收范围。

**SDK 不作为交付前提。** 项目自有 SDK 可以和主项目共用同一授权，无需每种语言另办；不发包就不用申请账号或坐标。若有人自行编写独立 SDK，由作者决定自己代码的许可证和发布方式；复制已有实现或材料则按原授权处理。上传仓库是分发渠道，不会替作者作出开源授权。

仅在决定独立发包时查：Cargo 的[包许可字段](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields)（crates.io 要求 `license` 或 `license-file`）；[Maven Central 的 POM 要求](https://central.sonatype.org/publish/requirements/#license-information)；Go 的[文档展示许可政策](https://pkg.go.dev/license-policy)（可拉取不等于一定展示文档）；npm 的[`license` 字段](https://docs.npmjs.com/cli/v11/configuring-npm/package-json#license)。仓内 `site` 是私有建站项目，不是 npm SDK。
