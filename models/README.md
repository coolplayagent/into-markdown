# 模型供应链

`manifest.json` 是模型 bundle 与来源目录；运行时与
`third_party/licenses/downloads.json` 交叉校验。它通过 SHA-256 固定上游
PP-OCRv6 tiny source archives，并记录目标语言、四个平台和许可证。source
archives 不是可安装的最终 ONNX runtime files。

仅在需要模型源产物时运行手动 Bazel 目标：

```shell
bazel build //models:source_models
```

普通构建与测试目标不依赖此 filegroup。生成的 ONNX 模型、ONNX Runtime 包和
本地下载资源不得提交到 Git。只有在可复现转换、派生产物哈希、字符表哈希、大小、
第三方声明和平台专用包全部进入权威清单并通过 release audit 后，bundle 才能标记
为 `available`。
