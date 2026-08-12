# OCR 与 AI

## 本地 OCR

默认模型包为 `pp-ocrv6-tiny-zh-en`，使用 PP-OCRv6 tiny 检测与识别源模型，
面向简体中文、繁体中文和英文混排。模型产物不提交到 Git。
`models/manifest.json` 记录 HTTPS 来源、SHA-256、模型角色、上游版本、语言、
格式和 SPDX 许可证。

这些条目当前是生成部署模型所需的 source archives，不是可安装的最终 ONNX
runtime files。模型管理命令会将 bundle 显示为 `planned` / `unavailable`，不会
将源码归档当作已安装推理模型。查询、校验、平台目录、原子安装与清理契约见
[本地模型管理](models.md)。

普通构建和测试既不下载模型，也不加载推理运行时。手动目标
`//models:source_models` 用于获取已固定哈希的上游源归档。未来会增加可复现的
转换动作，以生成准确的 ONNX 部署产物及其派生哈希。

OCR 实现负责图像解码、方向检测、归一化、DBNet 后处理、阅读顺序、裁剪批处理、
CTC 解码、置信度计算和 IR 合并。模型执行隐藏在 `TensorRuntime` 之后，初始
实现使用支持平台上固定版本的 ONNX Runtime CPU 包。

`OcrPolicy` 可取 `off`、`auto` 或 `always`，默认值为 `auto`。自动模式下，
只有图片输入、纯图片页面、可能含文字的内嵌图片，或原生文本提取不足的页面才应
触发 OCR。

## AI 提供者

AI 必须显式启用，并按能力路由。提供者可以分别支持视觉 OCR、图片描述、版面
修复、表格修复、公式修复、音频转写或 Markdown 后处理。提供者输出只能以带
溯源信息的节点，或经过验证且带版本的补丁形式进入 IR。

OpenAI-compatible HTTP 是规划中的适配器，不是 `core` 的强依赖。秘密信息只以
环境变量名引用，不得被序列化、写入日志、加入溯源信息或在普通配置文件中直接
接收。
