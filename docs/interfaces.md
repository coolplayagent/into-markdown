# 接口契约

公共外观由 `into_markdown` crate 提供。调用方创建 `EngineBuilder`，按需显式
添加或替换能力提供者，构建不可变的 `Engine`，然后等待
`Engine::convert(ConversionRequest)` 完成。

## 输入源契约

`InputRef` 区分本地路径、内存、标准输入和 URI。`FormatHint` 可携带显式格式、
文件名、扩展名、MIME 类型和字符集。`SourceResolver` 读取数据时
必须执行 `ResourceLimits`。URI 解析器还必须执行 `NetworkOptions`，网络访问
默认关闭。解析器返回不可变字节和不含秘密的元数据，避免检测器与转换器共享
可变流状态。

## 转换器契约

`FormatDetector` 可以检查受限长度的内容与提示。`Converter::probe` 是低成本的
适用性测试，不得执行实际转换。`Converter::convert` 只能生成 `Document`、
资源和诊断。包括 PDF 与多媒体适配器在内的所有格式实现都必须遵守此契约。

只有 `ProbeOutcome::NotApplicable` 允许注册表回退。探测成功后出现的错误是
权威错误。实现不得执行 Office 宏，并且必须将内嵌路径与压缩包视为不可信输入。

## 可选服务

`OcrEngine`、`Transcriber`、`AiProvider` 和 `TensorRuntime` 都是对象安全的
异步 SPI。引擎通过 `Services` 将可选服务传给转换器。
`AiProvider::capabilities` 必须准确声明已配置模型的能力；调用不可用能力时应
返回类型化错误。

视觉 OCR、图片描述、版面修复、表格与公式修复、音频转写和 Markdown
后处理均可独立配置 AI 模式。每项 AI 能力默认均为 `Off`。

`Engine::detect(DetectionRequest)` 只执行输入解析和格式检测，供 CLI 的
`formats detect` 使用；它不会探测或调用转换器。

## Markdown 渲染器

`MarkdownRenderer` 是统一 IR 到 GFM 的唯一边界。内置 `builtin.gfm` 会校验 IR
与所有嵌套图片的资源引用，规范化 LF，并按源顺序输出稳定字节。渲染结果是纯文本；
SPI 不允许渲染器写资源、追加诊断或改写 provenance。转换器诊断和引擎按深度优先
阅读顺序收集的 provenance 原样保留在 `ConversionResult` 中。

资源模式只决定 Markdown 表示：`extract` 生成与资源写出层共享的
`asset-<SHA-256(asset ID)>.<安全扩展名>` 有界 ASCII 文件名，
`embed` 生成 base64 data URI，`omit` 保留 alt 而不生成悬空链接。资源写出层必须
使用相同名字且不能在写出后单方面改名。CLI 在主产物或任何资源写入前预检全部
非空资源目标；稳定资源路径已经存在时，`rename` 与 `error` 都返回
`assetConflict`，只有 `overwrite` 会原子替换。`rename` 与 `error` 的每个精确目标
都使用原子 no-clobber 写入，因此预检后的竞态文件不会被覆盖；若竞态发生在一组
输出的中途，已完成的主产物或资源不会自动回滚。跨主产物与全部资源的事务式提交
由资源写出策略任务统一实现。

CLI 将文件系统路径按 POSIX、Windows drive 和 UNC 语法独立做词法规范化，并只在
相同 root/drive/share 内生成相对于 Markdown 基准目录的 percent-encoded URI path
reference。不同 root、drive 或 UNC share 稳定返回 `assetPathUnsupported`，不得输出
`C:/...` 自定义 scheme 或 `//server/...` 网络引用。原始 `%` 编码为 `%25`，渲染器
不会再次编码已经形成的 `%HH`。文件输出以 Markdown 文件父目录为基准，stdout 以
当前工作目录为基准。bundle 是自包含输出，渲染前固定使用 `assets` 前缀，归档内
`document.md` 的每个抽取资源 href 必须精确命中对应 ZIP entry，且不额外写外部资源。

## 执行上下文

`ConversionRequest` 与 `DetectionRequest` 都携带 `ExecutionOptions`。引擎为每次调用
创建一个 `ExecutionContext`，并把同一个上下文显式传给 `SourceResolver`、
`FormatDetector`、`Converter::probe`、`Converter::convert`、`OcrEngine`、
`TensorRuntime`、`Transcriber`、`AiProvider` 和 `MarkdownRenderer`。实现必须在读取循环、
解压循环、页面循环、模型批次和网络等待边界调用 `checkpoint`，异步等待应通过
`ExecutionContext::run` 包装；只在引擎入口检查一次不符合接口契约。

`CancellationToken` 是协作式、可克隆的取消句柄。总 timeout 从引擎接收请求时开始，
覆盖解析、检测、探测、转换、OCR、AI 与渲染，并以 `timeout` 稳定错误码失败；显式取消
使用 `cancelled`。取消、超时和完成以最后一次检查点线性化，成功完成事件发布后到达的
取消不会改写已经完成的结果。

阶段进度使用 `ProgressEvent` 和对象安全的 `ProgressListener`。总体进度以 basis points
表达并保持单调。OCR 与 AI 是转换期间可以交错出现的活动，而不是互斥的线性总体阶段。
监听器运行在隔离线程上；固定容量 mailbox 会合并同阶段更新，并在饱和时保留最新边界
与最终完成事件，因此慢监听器不会阻塞转换，监听器 panic 也不会穿透执行边界。回调期间
不持有进度状态锁，监听器可以安全地请求取消。接口不依赖特定异步运行时，也不创建
无界事件队列。

`ResourceLimits` 除格式专用限制外，还提供 `max_memory_bytes` 与
`max_temporary_bytes`。`ExecutionContext::reserve_memory` 使用 checked arithmetic 和
RAII guard；`temporary_file` 在写入时计费，并在成功、错误、取消、超时或预算超限后
删除临时产物。实现仍需使用格式专用预算，例如解压字节、条目、页数和资源大小；通用
内存预算只统计实现显式保留的内存，不声称代表进程 RSS。
提供者在分配大块输入副本、模型 tensor、解压缓冲或输出缓冲前必须调用
`reserve_memory`，并在需要磁盘暂存时使用 `temporary_file`；引擎在 SPI 边界的计费只是
补充防线，不能替代提供者内部检查。

检测候选携带置信度、稳定检测器 ID、证据和非致命诊断。用户显式候选始终优先，
其余候选按置信度、检测器优先级和稳定检测器 ID 排序；显式格式的置信度为 1。
检测器不能自行声明显式候选，置信度在引擎边界归一化。扩展名和 MIME 只构成提示，
不能压过更高置信度的 magic bytes 或容器结构证据。ZIP 探测只读取受限数量的目录
项和受限长度的 `mimetype` 内容；OLE 探测只检查有界的目录项区域，不提取宏或
内嵌对象。OLE 检测会验证 CFB header 并沿 DIFAT、FAT 和 directory chain 遍历，
只有 directory stream 中的流名可产生高置信候选；损坏或超限结构只产生带诊断的
低置信歧义候选。结构化文本探测最多检查 1 MiB UTF-8 前缀，可识别 HTML、XML、
RSS/Atom、JSON 和 Jupyter Notebook；不会猜测 CSV、纯文本或 Markdown。媒体容器
必须具备可识别品牌或 codec signature 才能获得高置信度，否则输出较低置信度与
歧义诊断。无 ID3 的 MP3 会验证 MPEG frame header 的版本、层、码率和采样率字段；
BMP 会验证 file/DIB header、声明大小、像素偏移和基本图像字段，不能只凭短签名获得
高置信度。HTML 探测可在有界前缀内跳过 BOM、空白、XML declaration 和注释，并以
ASCII 不区分大小写的方式识别 doctype 与 HTML/XHTML 根元素。

## 兼容性

错误文本用于描述问题，但不保证稳定；`ErrorCode::as_str()` 是稳定的机器接口。
可选后端缺失使用 `componentUnavailable`，与内部不变量错误分开。
提供者 ID、转换器 ID、节点 ID、模型包 ID 和线协议版本均为稳定标识符。枚举
允许增加新变体，因此使用方应保留默认分支。

CLI、未来 HTTP/SSE 服务和 Bundle 共用 `into_markdown` 导出的应用 DTO，不直接序列化
上述内部模型。版本、additive 兼容、解码资源预算和恶意输入规则见
[稳定数据传输契约](dto.md)。
