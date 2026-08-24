# 模型供应链权威

本目录只保存构建、来源和发布审计权威，不是面向用户的模型管理目录。产品中的本地模型始终
随 `official.ocr.ppocrv6` 或 `official.media.whisper` 完整能力插件交付；用户安装、校验、更新
和移除整个插件，不单独下载、替换或选择模型。

`manifest.json` 与 `third_party/licenses/downloads.json` 交叉校验，通过 SHA-256 固定上游
模型 source archive、目标语言、支持平台和许可证。source archive 仅供受控构建使用，不是
可安装的最终 ONNX runtime 文件，也不会被普通转换路径读取。

仅在需要模型源产物时运行手动 Bazel 目标：

```shell
bazel build //models:source_models
```

普通构建与测试目标不依赖此 filegroup。生成的 ONNX 模型、ONNX Runtime 包和本地下载资源
不得提交到 Git。派生产物只有在可复现转换、哈希、字符表、大小、第三方声明、平台包清单和
release audit 全部通过后，才可以进入对应能力插件。
