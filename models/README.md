# 模型供应链

`manifest.json` 是权威模型来源目录。它通过 SHA-256 固定上游 PP-OCRv6 tiny
归档，并记录目标语言和许可证。

仅在需要模型源产物时运行手动 Bazel 目标：

```shell
bazel build //models:source_models
```

普通构建与测试目标不依赖此 filegroup。生成的 ONNX 模型、ONNX Runtime 包和
本地下载资源不得提交到 Git。未来的生产打包流程将增加可复现的 Paddle-to-ONNX
转换、派生产物哈希、第三方声明和平台专用包。
