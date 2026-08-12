# into-markdown

[English](README.en.md)

`into-markdown` 是一个使用 Rust 开发、由 Bazel 构建的文档转 Markdown
平台。仓库当前提供架构设计、公共服务提供者接口、注册表与转换流水线、确定性
GFM 渲染器、TXT 与字符集转换器、命令行程序及契约测试；暂未实现 OCR 推理、
网络客户端或 LLM 调用。

本项目完全独立于相邻的 `anydoc` 和 `markitdown` 项目实现。包括 PDF、OCR
和 AI 生成内容在内的所有输入，都必须先进入带溯源信息的统一中间表示（IR），
再由中央渲染器生成 GitHub Flavored Markdown（GFM）。

## 构建

```shell
bazel build //...
bazel test //...
cargo check --workspace
```

支持的目标平台为 macOS ARM64、Linux x86_64、Linux ARM64 和 Windows
x86_64。项目明确不支持 macOS x86_64。

## 命令行

```shell
bazel run //apps/cli:into-md -- report.pdf
bazel run //apps/cli:into-md -- notes.txt
printf 'caf\351\n' | bazel run //apps/cli:into-md -- --charset windows-1252 -
bazel run //apps/cli:into-md -- report.pdf -o report.md
bazel run //apps/cli:into-md -- documents/ --recursive --output-dir markdown/
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- models show pp-ocrv6-tiny-zh-en --json
bazel run //apps/cli:into-md -- doctor
```

CLI 采用直接输入形式，不提供 `convert` 子命令。支持多文件与目录批量处理、stdin、
URI、OCR/AI 策略、结构化 JSON、资源 Bundle、分层配置、Provider、模型与插件管理。
联网与 AI 默认关闭，远程输入和 Provider 每次都需要显式 `--allow-network`。

结构化产物以 `schemaVersion: 1` 开始；转换结果、Bundle 和批量报告使用 CLI 与未来
HTTP 服务共享的公共 DTO。例如 `--emit result-json` 返回 `markdown`、版本化
`document`、base64 `assets`、`diagnostics` 和 `provenance`。协议细节、兼容策略与
不可信 JSON 资源预算见[稳定数据传输契约](docs/dto.md)。

TXT 转换可用，支持 UTF-8、带 BOM 的 UTF-16 与受限的常见字符集自动检测；显式
`--charset` 支持 `windows-1252`、`gb18030`、`big5` 和 `shift_jis`。无效序列默认
严格失败，`--encoding-errors replace` 会替换并输出带原始字节范围的诊断。

模型查询、离线校验、路径和安全清理后端已实现；当前权威清单只有上游 source
archives，没有可安装的最终 ONNX/字符表产物，因此安装返回稳定
`componentUnavailable`；校验、路径和清理对该 source-only 条目返回同一错误，
不会读取伪造安装状态或伪装成功。其他格式转换、OCR 推理、Provider 请求和插件
执行后端尚未实现。Windows 模型安装在 reparse-safe 目录 handle 持久同步完成审计前
同样 fail closed；目录解析和离线元数据查询不受影响。

实现路线详见[架构设计](docs/architecture.md)、[接口契约](docs/interfaces.md)、
[格式矩阵](docs/formats.md)、[OCR 与 AI](docs/ocr-and-ai.md)、
[本地模型管理](docs/models.md)、
[安全模型](docs/security.md)和[测试策略](docs/testing.md)。
命令与配置契约详见[命令行设计](docs/cli.md)和[配置文件](docs/configuration.md)，
许可证、第三方来源和发布审计详见[许可证治理](docs/licensing.md)。
