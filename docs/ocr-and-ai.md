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

检测模块只接收调用方已解码且明确描述的 `PixelView`：宽、高、row stride、
`Gray8`/`RGB8`/`BGR8`/`RGBA8`/`BGRA8`、八种 EXIF 方向和借用的像素字节。
它不猜测格式、不解码图片、不访问文件或网络；完整 `ImageConverter` 由独立的图片
转换安全边界提供。透明通道与 PaddleOCR 的 BGR 转换一致地忽略，不做背景合成。

PP-OCRv6 tiny 检测参数的机器可读权威文件是
`models/ppocrv6-tiny-detector-authority.json`。它固定 PaddleOCR 仓库 commit
`2661c7c0ef5c613e8f93c6e93b2e052399f0f854`、
`configs/det/PP-OCRv6/PP-OCRv6_tiny_det.yml` 和 DB 论文
`https://arxiv.org/abs/1911.08947`。预处理按官方配置进行 BGR、短边 736、长边
最多 4000、尺寸 round 到 32 的倍数、`1/255`、mean/std 和 NCHW 排列。归一化逐项
复刻 `NormalizeImage`：先把 uint8 转为 f32，再乘以固定 f32 `1/255`，随后减 f32 mean
并除以 f32 std；不能以除以 255 替代乘法。输入宽高之和
小于 64 时，严格按固定 Paddle 实现先向右/下补零到至少 32；缩放尺寸先经 Python
`int` 截断，超出 4000 时基于截断尺寸二次缩放并再次截断，最后才按 Python ties-even
round 到 stride。像素插值固定以 OpenCV 4.13 默认 `INTER_LINEAR` 的 uint8 输出为参考，
参考集每通道误差最多 1 LSB，归一化只发生在 uint8 舍入之后。八种方向在采样前映射，
输出框再以同一像素中心坐标规则逆变换到原图。

DB 后处理固定 bitmap threshold `0.2`、polygon box score `0.4`、最多 3000 个
候选和 unclip ratio `1.4`。二值图使用 Suzuki–Abe 边界跟踪；与官方
`RETR_LIST` 一样，outer 与 hole contour 都进入相同评分流程，而不是静默丢弃 hole。
每个候选经过 minimum-area rectangle、polygon mean score、closed round polygon
offset 和第二次 minimum-area rectangle，再按 PaddleOCR 的 top-left、top-right、
bottom-right、bottom-left 规则规范四点。输出仅包含原图坐标、角度、置信度和供后续
识别使用的 crop descriptor；recognizer、CTC、透视裁剪执行和 IR 合并不属于检测模块。
中英文混排区域严格使用固定 Paddle `predict_system.py` 的横排启发式：先按左上点
`(y,x)` 稳定排序，再仅对左上点 y 差严格小于 10 的相邻框向左插入。该规则不声明
支持垂排阅读顺序。

概率验证、bitmap 构造和 score 扫描均周期执行请求 checkpoint。score 对所有候选的
bounding-box pixels 与基于四条最大 8001-step 整数边推导的保守 work 上界做 checked
累计；嵌套大轮廓超过公开资源上限时在扫描前返回 `ResourceLimit`。round offset 只接收
已验证的 convex 四点路径；进入 `clipper2-rust` 前按固定 104 点、108 个 path header 和
104² work 的审计上界检查 caller cap、预留逻辑内存并执行 checkpoint，返回后立即再次
checkpoint。第三方单次调用不支持中途轮询，其输入与工作上界因此保持为常数。

普通测试使用 fake runtime 与人工概率图，不下载模型。当前 manifest 没有可执行
detector ONNX 文件，因此产品 resolver 稳定返回 `ModelUnavailable`，不能把 source
archive 当作模型。矩形、旋转框、hole、空图、噪点、非法概率、极端 stride、方向
逆变换、取消与逻辑预算均有离线测试。

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
