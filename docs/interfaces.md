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

JSON/XML 共用 `max_input_bytes`、`max_nesting_depth`、`max_field_bytes`、IR node/inline
上限及 ExecutionContext 内存预算。JSON detector 与 converter 先通过同一完整结构扫描器，
converter 再以显式容器栈施加重复键和 decoded string 策略，不建立递归 AST。XML 复用文本
转换器的 compact run decoder，维护 decoded UTF-8 boundary 到原始 byte boundary 的映射；
converter 交给 quick-xml 的只是解码文本，公开 provenance 始终回映到原始输入。XML 属性
由有界 start-tag scanner 与 quick-xml 属性顺序交叉校验，QName/value 各自保留精确原始 span。

HTML 转换器同样复用文本 decoder、compact byte mapping 与 `ExecutionContext`。HTML5 容错树
构造的节点位置无法唯一证明时，`SourceLocator` 使用整份输入的可靠包含范围而不猜测精确 span，
同时输出诊断。转换器不调用 `Services` 或 `SourceResolver`；relative URI 与已验证 base 的 join
仅产生引用数据，不能作为远程获取授权。

只有 `ProbeOutcome::NotApplicable` 允许注册表回退。探测成功后出现的错误是
权威错误。实现不得执行 Office 宏，并且必须将内嵌路径与压缩包视为不可信输入。

旧 Office converter 在 Core 内直接解析 CFB/OLE 与 DOC、PPT、XLS 的二进制记录，不经过
`NestedConversionRequest`、外部 worker 或规范化 OOXML。CFB reader 和三个格式前端共用
`ExecutionContext`、输入/内存/结构/表格/资源上限；损坏、加密和超限分别保持类型化错误。
恢复性降级必须产生结构化 diagnostic。`builtin.converter.legacy-office` 不读取插件目录、PATH
或系统 Office，也不调用 `Services`、`SourceResolver`、网络或子进程；宏、ActiveX、外部工作簿
和嵌入式可执行对象不会执行。

## PDF 提取与预算

Rust `ResourceLimits` 和请求 JSON 的 `options.limits` 共用
`max_pdf_page_objects: u32`（100,000）、`max_pdf_total_objects: u64`（10,000,000）
及 `max_pdf_layout_comparisons: u64`（12,000,000）。省略字段使用默认值，显式零值
失败。单页对象最高 10,000,000；Web 请求以默认值作为各字段上限。累计对象数检查
溢出，扫描超限错误附预算名称、实际值、上限和页码。最终 IR 限制独立生效。
版面比较预算按每次整份 PDF 版面重建计数，原生提取和 OCR 重建分别应用。

PDFium 边界保留严格便捷接口 `TextPage::plan_links` / `PlannedLinks::materialize`。
调用方可用 `plan_link_extraction(LinkPolicy, checkpoint)` 规划带策略的链接提取，
先为 `allocation_bytes()` 预留内存，再用 `materialize_with_checkpoint` 得到
`LinkExtraction { links, diagnostics }`。规划和物化使用相同枚举与分类，检查扫描数、
有效结果数、诊断容量、URI 分配及分类/几何变化；无进度、失效句柄和资源错误不会降级。
`LinkIdentity` 保留注释或网页来源及原始序号，网页链接还保留矩形序号。

Core 的 `best-effort` 恢复不可用链接矩形以及含内部 NUL 或无效编码的 URI，使用
`pdf.linkOmitted` 和页面 locator 记录并省略单个链接；规划阶段按最坏情况预留诊断容量。
`strict` 对这些链接失败。有限倒序和部分越界矩形在两种策略下均规范化并保留。
URI 长度上限、枚举异常、资源错误与规划变化保持硬错误，其他 PDF 对象继续执行原有
几何校验。

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

`core::canonical_external_asset_uri` 是 DTO、转换器和渲染器共享的 additive 公共安全
边界：仅接受经 `url::Url` 解析后字节级 canonical、无 userinfo/query/fragment 的绝对
HTTP(S) URI，并拒绝非法 percent escape。它只验证并保留审计引用，不授予网络访问。

`MarkdownRenderer` 是统一 IR 到 GFM 的唯一边界。内置 `builtin.gfm` 会校验 IR
与所有嵌套图片的资源引用，规范化 LF，并按源顺序输出稳定字节。渲染结果是纯文本；
SPI 不允许渲染器写资源、追加诊断或改写 provenance。转换器诊断和引擎按深度优先
阅读顺序收集的 provenance 原样保留在 `ConversionResult` 中。

Engine 在渲染后、任何 sink 写入或成功 checkpoint 提交前执行统一结果策略。Markdown
含可见非空白字符时是普通成功；只有转换器完整扫描其可见内容域并附带私有
`SourceContentEvidence::Empty`，空 Markdown 才能以 `complete` / `emptySource` 成功。
未被证明为空、但最终 Markdown 为空且没有可用的 asset-only 结构时返回
`ConversionError::EmptyContent`；为保持既有错误分类兼容，它的 `ErrorCode` 仍是
`malformed`，细分 `reasonCode` 是 `emptyContent`。只含受 IR 引用资源的结果使用
`assetOnly`：只有能表示每个资源 payload 或外部 URI 的结构化/资源目标可以成功；纯
Markdown、悬空 IR、以及缺少本地 payload 的 bundle 失败。普通执行、`convert_into`、可恢复 checkpoint fresh/resume、CLI
stdout、文件事务和 Web 发布共用这项判定，判空不会再次调用 converter。

`ConversionOutcome` 的诊断严重度契约可审计且不使用诊断码白名单：`Info` 只能描述无
正文内容损失的审计事实，全部诊断均为 `Info` 时仍是 `complete`；任何省略、替代或净化
必须由生产端发出 `Warning` 或 `Error`，结果为 `degraded`。例如 OCR 低置信区域、隐藏
slide、未知 RTF 控制、非线性 EPUB spine 章节，以及从正文、导航或 CSS 依赖链可达但
未证明无损映射的 EPUB 资源省略均为 `Warning`；孤立 manifest 项不表示内容损失。feed
源顺序、PDF 扫描页识别、MediaWiki 标题重定向以及 EPUB
inert rights 元数据保留 `Info`。

资源模式只决定 Markdown 表示：统一规划器在渲染前生成并冻结与资源写出层共享的
`asset-<完整 SHA-256(bytes)>.<MIME 权威扩展名>` 有界 ASCII 文件名，
`embed` 生成 base64 data URI，`omit` 保留 alt 而不生成悬空链接。资源写出层必须
使用相同计划且不能在写出后单方面改名。相同内容和规范化 MIME 按完整字节去重；
多个 `AssetId` 可映射到同一 URI，相同内容的 MIME 冲突稳定失败。CLI 在主产物或任何资源写入前预检全部
非空资源目标；稳定资源路径已经存在时，`rename` 与 `error` 都返回
`assetConflict`，只有 `overwrite` 会原子替换。`rename` 与 `error` 的每个精确目标
都使用原子 no-clobber 写入，因此预检后的竞态文件不会被覆盖。提交前完整 stage 和
fsync，并持久记录签名 journal、递增 generation、目标身份、备份与安装状态。每个
物理目标父目录由固定管理器 lease 保守互斥；lease 绑定父目录身份、事务 root 身份和
随机 nonce，所有查询、发布与移除均相对已认证目录 handle 完成，不依赖路径字节比较或
祖先扫描。相关输出开始前只通过目标父目录 lease 恢复相交事务：`committed` 之前恢复旧集合，之后验证并
完成新集合清理，所以 `overwrite` 只产生完整旧集合或完整新集合。回滚失败返回稳定
`rollbackFailed` 并保留 journal/备份；下一次恢复可继续已完成一部分的幂等步骤。
恢复成功后会在 `ExecutionContext` checkpoint 间有界重做预检；只重试精确内部恢复信号，
超限返回 `recoveryLimit`。该过程发生在 stdout 写入之前，不能导致 stdout 重复输出。
Unix 的所有目标变更绑定已认证目录 handle，使用相对 no-follow/no-replace 操作；跨
文件系统、符号链接与非 regular file 在任何目标变更前拒绝。Windows 输出事务返回
稳定 `componentUnavailable`，路径规划与 bundle 编码不受影响。stdout 的外部 extract
使用同一状态机，只把 stdout 自身视为不可回滚的流边界。

Bundle manifest 使用独立的 `schemaVersion: 2`，不改变结果 DTO、Document IR、
diagnostics 与 provenance 的 schema 1。每个物理资源只有一个 path；`id` 是按字节序
排序后的 canonical ID，`sourceAssetIds` 列出映射到该内容的所有 ID。reader 同时接受
manifest schema 1 和 2，并把 schema 1 条目归一成 `sourceAssetIds=[id]`。

CLI 将文件系统路径按 POSIX、Windows drive 和 UNC 语法独立做词法规范化，并只在
相同 root/drive/share 内生成相对于 Markdown 基准目录的 percent-encoded URI path
reference。不同 root、drive 或 UNC share 稳定返回 `assetPathUnsupported`，不得输出
`C:/...` 自定义 scheme 或 `//server/...` 网络引用。原始 `%` 编码为 `%25`，渲染器
不会再次编码已经形成的 `%HH`。文件输出以 Markdown 文件父目录为基准，stdout 以
当前工作目录为基准。bundle 是自包含输出，渲染前固定使用 `assets` 前缀，归档内
`document.md` 的每个抽取资源 href 必须精确命中对应 ZIP entry，且不额外写外部资源。

## 执行上下文

可恢复转换通过 `RecoveryStore::open`、`RecoveryStore::create_token` 和
`Engine::convert_recoverable` 显式启用。调用方应持久保存 token；状态端点可用
`RecoveryStore::inspect` 读取不含 payload 的版本化 `TaskCheckpoint`。恢复 token 是
32 个小写十六进制字符，解析时在任何文件系统访问前拒绝非规范值。checkpoint 不兼容、
损坏、版本未知、路径不安全或并发阶段冲突统一返回稳定 `ErrorCode::Recovery`，详细
`reason` 可区分 `invalidToken`、`incompatible`、`corrupt`、`unsupportedVersion`、
`unsafePath`、`conflict`、`limit` 与 `io`。

`inspect` 只读取不可变阶段文件末尾的固定 4 KiB 元数据块；payload 摘要在真正恢复时
验证。Unix 最终 store root 必须由当前 effective user 拥有，且 group/other 无任何
权限；祖先目录可以公开。`open` 对已有非私有目录返回 `unsafePath`，不会替调用方
执行 `chmod`。所有状态操作绑定打开时的目录 handle 与 identity，并在操作边界用该
handle 的 `fstat` 重验 owner/mode；打开后放宽权限会 fail closed。token 级持久锁保证
并发调用只产生一个持久结果。checkpoint 临时写入使用同一
`ExecutionContext` 的 temporary budget；完整读取在 owned serde 前先执行 2 GiB 大小、
JSON depth/width/value 预检并预留内存，depth 边界可容纳公共 IR 的最大合法深度。资源采用
声明解码长度的规范 padded base64 wire；编码、请求资源上限和共存峰值校验通过后
才分配解码缓冲。尚无经审计相对目录 primitive 的平台会稳定返回
`componentUnavailable`。

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
可表示且尚未到期的 deadline 使用独立 timer 唤醒所有已注册 waiter；timer 线程无法创建时，
上下文及其所有 clone 的 checkpoint 和异步等待稳定返回
`componentUnavailable`（component 为 `deadline-timer`），不会把请求伪装成无 deadline，
也不会等待原 deadline 才暴露失败。该初始化错误优先于同一上下文随后观察到的显式取消。

阶段进度使用 `ProgressEvent` 和对象安全的 `ProgressListener`。总体进度以 basis points
表达并保持单调。OCR 与 AI 是转换期间可以交错出现的活动，而不是互斥的线性总体阶段。
监听器运行在隔离线程上；进度状态锁覆盖序号分配和入队，dispatcher 还会丢弃旧序号及
终态后的事件。固定容量 mailbox 会合并同阶段更新，并在饱和时保留最新边界与最终完成
事件，因此慢监听器不会阻塞转换，监听器 panic 也不会穿透执行边界。回调期间
不持有进度状态锁，监听器可以安全地请求取消。接口不依赖特定异步运行时，也不创建
无界事件队列。mailbox 的关闭谓词与队列共用同一把锁；`Completed` 成功入队会同时关闭
发布端，worker 在退出前 drain 所有已接受事件。未发布终态就释放上下文时也会关闭并
drain 已入队事件。上下文释放只通知关闭，不等待慢回调；worker handle 由有界的进程级
回收器异步 join，回收器不可用或已满时安全 detach。永久不返回的回调必然继续占用它的
专属 worker 和 listener，无法由安全 Rust 强制终止，但不会阻塞转换或上下文释放路径。

`ResourceLimits` 除格式专用限制外，还提供 `max_memory_bytes` 与
`max_temporary_bytes`。`ExecutionContext::reserve_memory` 使用 checked arithmetic 和
RAII guard；`temporary_file` 在写入时计费，并在成功、错误、取消、超时或预算超限后
删除临时产物。实现仍需使用格式专用预算，例如解压字节、条目、页数和资源大小；通用
内存预算只统计实现显式保留的内存，不声称代表进程 RSS。
source buffer 按已初始化的逻辑 payload bytes 计费，并用 `try_reserve_exact` 避免实现主动
请求额外增长余量；allocator 的 size-class 舍入、元数据及其他 RSS 开销不属于这项
协作式逻辑预算。Rust 库的确定性 `ResourceLimits::default()` 使用 2 GiB；本地 CLI/Desktop
按调用开始时的一次机器快照选择批处理共享预算，公式与探测缺口规则见 [CLI 内存策略](cli.md)。
未显式配置时，本地资产额度从最终共享预算派生；各资产字段的显式值独立优先。
Web 安全上限保持独立。`--jobs` 不会复制该预算；当前
converter preflight 需要完整请求 envelope 时，批调度器会等待前一项的结果提交后再放行。
source scratch 会在共享转换前释放，不再叠加 64 KiB scratch。
图片和 OCR 源图不使用独立的固定宽高或像素阈值：格式解析器先认证非零尺寸并用 checked
arithmetic 计算 decoded bytes、stride 与 working set，再由 `max_decompressed_bytes`、
`max_memory_bytes`、资产额度和实际可用 credit 决定是否准入。decoder 仍绑定已认证的精确头部
尺寸和 allocation budget。OCR recognizer 的区域、crop、归一化宽度、张量及输出限制属于模型与
结构契约，继续独立执行。

`Engine::convert_into(request, sink)` 以 64 KiB 有序块向 `ArtifactSink` 写出 Markdown 和
资产，并返回 `ConversionSummary`。它避免前端再创建一份完整序列化缓冲；兼容接口
`convert()` 仍返回聚合的 `ConversionResult`，适合小文件或需要完整 IR 的调用方。CLI 的
Markdown、IR JSON、result JSON 与 bundle 均先流式写入受 `max_temporary_bytes` 约束的
同文件系统临时文件，再由输出事务分块哈希、同步并原子提交。
大型 XLS、XLSX 与 XLSB 工作簿在完整表格 IR 会超过统一节点上限时，转换器改用有界的 2048 行
TSV page blocks；页块保持工作表、行和列顺序，Tab、换行、反斜杠和反引号采用可逆转义，
渲染器对该受约束块使用专用内存计划。最终 artifact 仍是一个 Markdown 文件，结果以
`spreadsheet.largeTablePaged` 标记为 degraded，避免把表格语义简化误报为 complete。
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

HTTP(S) resolver 由独立的 policy、DNS、exact-IP connect、TLS、HTTP/1、body 与 error
模块组成；Provider 通过窄 transport API 复用相同的 host/IP/DNS/connect/TLS 边界，不复制
另一套 SSRF 或证书策略。remote resolver 自身只装配 options、redirect 与 source metadata。
成功结果以 `ResolvedSource` 携带 final decoded `Arc<[u8]>` 的 exact memory reservation；
`SourceMetadata.uri` 是移除 query/fragment 的最终 canonical URL；兼容的
`ResolvedSource::resolution_metadata()` sidecar 携带同样脱敏的有序 redirect 记录，避免给
公开 `SourceMetadata` struct literal 增加必填字段。`name` 与 `media_type` 仅为经过严格语法
与 portable-name 校验的检测提示。

resolver 若还保留 URL、policy 或 redirect/source metadata，可在私有 `ResolvedSource` wrapper
上附加独立 retained-metadata lease；Engine 的 source-byte exact resize 不会缩减该 lease，二者
持续到 terminal event 后共同释放。MediaWiki resolver 使用此路径，并保留 transport 返回的真实
final API URL；它先验证 transport 的严格 `application/json`，再在同一 base MIME 上附加仅供进程内
使用的 authenticated-resolver 参数。HTTP transport 会剥离服务器提供的全部 Content-Type 参数，
因此普通 HTTP 响应不能伪造该 identity。专用 detector/probe 只有在 identity、标准 API endpoint 与
JSON object shape 同时成立时才接受 Wikipedia；普通 `/w/api.php` JSON 仍进入通用 JSON converter。

检测候选携带置信度、稳定检测器 ID、证据和非致命诊断。用户显式候选始终优先，
其余候选按置信度、检测器优先级和稳定检测器 ID 排序；显式格式的置信度为 1。
检测器不能自行声明显式候选，置信度在引擎边界归一化。扩展名和 MIME 只构成提示，
不能压过更高置信度的 magic bytes 或容器结构证据。ZIP 探测只读取受限数量的目录
项和受限长度的 `mimetype` 内容；OLE 探测只检查有界的目录项区域，不提取宏或
内嵌对象。OLE 检测会验证 CFB header 并沿 DIFAT、FAT 和 directory chain 遍历，
只有 directory stream 中的流名可产生高置信候选；损坏或超限结构只产生带诊断的
低置信歧义候选。RTF magic 只有在输入开头严格匹配 `{\\rtf`、至少一位版本号和合法 delimiter 时才形成
高置信候选；`.rtf`/MIME 提示本身不能使不符合该 header 的普通文本通过 converter probe。
converter crate 还提供窄作用域的 `convert_rtf_bytes(bytes, options, context)` 给 MSG 等已解压
容器正文复用；该入口不接受 `Services`，沿用调用方同一个 `ExecutionContext`、limit、取消、
provenance 和 live-memory lease，不能重置预算或获得网络能力。
HTML、XML 与 RSS/Atom 探测最多检查 1 MiB UTF-8 前缀。JSON 与
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

## TaskStore API

公开 façade 重导出 `TaskStore`、`BusyControl`、`TaskId`、`NewTask`、`TaskRecord`、
`TaskTransition`、`TaskCursor`、`ReconcileSummary` 与 closed enums。所有 DTO 有界：progress
为 `0..=1_000_000` 整数，diagnostic 最多 64 个，artifact 最多 128 个，page 最多 100 行；
ID/token/storage key/digest/fingerprint 在文件访问或复杂分配前验证为固定长度小写 hex。
`InputReference` 的两个 fingerprint 必须与 RecoveryStore checkpoint 精确匹配；输入 bytes、
checkpoint/result bytes 均不进入 SQLite。`TaskId` 自定义
deserialize 也执行同一验证，不能绕过 constructor。

配置 JSON 使用 `deny_unknown_fields`，只允许 output format、本地 OCR、layout 三项非秘密
设置。Provider、Authorization、环境变量、URL 和自由文本 diagnostic 没有持久化字段。
`create`、`get`、`list`、`transition`、`set_pinned`、`reconcile`、`backup` 都是同步接口；
Tokio/Axum handler 必须用 `spawn_blocking` 或等价 pool。错误以 `TaskStoreError` 区分 unsafe
path、unknown schema、corruption、limit、CAS conflict、busy deadline、cancel、I/O 与
unsupported platform，损坏 enum/JSON 不会降级为默认值。

`Succeeded` 表示 RecoveryStore 的 complete-result checkpoint metadata durable，并不声称外部
Markdown/asset 已发布；artifact index 可独立为空。Failed、Interrupted、Cancelled terminal
transition 禁止新增 artifact。进度只允许单调增加；reconcile 对 Converted/Succeeded 分别修复
最低 `900_000`/`1_000_000`，其他 terminal 状态保留此前进度。

隔离 OCR 请求额度取用户配置、认证 capability 上限和已取得工作租约可用量的最小值。
宿主协议缓冲单独保留，provider 与模型子进程共同受该请求额度约束；Unix 检查所属进程组，
Windows 使用 Job Object 总内存上限。`OcrRecognitionMemory` 仅表示受控的识别阶段私有
逻辑预算拒绝，`code()` 保持 `resourceLimit`，`reason_code()` 为 `ocrRecognitionMemory`。
公共 API 的调用方可据此区分识别额度与全局资源失败，禁止通过错误文本判断降级。
可选图片的其他识别阶段资源限制通过稳定 limit 名称分类：本地宽度、crop、张量、输出、
regions 与 decoded 限制，以及进程边界的 `ocrWidthLimit`、`ocrPixelLimit`、
`ocrTensorLimit`、`ocrStructureLimit`。全局内存、资产、provider frame、协议、进程、取消
和超时保持终止语义。

Linux 进程组观测累加当前 `smaps_rollup` 的 PSS、SwapPss 及独立 huge-page 字段，
共享普通页面按比例计入，历史进程峰值单独用于测量报告。已退出进程的 ENOENT/ESRCH
视为离开进程组；其他观测失败终止请求并报告进程异常。内存超额错误包含观测字节数及额度。
PDF 原生页沿用逐页预检降级：必须有本页原生正文，扫描页标记会阻止降级。
通用容器增强器的全局预检与工作租约不足保持失败，逐图片运行期恢复由识别错误分类决定。
