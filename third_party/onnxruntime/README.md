# ONNX Runtime

生产 OCR 运行时固定使用官方 ONNX Runtime 1.29.0 CPU 包，支持 macOS ARM64、
Linux x86_64、Linux ARM64 和 Windows x86_64。`manifest.json` 记录上游
GitHub Release 发布的产物及其 SHA-256。

相关 filegroup 标记为 `manual`：脚手架在普通构建和测试中既不下载也不链接
运行时。未来的 FFI BUILD 目标会根据目标平台准确选择一个运行时。项目有意不
提供 macOS x64 仓库或目标。
