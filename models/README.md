# 模型供应链权威

本目录保存构建、来源和发布审计权威。产品中的本地模型随 `official.ocr.ppocrv6` 或
`official.media.whisper` 完整能力插件交付；用户以完整插件为单位执行安装、校验、更新和移除。

`manifest.json` 与 `third_party/licenses/downloads.json` 交叉校验，通过 SHA-256 固定上游
模型 source archive、目标语言、支持平台和许可证。source archive 供受控构建使用；可安装的
ONNX runtime 与模型产物经发布门禁后进入对应能力插件。

仅在需要模型源产物时运行手动 Bazel 目标：

```shell
bazel build //models:source_models
```

普通构建与测试目标不依赖此 filegroup。生成的 ONNX 模型、ONNX Runtime 包和本地下载资源
不得提交到 Git。派生产物只有在可复现转换、哈希、字符表、大小、第三方声明、平台包清单和
release audit 全部通过后，才可以进入对应能力插件。
