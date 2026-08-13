# 规划格式矩阵

运行时的权威列表以 `into-md formats` 输出为准。DOCX/DOCM、TXT、Markdown、HTML、CSV、TSV、JSON、XML、IPYNB 状态为
`available`，其余尚未
实现的条目保持 `planned`。

| 类别 | 格式 |
| --- | --- |
| 文档 | PDF；DOC/DOCX/DOCM；PPT/PPS/POT/PPTX/PPTM/PPSX/PPSM；XLS/XLSX/XLSM/XLSB；ODT/ODS/ODP；RTF；EPUB |
| 文本与数据 | TXT；Markdown；HTML；CSV/TSV；JSON；XML；RSS/Atom；IPYNB |
| 图片 | PNG；JPEG；TIFF；WebP；BMP |
| 音频 | WAV；MP3；M4A；FLAC；OGG |
| 视频 | MP4；MOV；MKV；WebM，通过具备相应能力的 AI 提供者处理 |
| 容器与消息 | ZIP；Outlook MSG |
| 远程来源 | HTTP(S)；Wikipedia；RSS；YouTube |

## HTML

HTML 转换器使用固定版本 `html5ever 0.39.0` 执行 HTML5 容错树构造，并以确定性规则选择
正文。非空的 `main`、`article` 与 `role=main` 候选按固定文本量、链接密度、段落和标题权重
评分；document order 用于打破同分。没有可用显式候选时降级到 `body` 并产生
`html.mainContentFallback`。`nav`、`aside`、`footer`、导航/广告/弹窗/推荐区域以及
`hidden`、`inert`、`aria-hidden=true` 内容不会混入正文；仅剩这些区域时稳定返回无可见正文。

标题、段落、行内样式、链接、图片、列表、带 rowspan/colspan 的表格、`pre`/`code`、分隔线、
title/author/description/lang 等 metadata 进入统一 IR。HTML5 tree builder 可能合成或重挂节点，
无法唯一证明节点级原始位置时，节点 provenance 只保存整份输入的可靠包含 byte range，并产生
一次诊断；不会用字符串搜索猜测重复标签、实体或隐式节点位置。

编码统一复用 TXT 安全 decoder。BOM 与显式 charset 优先；安全子集只在原始输入前 1024 bytes
扫描 `<meta charset>`，未知编码稳定拒绝，冲突产生诊断，显式错误不尝试其他编码。首个有效
HTTP(S) `<base>` 可与受信 source URI 解析相对引用；userinfo、query、fragment、localhost、
私有/回环/链路本地 IP 与非 HTTP(S) base 均拒绝。解析 URL 只是数据变换，不授予网络权限。
图片只有在解析结果已经严格等于 `canonical_external_asset_uri` 时，才作为 bytes 为空的
external-only audit Asset 返回；转换器不会 fetch，未来获取仍必须经过显式授权的
`SourceResolver`。

`script`、`style`、`template`、`noscript` 及表单/UI 内容整体忽略；SVG/MathML 整体降级为
不可执行代码文本并诊断，其内部链接和图片不进入资源。解析、正文选择和资源处理不执行代码、
不解释 CSS、不会调用网络服务。

## Jupyter Notebook

IPYNB 转换器严格解析 nbformat 4 JSON，不执行 cell 代码、JavaScript、HTML，也不读取
附件路径或获取远程资源。Markdown、code、raw cell、执行计数、cell/notebook/output metadata、
stream、error、display data、execute result 与 update display 按源顺序进入统一 IR；代码语言
来自 `metadata.language_info.name`。Markdown 内容复用内置 Markdown 转换器，最终仍只由中央
GFM renderer 输出。nbformat 4.5 起 cell ID 必填、唯一并遵循官方 1–64 个安全字符约束；
较早 minor 中出现的 ID 也按同一规则校验。execute-result count 与 transient display ID 作为
稳定 namespaced metadata 保留，重复 update-display 仍保持源序，不执行回写。

MIME bundle 使用固定优先级：PNG、JPEG、GIF、WebP，随后是 Markdown、plain text、HTML。
图片同时校验 MIME、严格 base64、data URI 前缀、解码预算和文件签名；附件名禁止路径分隔符、
点路径与控制字符。Markdown attachment 只在 parser 识别出的 exact image URI target 上绑定，
不会改写 prose 或 code；missing reference 和当前 IR 无法表达的 attachment link 稳定拒绝，
inline attachment image 因统一 IR 仅支持 block image 也稳定拒绝，且成功结果不会保留内部
placeholder URI 或孤立 attachment asset；未引用附件不解码。raw cell attachments 则按源序保留为相邻 asset 节点。HTML 只保存为
`html` fenced code 并产生诊断，不依赖 HTML 转换器。
ANSI escape 会删除，TAB/LF/CR 之外的控制字符会替换并诊断。无法安全表示的 MIME bundle
输出明确占位与诊断；不支持的附件稳定拒绝，绝不伪造成可访问资源。

Notebook 使用单次 strict seeded JSON parse，在分配 DOM 时直接拒绝重复 key、过深、过宽或
超大字段；不会先构造并丢弃通用 JSON IR。DOM、字符串、decoded assets 与最终 IR 的共同存活期
由一笔保守请求 reservation 覆盖，string-array 按聚合 decoded UTF-8 长度检查并 fallible reserve。
嵌套 Markdown 合并前递归累计 block/list/table/inline，metadata、diagnostic 与 assets 同样受全局
计数或内存预算约束。组合字段与控制字符清洗后的最终 UTF-8 也重新执行 checked/fallible 字段
预算。PNG chunk/CRC/IEND、GIF block/LZW/trailer、WebP RIFF/chunk/codec 与 JPEG marker/scan
结构均有界验证，并要求必要顺序/唯一性及容器与 bitstream 尺寸一致；随后在预留完整 decode
working set、设置 decoder width/height/allocation limits 后实际解码全部像素并复核尺寸。损坏 codec
payload、截断图片与尺寸炸弹都不会成为 Asset。

## DOCX 与 DOCM

Word Open XML 转换器离线解析样式标题、富文本、编号列表、表格、关系链接、图片、脚注、
页眉页脚、批注、字段和 OMML 公式，并统一写入 Document IR 后交由中央 GFM 渲染器输出。
DOCM 的 VBA 部件只在 ZIP 目录中识别，内容永不解压、加载或执行；结果包含明确诊断。
解析在分配和解压前执行输入、条目数、解压总量、单项/总资源、XML 深度、IR 节点和行内
节点预算。加密的 OOXML/OLE 包返回 `encrypted`；损坏 ZIP/XML、重复或越界部件名、重复关系、
DTD/实体及逃逸包根的内部关系均 fail closed。外部关系只保留为 URI，不发起网络访问。

## Markdown 与 GFM

Markdown 转换器使用固定版本的 `pulldown-cmark 0.13.4`，启用 CommonMark、GFM 表格、
删除线、任务列表、脚注与 GitHub blockquote 类型解析。ATX/setext 标题、段落、软换行和
硬换行、强调、链接与 autolink、嵌套有序/无序/任务列表、fenced/indented code、表格、
脚注定义和引用都会进入统一 IR；fenced code 的首个 info-string token 保存在 language。
混合普通项与任务项的源列表按连续 marker 类型拆成相邻列表节点；有序普通区间保留对应
起始序号，嵌套内容仍留在原列表项内。GFM 表格的每列 left/center/right/none alignment
直接保存在 Table IR；schema v1 中缺少 alignment 字段的旧 JSON 按全 none 解码。
reference link 与脚注由解析器跨全文解析，允许定义位于引用之后；重复定义遵循 first-wins，
同时产生带原始字节范围的诊断。

Markdown 规范输入固定为 UTF-8，允许 UTF-8 BOM 且 BOM 不进入正文。默认严格拒绝非法
UTF-8；只有显式 replacement 解码策略才插入 U+FFFD 并继承共享文本解码器的原始字节诊断。
块节点的 provenance 保存源编码半开 byte range。当前 IR 没有独立的行内 provenance 字段，
因此行内内容由其最小包含块的范围覆盖，不伪造字符位置。

当前 IR 没有 blockquote 与 raw HTML 专属节点。blockquote 原始片段保存为 language 为
`markdown-blockquote` 的代码块；HTML block 保存为 `html` 代码块。局部、严格白名单且
正确嵌套的 renderer 格式标签序列只恢复为 InlineMark；其他 inline HTML 保存为 inline
code 并产生明确降级诊断，raw HTML token 从不进入 CommonMark parser frame。`script`、`style` 及事件属性不会执行，也不会作为可执行
HTML 输出。独立段落中的绝对 HTTP(S) 图片以 `externalUri` 和空 bytes 的 external-only
Asset 进入 `Block::Image`，extract/embed 直接渲染经校验的原 URI，omit 只保留 alt；转换器
始终不联网。外部图片 URI 禁止 userinfo、query、fragment 和危险 scheme。inline 图片因现有
IR 只有 block image 而明确诊断并降级为链接；相对/fragment 图片不读取，data URI 不解码，
均保留 alt 与 target 后诊断降级。不会创建空 bytes 且没有 `externalUri` 的假资源，也不会
绕过统一 AssetPlan 校验。result-json 保留 external-only Asset；portable bundle 要求资源
具有可携带 bytes，因此对这类结果返回稳定的 `bundleAssetMissingContent`。
远程 SVG 仍不会由转换器获取或解析，并额外产生 active-content 风险诊断；Markdown
消费者若主动打开该 URI，可能自行发起网络请求或执行其安全模型允许的 SVG 内容。

内容自动检测只在多项明确结构、完整 fence 或 GFM table separator 构成强证据时产生
Markdown 候选；普通散文和单个偶然 marker 仍由 TXT 处理。扩展名、`text/markdown` MIME
与显式格式提示继续走统一 hint 优先级。解析使用 offset event iterator，定期执行 execution
checkpoint，并在构造 IR 前限制输入、事件深度、块数、行内数和请求内存。转换器在构造
parser 前按固定开销、输入字节、受输入与 IR 上限共同约束的事件工作单元及配置深度，预留
一笔确定性的 parser 逻辑工作预算。这是 `ExecutionContext` 的协作式逻辑计费，不代表
第三方 parser 的 allocator capacity、元数据或进程 RSS。第三方 Cow 在进入本项目拥有的
IR 时按其逻辑内容计费一次；转换器主动增长的 Vec/String 按请求的 capacity delta 计费。

## TXT 与字符集

TXT 转换器支持无 BOM/有 BOM UTF-8、带 BOM UTF-16LE/UTF-16BE，以及显式或自动
检测的 `windows-1252`、`gb18030`、`big5`、`shift_jis`。显式标签会做大小写、连字符
和已记录别名规范化，allowlist 之外的标签稳定拒绝。BOM 不进入正文，显式标签与 BOM
冲突时不会猜测。

无 BOM 自动检测使用至多 64 KiB 解码样本选择 allowlist 编码。带 BOM、无 BOM 与传统
字符集输入均按实际字符集增量解码完整内容并应用同一安全规则；除 TAB、LF、CR 外，
NUL、C0、DEL 与 C1 中任一
Unicode control 都会使自动 probe 返回 `NotApplicable`，包括位于 64 KiB 样本之后或
由多字节序列解码得到的控制字符。BOM 本身不会无条件形成高置信候选；样本尾部的截断序列仍由
转换阶段返回稳定编码错误，带 BOM 的二进制伪装则不作为 TXT 候选。

JSON、XML、HTML 等结构化候选先于普通文本。JSON 使用非递归词法与结构状态机扫描完整
输入，字符串 escape/Unicode、number、literal 和 `{}`/`[]` nesting 都必须合法；`\u`
形式的 UTF-16 high surrogate 必须紧邻一个 low-surrogate escape，lone low surrogate、
错误配对和输入结尾的 high surrogate 均使 JSON 判定失效，转义反斜杠后的 `uD800` 只按
普通文本处理。扫描过程定期 checkpoint，并对 nesting 执行 checked 上限。采样边界处的 `valid-open` 或
`complete` 只是中间状态，检测器继续读取完整 resolved bytes；只有完整结构及其后纯
空白形成 JSON 候选，闭合结构后的非空白尾部可继续按 TXT 评估。

CSV/TSV 自动检测至少需要三行非空记录、RFC 风格 quote 语法、一致且不少于两列，
以及至少一列呈现非数字表头与全数字数据的类型差异；两行逗号或 Tab 散文仍按 TXT
评估。扩展名、MIME 或显式格式提示对 CSV/TSV 具有权威性；单独的字符集提示仍按文本
解释。已知二进制 magic 也不会降级为 TXT。

自动检测与 converter probe 使用和转换相同的完整逻辑记录解析器，字段内的 CRLF、LF、
CR 不会被误计为新记录。强证据忽略空记录；因此显式 `pad` 可恢复含空记录的自动候选，
而 strict 策略仍在转换阶段返回权威的不等宽错误。

## CSV 与 TSV

CSV 使用逗号，TSV 使用 Tab；分隔符不从内容猜测后互换。解析器接受 CRLF、LF、CR、
外围双引号、doubled quote、字段内换行、尾随空字段、空记录与 BOM，不执行 trim、注释
或公式。表格默认以首条记录宽度为准，不等宽记录返回稳定 malformed；`ragged_rows =
"pad"` 只补齐较短记录并输出 `delimited.raggedRecordPadded`，较宽记录仍拒绝。

表头默认使用保守启发式：首行非空且唯一，并至少有一列呈“文本标签、后续全为数字”；
`always` 与 `never` 可覆盖。每个单元格内容节点的 provenance 使用原始编码的半开 byte
range，范围包含外围引号和 doubled quote 原始字节；`locator.cell` 保存零基行列。
Table 节点覆盖全部原始记录。转换输出始终为矩形 Table IR，GFM 转义只由中央渲染器完成。

## JSON 与 XML

JSON 转换器实现 RFC 8259 的对象、数组与标量。对象属性保持源顺序；重复键会稳定拒绝，
避免不同消费者采用 first-wins 或 last-wins 造成歧义。数字以原始 lexeme 进入 Inline Code，
不经浮点转换，因此任意长度整数和指数形式不会丢失精度。对象与数组容器按直接 member 名或
数组索引生成层级标题，标量按源序形成段落或代码块；所有内容仍先进入统一 IR。字符串严格校验 escape、
控制字符与 UTF-16 surrogate pair。JSON 接受无 BOM 或 UTF-8 BOM 的 UTF-8，BOM 不进入内容，
provenance 均映射到原始输入半开 byte range。无格式提示时，忽略 RFC 空白后完整匹配 RFC 8259
的对象、数组、`true`、`false`、`null`、数字或字符串均确定为 JSON；例如 `true`、`123` 与
`"x"` 不再作为 TXT。扩展名、MIME 或显式格式提示仍优先选择 JSON converter。

XML 转换器接受 UTF-8、UTF-8 BOM、UTF-16LE/BE BOM，以及带 XML magic 的无 BOM UTF-16
声明输入。声明必须与实际编码一致；其他编码稳定拒绝。元素按文档序生成层级标题，属性按
源顺序各自写入 `xml-attribute-name` 与 `xml-attribute-value` IR code block；前者记录 QName、
local name、prefix 与 namespace URI，二者的 provenance 分别覆盖原始 QName 与引号内 value。
mixed text 与 CDATA 保持事件顺序；注释保存在 `xml.comment.NNNNNN` 文档 metadata，
processing instruction 以明确的 `xml-processing-instruction` IR code block 保留。

XML 使用 namespace-aware streaming parser，校验作用域、未绑定 prefix、重复 expanded
attribute 与 closing tag。DOCTYPE、DTD 和自定义实体全部拒绝，也不会创建 external resolver；
仅五个预定义实体与 numeric character reference 可解码，并受累计文本预算约束。UTF-16
解码维护 decoded UTF-8 boundary 到原始字节的紧凑映射，因此元素、文本、CDATA、实体与 PI
的 provenance 仍指向原始编码字节。两种格式都执行输入、nesting、node、string/text、内存、
取消与 deadline 预算。JSON token、显式容器栈和 IR，以及 XML 解码、紧凑映射、事件栈、
属性扫描和 IR 均在分配前计入同一请求的逻辑内存；明确的结构化前缀即使损坏也不会回退为 TXT。

默认严格拒绝非法或截断序列；传统字符集使用增量 decoder 报告的 malformed 长度和
已消费范围定位原始字节，不以 lead-byte 宽度猜测。replacement 模式为 decoder 实际
报告的每个错误插入一个 U+FFFD；原始范围相邻的错误合并为一条稳定诊断，诊断同时记录
替换数量、规范化编码名和原始半开 byte range，合法后续字节保持不变。

CRLF、LF、CR 都作为源换行处理。单个换行保留为段内显式换行，空行分隔段落；连续
空行不制造无内容 IR 节点。段落 provenance 的 `byteStart`/`byteEnd` 始终指向原始编码
输入，包括 UTF-16 surrogate pair 和多字节传统编码，不使用解码后的字符位置。
创建节点前会以 checked arithmetic 同时核算 block、inline 和解码文本预算；超过 IR
上限直接返回 `resourceLimit`，不会先构造超限文档再由 Engine 报内部错误。

只有在格式规范允许时，容器变体才共享解析器。启用宏的 Office 文档仅作为内容包
解析，宏代码永远不会被加载或执行。除非显式启用网络访问，否则远程格式保持
不可用状态。
