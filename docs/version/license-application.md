# DEC-01：项目级开源许可确认单

**这不是软件著作权登记。** 采用现成的开源许可，是有权授权的人选定条款并随项目发布，无需向 OSI 或包仓库申请发证。当前基线未发现顶层 `LICENSE`；本表不代表已经授权。如没有单位审批流程，维护者只需确认下列几项。独立实现协议不需要领取「官方 SDK 许可证」。

| 请维护者填写 | 决定 |
|---|---|
| 项目 / 生效版本或标签 | ling / ______ |
| 有权授权的个人或组织、署名 | ______（如含其他权利人的贡献，先确认授权范围；不从 Git 作者名推定） |
| 选用许可，择一 | ☐ [MIT](https://opensource.org/license/mit)（条款简短）　☐ [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)（含明示专利许可及条件）　☐ 其他：______ |
| 覆盖范围 / 例外 | 建议覆盖本项目自有服务端、CLI、仓内客户端源码、文档与测试夹具；例外：______。第三方组件维持原许可。 |
| 确认人 / 日期 | ______ / ______ |

**确认后再做（[0.1.4 工作包 6](v0.1.4-preview.md)）：** 在根目录放所选标准全文并写明范围，仓库元数据与之保持一致。按实际分发的文件保留版权、许可及适用的 NOTICE，不以依赖锁文件代替制品核对。例如 Windows Setup 已带 WinSW 的 MIT 文本，便携包不含 WinSW；检查新包和便携升级后的声明，不要求所有包带同一份无关组件清单。

**SDK 不作为交付前提。** 项目自有 SDK 可以和主项目共用同一授权，无需每种语言另办；不发包就不用申请账号或坐标。若有人自行编写独立 SDK，由作者决定自己代码的许可证和发布方式；复制已有实现或材料则按原授权处理。上传仓库是分发渠道，不会替作者作出开源授权。

仅在决定独立发包时查：Cargo 的[包许可字段](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields)（crates.io 要求 `license` 或 `license-file`）；[Maven Central 的 POM 要求](https://central.sonatype.org/publish/requirements/#license-information)；Go 的[文档展示许可政策](https://pkg.go.dev/license-policy)（可拉取不等于一定展示文档）；npm 的[`license` 字段](https://docs.npmjs.com/cli/v11/configuring-npm/package-json#license)。仓内 `site` 是私有建站项目，不是 npm SDK。
