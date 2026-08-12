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

TXT 的 `FormatHint.charset` 会由 Engine 复制到请求级 `ConversionOptions.text.charset`，
供不改变 Converter SPI 的实现读取。显式字符集权威且必须在确定 allowlist 内规范化；
默认 `TextDecodingMode::Strict`。`Replace` 只能在每段恢复都附带稳定 diagnostic code、
encoding 与原始 byte range 时继续。转换器必须在 resolver 之后再次执行输入大小检查。

CSV/TSV 与 TXT 共用 converters 内部的安全解码及原始字节映射。新增的
`ConversionOptions.delimited_text.header` 和 `ragged_rows` 均带 serde 默认值，旧请求
分别解析为 `auto` 与 `strict`。表格预算由 `ResourceLimits` 的 `max_table_rows`、
`max_table_columns`、`max_table_cells` 和 `max_field_bytes` 控制。
字节映射以一次 decoder 输出序列为 span，再按连续的“解码 UTF-8 宽度/原始编码宽度”
run 压缩；ASCII identity 通常只占一个 run。Big5 等把一个原始序列展开为多个 Unicode
scalar 时，这些 scalar 共用一个 span，任一重叠子范围均覆盖完整原始序列。随机
byte-range 查询对 run 起点做二分，TXT 的顺序分行则使用单调游标。

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

本地路径和 stdin 的阻塞读取运行在进程级固定工作者中：路径使用四个工作者与容量 32
的有界队列，stdin 使用独立的单工作者与容量 1 的队列，避免一个不可中断的 stdin
读取占满路径池。调用 future 只等待共享结果，因此 deadline 或取消可以立即返回；
工作者在系统调用返回后观察同一上下文，停止读取并丢弃已无人等待的结果。stdin 的
底层阻塞 `read` 本身无法被协作取消：调用方超时返回后，单个 stdin 工作者仍会保留该
请求的上下文、listener 和已有 reservation，直至 `read` 返回或 EOF；随后才释放。
这项进程级成本限制为一个工作者和一个排队请求，在它们被占用时，更多 stdin 请求稳定
返回 `resourceLimit`，而不会创建额外线程。路径池也采用固定线程与有界队列；工作者
不可用返回 `componentUnavailable`。每次读取最多请求剩余输入预算加一个字节，并在
scratch 或 source buffer 分配前执行 checked 累加和协作式内存预留。

路径打开不能依赖规划时的一次 symlink 检查。Unix resolver 使用 `O_NOFOLLOW`，打开
后要求 regular file，并核对紧邻打开前的设备与 inode；Windows 使用
`FILE_FLAG_OPEN_REPARSE_POINT` 单次打开权威句柄，立即从该句柄拒绝 reparse attribute
与非 regular file，并通过 `GetFileType` 要求 `FILE_TYPE_DISK`；安全 wrapper 将
`FILE_TYPE_UNKNOWN` 与 `GetLastError` 组合成明确成功或 I/O 失败。打开前的路径策略拒绝
`\\.\`、`\\?\GLOBALROOT` 等设备 namespace 及 `NUL`、`CON`、`COM1` 等保留设备
组件，同时允许普通 `\\?\C:\...` 和 `\\?\UNC\...` 长路径。后续读取始终使用
同一句柄；它没有路径 metadata 快照与二次 open 之间的身份窗口，也不会先跟随 reparse
target。其他平台若没有经过审计的 no-follow 策略，则返回 `componentUnavailable`。
规划阶段仍用于尽早给出 `symlinkDenied`，resolver 的 handle 级策略才是最终读取边界；
它不承诺锁定更早规划阶段看到的普通文件版本。

`CancellationToken` 是协作式、可克隆的取消句柄。总 timeout 从引擎接收请求时开始，
覆盖解析、检测、探测、转换、OCR、AI 与渲染，并以 `timeout` 稳定错误码失败；显式取消
使用 `cancelled`。取消、超时和完成以最后一次检查点线性化，成功完成事件发布后到达的
取消不会改写已经完成的结果。
library 的零 `Duration` 表示立即 deadline；CLI 和配置拒绝零值。若极大 `Duration`
无法转换为平台 `Instant`，则饱和为无 deadline，不能回绕成立即 timeout。

阶段进度使用 `ProgressEvent` 和对象安全的 `ProgressListener`。总体进度以 basis points
表达并保持单调。OCR 与 AI 是转换期间可以交错出现的活动，而不是互斥的线性总体阶段。
监听器运行在隔离线程上；进度状态锁覆盖序号分配和入队，dispatcher 还会丢弃旧序号及
终态后的事件。固定容量 mailbox 会合并同阶段更新，并在饱和时保留最新边界与最终完成
事件，因此慢监听器不会阻塞转换，监听器 panic 也不会穿透执行边界。回调期间
不持有进度状态锁，监听器可以安全地请求取消。接口不依赖特定异步运行时，也不创建
无界事件队列。

`ResourceLimits` 除格式专用限制外，还提供 `max_memory_bytes` 与
`max_temporary_bytes`。`ExecutionContext::reserve_memory` 使用 checked arithmetic 和
RAII guard；`temporary_file` 在写入时计费，并在成功、错误、取消、超时或预算超限后
删除临时产物。实现仍需使用格式专用预算，例如解压字节、条目、页数和资源大小；通用
内存预算只统计实现显式保留的内存，不声称代表进程 RSS。
source buffer 按已初始化的逻辑 payload bytes 计费，并用 `try_reserve_exact` 避免实现主动
请求额外增长余量；allocator 的 size-class 舍入、元数据及其他 RSS 开销不属于这项
协作式逻辑预算。默认 `max_input_bytes` 为 512 MiB、`max_memory_bytes` 为 1 GiB 时，
scratch 会在共享转换前先释放，所以最坏 `Vec`/`Arc` 双 payload 峰值恰好落在 1 GiB
边界内，不会再叠加 64 KiB scratch。
提供者在分配大块输入副本、模型 tensor、解压缓冲或输出缓冲前必须调用
`reserve_memory`，并在需要磁盘暂存时使用 `temporary_file`；引擎在 SPI 边界的计费只是
补充防线，不能替代提供者内部检查。
内置 source resolver 在 scratch 与 source buffer 分配前预留预算，并让
`ResolvedSource` accounting wrapper 携带唯一 RAII reservation 穿过 resolver 到引擎的
handoff。`Vec` 转换为共享 bytes 前按可能复制的峰值预留，转换结束后只释放旧 buffer
对应部分。引擎只接受 context identity 与实际输入长度都完全匹配的 reservation，跨请求
或不足的 reservation 不能绕过预算。wrapper 不进入
`ResolvedInput` 的公开布局，既有第三方 resolver 的两字段 struct literal 及 `resolve`
实现保持可编译；对象安全的 `resolve_accounted` 默认方法把旧输出直接包装为未计费结果，
不复制 source bytes，Engine 随后补计。提供者若要消除自身分配与 Engine 补计间的窗口，
可覆写该方法并从分配前携带 reservation。Engine 持有 wrapper 到检测或转换整体结束。
内存输入的 `Arc` 虽然不复制，仍按其完整长度计入当前请求。

检测候选携带置信度、稳定检测器 ID、证据和非致命诊断。用户显式候选始终优先，
其余候选按置信度、检测器优先级和稳定检测器 ID 排序；显式格式的置信度为 1。
检测器不能自行声明显式候选，置信度在引擎边界归一化。扩展名和 MIME 只构成提示，
不能压过更高置信度的 magic bytes 或容器结构证据。ZIP 探测只读取受限数量的目录
项和受限长度的 `mimetype` 内容；OLE 探测只检查有界的目录项区域，不提取宏或
内嵌对象。OLE 检测会验证 CFB header 并沿 DIFAT、FAT 和 directory chain 遍历，
只有 directory stream 中的流名可产生高置信候选；损坏或超限结构只产生带诊断的
低置信歧义候选。HTML、XML 与 RSS/Atom 探测最多检查 1 MiB UTF-8 前缀。JSON 与
Jupyter Notebook 使用带 checkpoint 和 nesting 上限的非递归状态机扫描完整 resolved
bytes；采样边界处无论结构开放还是恰好闭合都不会提前定案，完整结构后的非空白尾部
使 JSON 判定失效。至少三行、字段数一致、quote 合法且具备表头/数字数据类型证据的
逗号或 Tab 分隔启发式会保留 planned CSV/TSV 候选，防止普通文本回退抢占；它不会执行
CSV/TSV 转换，也不会把两行散文或 Markdown 当作表格。
媒体容器必须具备可识别品牌或 codec signature 才能获得高置信度，否则输出较低置信度与
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

公共 SPI 的兼容性由独立 consumer target 验证，而不是只依赖定义 SPI 的 crate 单测。
`ResolvedInput` 的 `bytes` 与 `metadata` 两字段 struct literal、请求构造器以及
`SourceResolver::resolve_accounted` 的默认适配器属于受保护的源码兼容面。公共 DTO
刻意不实现通用 serde trait；调用方必须使用带 schema 和预算的 `to_json`、
`from_json` 或对应 writer 方法。Cargo 与 Bazel 对这两类边界运行等价的编译契约。
