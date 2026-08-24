# Into Markdown 文档导航

本文档集描述当前产品与发布边界。公共 CLI、DTO、能力名称、插件 ID、Agent Skill 名称和
发布路径属于稳定接口；实现细节以代码、机器权威清单和自动化测试为准。ADR 记录当时的架构
决策，不用来替代当前接口文档。

## 使用与公共接口

- [CLI](cli.md)：转换、批量、Bundle、能力、Provider、插件、配置和诊断命令。
- [配置](configuration.md)：分层配置、profile、网络授权和能力路由。
- [格式矩阵](formats.md)：catalog 中的格式、检测、转换和失败边界。
- [DTO](dto.md)：Document IR、result JSON、batch report 与兼容规则。
- [本地 Web 服务](ui.md)：工作台、任务历史、产物、管理能力和 loopback 安全边界。
- [Agent Skill](agent-skill.md)：`into-markdown` skill 的安装、发布和验证。

## 架构与扩展

- [架构](architecture.md)与[公共接口](interfaces.md)：Engine、IR、恢复、资源预算与 SPI。
- [能力插件](capability-plugins.md)：OCR、语音、说话人分离和旧版 Office 的完整插件边界。
- [插件管理](plugin-management.md)、[进程插件](process-plugins.md)与
  [WASI 插件](wasi-plugins.md)：签名、信任、作用域、隔离与生命周期。
- [OCR 与 AI](ocr-and-ai.md)：embedded OCR、本地能力和远端 Provider 的证据边界。

本地模型只属于完整能力插件，不提供独立模型安装、更新、替换或选择接口。仓库 `models/`
保存构建与供应链权威，不代表面向用户的模型管理功能。

## 安全、质量与发布

- [安全模型](security.md)、[Web 威胁模型](web-security-threat-model.md)和
  [ODF 安全](odf-security.md)。
- [测试策略](testing.md)、[安装后 smoke](installed-smoke.md)、
  [语义布局质量](semantic-layout-quality.md)与 [QA 门禁](qa/test-gates.md)。
- [macOS ARM64 发布](macos-arm64-release.md)与
  [Linux/Windows 发布](platform-modular-release.md)。
- [许可证治理](licensing.md)、[许可证检查](license-governance.md)和
  [第三方许可说明](../THIRD_PARTY_NOTICES.md)。

每个平台产品发布由一个 Core、三个自包含能力插件和一份平台无关 Agent Skill 构成。
Core 内置 canonical skill 目录；独立 ZIP 与 Core 副本逐文件一致。产品安装器与卸载器不修改
用户的 agent skill 目录。

## 文档维护规则

- 示例使用真实公开命令，不虚构 `convert` 子命令或独立模型管理接口。
- 本地与远端能力都按当前 invocation 的网络授权执行；普通转换默认离线。
- 发布结论必须来自全新安装包与真实文档、图片、音视频黑盒验收。
- 历史兼容描述必须明确标为兼容读取或迁移行为，不能冒充当前推荐用法。
- 改动公共接口、能力路由、发布内容或安装行为时，同时更新本导航关联的权威文档、README、
  installed-smoke 和发布测试。
