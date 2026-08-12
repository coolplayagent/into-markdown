# into-markdown

[English](README.en.md)

`into-markdown` 是一个使用 Rust 开发、由 Bazel 构建的文档转 Markdown
平台。仓库当前提供架构设计、公共服务提供者接口、注册表与转换流水线骨架、
命令行程序及契约测试；暂未实现生产可用的格式解析、OCR 推理、网络客户端或
LLM 调用。

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

## 命令行骨架

```shell
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- plugins
```

实现路线详见[架构设计](docs/architecture.md)、[接口契约](docs/interfaces.md)、
[格式矩阵](docs/formats.md)、[OCR 与 AI](docs/ocr-and-ai.md)、
[安全模型](docs/security.md)和[测试策略](docs/testing.md)。
