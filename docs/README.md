# Into Markdown 文档导航

本文档集描述当前产品与发布边界。公共 CLI、DTO、能力名称、插件 ID、Agent Skill 名称和
发布路径属于稳定接口；实现细节以代码、机器权威清单和自动化测试为准。ADR 记录当时的架构
决策，不用来替代当前接口文档。

## 使用与公共接口

- [CLI](cli.md)：转换、批量、Bundle、能力、Provider、插件、配置和诊断命令。
- [可执行命令与格式示例](cli-examples.md) / [English](cli-examples.en.md)：全部公共命令和
  当前可用格式的可运行示例。
- [安装与部署](user-guide.md) / [English](user-guide.en.md)：五目标校验、安装、离线能力、
  排障和卸载。
- [配置](configuration.md)：分层配置、profile、网络授权和能力路由。
- [格式矩阵](formats.md)：catalog 中的格式、检测、转换和失败边界。
- [DTO](dto.md)：Document IR、result JSON、batch report 与兼容规则。
- [本地 Web 服务](ui.md)：工作台、任务历史、产物、管理能力和 loopback 安全边界。
- [Agent Skill](agent-skill.md)：`into-markdown` skill 的安装、发布和验证。

## 架构与扩展

- [架构](architecture.md)与[公共接口](interfaces.md)：Engine、IR、恢复、资源预算与 SPI。
- [能力插件](capability-plugins.md)：OCR、语音与说话人分离的完整插件边界。
- [插件管理](plugin-management.md)、[进程插件](process-plugins.md)与
  [WASI 插件](wasi-plugins.md)：签名、信任、作用域、隔离与生命周期。
- [插件开发](plugin-development.md) / [English](plugin-development.en.md)：协议选择、开发、
  签名、真实执行和发布门禁。
- [OCR 与 AI](ocr-and-ai.md)：embedded OCR、本地能力和远端 Provider 的证据边界。

完整能力插件是本地模型与运行时的安装、更新和验证单元。仓库 `models/` 保存这些插件所需的
构建与供应链权威。

## 安全、质量与发布

- [安全模型](security.md)、[Web 威胁模型](web-security-threat-model.md)和
  [ODF 安全](odf-security.md)。
- [测试策略](testing.md)、[安装后 smoke](installed-smoke.md)、
  [语义布局质量](semantic-layout-quality.md)与 [QA 门禁](qa/test-gates.md)。
- [macOS ARM64 发布](macos-arm64-release.md)与
  [Linux/Windows 发布](platform-modular-release.md)。
- [许可证治理](licensing.md)、[许可证检查](license-governance.md)和
  [第三方许可说明](../THIRD_PARTY_NOTICES.md)。
- [贡献指南](../CONTRIBUTING.md) / [English](../CONTRIBUTING.en.md)。

每个平台产品发布由一个 Core、两个自包含能力插件和一份平台无关 Agent Skill 构成。
Core 内置 canonical skill 目录；独立 ZIP 与 Core 副本逐文件一致。用户将 skill 显式安装到
agent 的发现目录。

## 文档维护规则

- 示例遵循真实公开 CLI 语法；模型相关安装与更新以完整能力插件为管理单元。
- 每个公共 CLI 命令和每种当前可用格式必须在中英文可执行示例中各出现一次，并由真实
  `into-md` 和实时 format catalog 在 CI 中校验。
- 本地与远端能力都按当前 invocation 的网络授权执行；普通转换默认离线。
- 发布结论必须来自全新安装包与真实文档、图片、音视频黑盒验收。
- 历史兼容描述必须明确标为兼容读取或迁移行为，不能冒充当前推荐用法。
- 改动公共接口、能力路由、发布内容或安装行为时，同时更新本导航关联的权威文档、README、
  installed-smoke 和发布测试。
