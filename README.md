# Into Markdown

[English](README.en.md) · [文档导航](docs/README.md)

Into Markdown 是本地优先、默认离线的文档转 Markdown 产品。它提供统一的 Rust 转换核心、
`into-md` CLI、本地 Web 工作台、稳定 JSON/Bundle 接口，以及三个自包含的可选能力插件。
所有内容先进入带诊断与溯源的 Document IR，再由唯一的确定性渲染器生成 GFM。

项目独立于相邻的 `anydoc` 与 `markitdown`，不调用或包装它们。

## 产品组成

- Core：CLI、Web 工作台、Document IR、格式检测与转换、PDFium、安全输出事务、插件与
  Provider 管理、安装后验收工具。
- OCR 插件 `official.ocr.ppocrv6`：PP-OCRv6、ONNX Runtime、worker、模型与字符表。
- 语音插件 `official.media.whisper`：FFmpeg、Whisper、VAD、说话人分离模型与运行时。
- 旧版 Office 插件 `official.legacy-office.libreoffice`：LibreOffice runtime 与隔离 worker。
- Agent Skill `into-markdown`：指导兼容 agent 使用已安装的 `into-md`，同时提供独立 ZIP 并
  以相同字节内置于每个平台 Core。

模型是完整能力插件的内部实现资源，不是独立安装、更新或切换的产品对象。普通转换不会
下载插件、模型或远端内容。

## 支持范围

当前格式 catalog 包含 PDF、DOC/DOCX、PPT/PPTX、XLS/XLSX、ODT/ODS/ODP、RTF、EPUB、
TXT、Markdown、HTML、CSV/TSV、JSON、XML、RSS/Atom Feed、Jupyter Notebook、图片、ZIP、
Outlook MSG、音频和视频。旧版 Office、OCR、语音转写与说话人分离由对应能力插件提供。

支持 macOS ARM64、Linux x86_64、Linux ARM64 和 Windows x86_64；不支持 macOS x86_64。
平台边界见 [macOS 发布](docs/macos-arm64-release.md)与
[Linux/Windows 发布](docs/platform-modular-release.md)。

## 使用 CLI

转换直接接收输入，不存在 `convert` 子命令：

```sh
into-md version --json
into-md report.docx -o report.md --conflict error --log-format json
into-md scan.png --ocr always -o scan.md --conflict error --log-format json
into-md meeting.webm --ai audio-transcription=only \
  -o meeting.md --conflict error --log-format json
```

批量任务先规划，再转换并读取逐项报告：

```sh
into-md documents/ --recursive --output-dir markdown/ \
  --conflict error --dry-run --log-format json
into-md documents/ --recursive --output-dir markdown/ \
  --conflict error --report conversion-report.json --log-format json
```

远程来源和远端 Provider 默认拒绝。只有与用户当前意图一致时，才为本次调用增加
`--allow-network`，并尽量使用 `--allow-host` 收窄主机；私网还需要单独的
`--allow-private-network` 授权。

完整接口见 [CLI](docs/cli.md)、[配置](docs/configuration.md)、[格式矩阵](docs/formats.md)和
[DTO](docs/dto.md)。

## 能力与 Web 工作台

```sh
into-md capabilities list --json
into-md capabilities show ocr --json
into-md setup ocr
into-md setup media
into-md setup legacy-office
into-md doctor --json
into-md ui
```

`setup` 安装并验证完整官方能力插件；转换过程不会隐式调用它。`into-md ui` 只监听
`127.0.0.1`，提供批量转换、进度与取消、任务历史、产物预览/下载，以及格式、能力、
Provider、插件、配置和诊断管理。Web 与 CLI 复用相同的能力路由、配置和安全边界。

详见 [能力插件](docs/capability-plugins.md)、[插件管理](docs/plugin-management.md)和
[本地 Web 服务](docs/ui.md)。

## Agent Skill

发布件 `into-markdown-skill.zip` 可显式解压到 agent 的 skill 发现目录。每个平台 Core 也包含
`share/into-markdown/skills/into-markdown/`，便于用户自行复制或建立链接。产品安装器和
卸载器不会修改用户的 agent 目录。Codex 可显式调用 `$into-markdown`，也可在匹配的转换任务中
自动选择它。

安装、校验与发布契约见 [Agent Skill 发布与安装](docs/agent-skill.md)。

## 开发与验证

```sh
bazel build //...
bazel test //...
cargo check --workspace
```

Bazel 是发布构建权威；Cargo 用于快速开发检查和定向测试。真实发布结论必须来自全新 Core
安装、能力插件安装与真实文件/媒体黑盒验收，不能只依据编译、mock 或 skill 结构校验。

架构、安全、测试和发布文档统一收录在 [docs/README.md](docs/README.md)。许可证与第三方来源
见 [LICENSE](LICENSE)、[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)和
[许可证治理](docs/licensing.md)。
