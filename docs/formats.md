# 核心格式目录

运行时的权威列表以 `into-md formats` 输出为准。PDF、DOC/DOCX/DOCM、ODT/ODS/ODP、
PPT/PPS/POT/PPTX/PPTM/PPSX/PPSM/POTX、XLS/XLSX/XLSM/XLSB、EPUB、RTF、ZIP、TXT、
Markdown、HTML、CSV、TSV、JSON、XML、RSS/Atom、IPYNB、Outlook MSG，以及
PNG/JPEG/TIFF/WebP/BMP 图片以及 Audio/Video 状态为 `available`。DOC/PPT/XLS 是 Core 原生
格式；Audio/Video 要求完整语音能力插件。能力缺失时返回
`componentUnavailable`。YouTube、Wikipedia/MediaWiki 等站点适配器不是格式 catalog 条目，
ASR、AI Provider 与插件本身是能力来源，不会以 `planned` 冒充格式。

| 类别 | 格式 |
| --- | --- |
| 文档 | PDF；DOC/DOCX/DOCM；PPT/PPS/POT/PPTX/PPTM/PPSX/PPSM/POTX；XLS/XLSX/XLSM/XLSB；ODT/ODS/ODP；RTF；EPUB |
| 文本与数据 | TXT；Markdown；HTML；CSV/TSV；JSON；XML；RSS/Atom；IPYNB |
| 图片 | PNG；JPEG；TIFF；WebP；BMP |
| 音视频 | Audio；Video（经受认证 FFmpeg 解码并由语音能力插件转写） |
| 容器与消息 | ZIP；Outlook MSG |
| 受控输入基础 | HTTP(S) SourceResolver（默认离线，须显式网络授权） |

每种当前可用格式的可执行 dry-run 示例见[命令与格式示例](cli-examples.md)，并由 CI 从
`into-md formats --json` 实时发现后逐项校验。

## 图片

图片转换器只按完整 magic 与 container envelope 接受 PNG、JPEG、Classic TIFF/BigTIFF、
WebP 和 BMP，不信任扩展名或 MIME。进入 decoder 前会验证 PNG chunk 顺序与 CRC、JPEG
marker/segment/entropy、WebP RIFF/chunk/frame、BMP DIB/pixel range，以及 TIFF IFD 链和
strip/tile range；文件尾必须由格式声明精确覆盖。尺寸、累计像素、帧数、结构项、解压、
asset、内存、取消与 deadline 均受同一请求预算约束。TIFF 和动画 WebP 的每帧映射为独立
Page；方向在受界像素上应用，DPI 只读取有限数字字段，ICC/Exif/XMP 自由文本与 active
payload 不执行。

原始文件作为离线 Asset 保留；多帧或需要方向归一化时，页面另生成受界 PNG Asset。
OCR `off` 零调用，`auto` 缺模型或缺少 detector/model 绑定证据时保留图片并给出明确诊断，
`always` 则稳定失败；只有 identity-bound detection/recognition evidence 可以生成
`Inline::OcrText`。`image_description` 按 `off`/`fallback`/`prefer`/`only` 路由固定
`ImageDescription` 请求，不接受文档 prompt；AI 节点、诊断、provider/page provenance 和
内存 plan 全部验证成功后才事务发布。

## Office 97–2003 原生转换

DOC、PPT/PPS/POT 与 XLS 只接受经 CFB/OLE 目录证据或调用方显式选择的候选，converter probe
仍要求完整 compound-file magic。共享 CFB reader 在解析前认证 DIFAT、FAT、miniFAT、目录树、
stream 链、循环、重叠、截断与资源预算；三个前端在同一 `ExecutionContext` 和
`ConversionOptions` 下直接生成 Document IR、Asset 与结构化 diagnostic。

DOC 读取 Office 97–2003 FIB 与 piece table；PPT 按 CurrentUser、UserEdit 与 persist authority
恢复 slide 和 speaker notes 顺序；XLS 先完成 CFB/BIFF8 边界校验，再复用统一 Workbook IR
组装。公式只保留源码与缓存值，宏、ActiveX、外部工作簿和嵌入式可执行对象不执行、不联网。
可认证的 PNG/JPEG 载荷在 asset 限额内保留，无法稳定定位或安全解释的内容产生就近 diagnostic。

Word 6/95、Excel 5 等更早版本返回 `unsupported`；加密、损坏和超限分别返回 `encrypted`、
`malformed` 与具体 `resourceLimit`。Core 不查询 PATH、系统 Office、外部命令、OCR、AI 或网络，
临时安装目录中的 Core 可在四个发布平台直接转换这些格式。

## Outlook MSG

MSG 转换器默认离线读取 CFB/OLE 与 MAPI property streams，提取发件人、To/CC/BCC、主题、
提交或送达时间、传输头和正文。正文选择顺序固定为 HTML、压缩 RTF、纯文本；HTML 复用同一
HTML5 安全转换边界，纯文本直接进入统一 IR。压缩 RTF 先完整验证 MS-OXRTFCP header、CRC、
dictionary 与 back-reference，再在同一请求 context 和 limits 下把解码 RTF bytes 交给窄 RTF
转换接口，不会回退到低语义正文或复制另一套 parser。

String8 只接受显式受支持 MAPI codepage 并无替换解码；Unicode property stream 必须是对齐、
有效且不含 NUL 的 UTF-16LE，property entry 的声明大小另外计入规范终止符。附件只接受离线
by-value 和 embedded-message 方法，名称、MIME、
Content-ID、单项与总字节数均先校验。只有 HTML 中 exact canonical `cid:` 引用且通过 PNG/JPEG
结构审计的唯一附件会在引用位置绑定本地 Asset；未引用 CID 或非图片作为普通附件保留，重复
Content-ID fail closed，且不解析或获取远程目标。
嵌套 MSG 在同一 CFB directory graph 内递归，受共享深度、条目、展开字节、资源和工作预算约束。
每个正文、header、附件说明和嵌套节点的 provenance `part` 保留从 `msg` 根到 property stream
及 attachment storage 的完整链。HTML/RTF 子转换的 byte offset 属于解码流，无法可逆映射到
MSG property stream 时明确省略 `byteStart`/`byteEnd`，不伪造原文件坐标；附件 Asset ID 与
同一链也写入 namespaced metadata。

CFB reader 在分配和发布前验证 version/sector shift、DIFAT/FAT/miniFAT、directory sibling/child
图、stream 声明长度要求的 exact sector/mini-sector chain 及全部所有权（最后 sector 仅允许
内容 padding）。循环、额外 sector、重复所有权、交叉重叠、越界、
截断、扇区数量炸弹、重复大小写名称、危险附件路径、未知 codepage 和 property length 不一致
均 fail closed。解析不调用网络、系统 Outlook、COM、外部命令或可选 AI 服务。

## OpenDocument（ODT、ODS、ODP）

OpenDocument 转换器完全离线实现 ODF 1.2/1.3 的安全子集，不调用 LibreOffice 或其他办公软件。
包必须以无 extra/data-descriptor、未压缩、首项且内容/CRC/size 精确匹配的 `mimetype` local header
开始，并与唯一 central directory 双向绑定；全部 raw 名为严格 UTF-8，非 ASCII 时必须设置 bit 11，
且禁止 Unicode Path/name-changing extra 与 entry comment。`META-INF/manifest.xml` 的根媒体类型、版本、封闭 core/image/
empty-directory 图与实际 ZIP 目录必须一致。加密项、ZIP64、DTD/自定义实体、未知元素/属性、路径逃逸、重复部件、
脚本/宏、签名、嵌入文档、外部图片与活动事件全部 fail closed。HTTP(S)、mailto 和文档内片段
链接只作为惰性数据保留，转换器不会访问网络。

ODT 提取标题、段落、按 family/origin 解析的继承文本样式、正式 list-level number/bullet/start/continuation、
隐式嵌套 list identity、无 marker list-header、表格、图片、脚注与严格配对的 ranged/point 批注；ODS 将工作表、严格 typed cache、隔离的 OpenFormula、行列重复、合并跨度和零基坐标
映射到 `Sheet`/`Table` IR。尾部空重复保持稀疏而不物化，但仍推进并验证逻辑坐标。ODP 按源顺序
生成 `Slide`，仅 title placeholder 成为标题，subtitle 保留为正文；嵌套 group transform 组合后的
正文形状、表格、图片、finite affine 边界框、旋转和 speaker notes 保留在 slide 内。图片仅接受 manifest MIME、
规范扩展与 sniffed bytes 一致、有界且可完整解码的 PNG/JPEG/GIF/WebP，并以内容散列去重。

Converter SPI 在创建 ZIP、XML DOM 或图片 decoder 之前获取与 Engine 预检同上下文认证的全部
逻辑内存 credit；临时对象析构后，同一不透明 lease 由中央 retained-output estimator 校准并随
结果转移。工作集按可达 core/assets 和阶段峰值规划，未引用图片只以固定流缓冲审计 CRC，不按 expanded
bytes 占用内存 permit。取消和 deadline 在 ZIP、XML、repeat、页面及图片有界 codec 边界定期检查。

## PDF

PDF 使用审核并固定版本的 PDFium 动态库。Windows portable Core 与 Agent Skill 在规范化可执行文件
旁的 `lib/pdfium/pdfium.dll` 携带 manifest 固定运行时，CLI 默认只从该发布布局发现它；不会从
当前目录、系统库、`PATH` 或动态加载器环境回退，也不下载运行时。库调用方仍可通过跨平台
runtime resolver，或以 `PDFIUM_LIBRARY` / `PdfConverter::with_runtime_path` 给出精确绝对路径；
缺失或认证失败稳定返回 `componentUnavailable`。

每页先产生一个 `Block::Page`。字符按 PDFium 原生 character index 忠实进入
`Inline::SourceText`，携带 index、Unicode scalar（不可映射、surrogate、NUL 与禁用控制字符
为 U+FFFD）、字体名称线索、字号、规范化到 `[0, 360)` 的字符角度与边界框。页面、字符、链接、
图片和 OCR evidence 节点都带一基页码 provenance；坐标统一为 PDF point（1/72 inch）、应用页面
旋转后的左上原点、X 向右、Y 向下，page locator 同时记录显示宽高与顺时针旋转角。

PDF converter 随后把 native 字符、图片和已验证 OCR merge 结果交给独立的通用 IR 几何阶段。
该阶段按方向聚合字符为行，以有界 XY-cut 恢复页面阅读序，再保守生成段落、标题、列表和对齐
表格；native/OCR 重叠文本先按 NFC、空白和几何重合去重，native source 优先。水平、竖向和旋转
文本只依据坐标和 source angle 排序，不从文字内容猜测语言或文档结构；证据不足时保留普通段落，
不制造标题、列表或表格。两行纯文本表格必须有精确重复的文字边界，或由 PDFium 有界提取的
多个 PATH bounds 形成覆盖每个 cell 的稳定行列网格；孤立矩形和 diagram path 不算表格证据。
PATH bounds 只作为 page-scoped transient sidecar 参与布局，不进入 Document IR 或 wire DTO，
字体只在几何已证明的窗口内标注 header。跨至少两个不同页面、位于页面上下 12% 边缘且规范化文本完全相同的
段落会在既有 locator `part` 中标记为 `pdf/running-header` 或 `pdf/running-footer`，不删除原文，
也不通过去数字等规则猜测变化中的页码。所有重建节点继续保留原 source locator 与 provenance。

依赖边界保持单向：PDFium 只负责可信快照，OCR 只读写通用 Document IR，`pdf-layout` 只消费
统一 IR，converter 负责先 OCR merge 再最终布局。普通 build/test 不下载 PDFium 或模型；真实
PDF layout 与 OCR 质量分别由显式 authority target 验收。

注释 link 与 PDFium web link 都被提取。内部目标表示为 `#pdf-page-N`，外部目标只接受绝对
HTTP(S)/mailto URI；中央 Markdown renderer 为每页发出稳定的 `pdf-page-N` 安全 anchor，因此
内部目标不会成为悬空链接。无效 UTF-8、NUL、控制字符、危险 scheme 或无法表达的 destination
fail closed，转换过程从不访问 URI。真实 image object bitmap 会校验尺寸、stride、format 与
完整 pixel bytes，再转为有界 BMP asset；内容 SHA-256 形成稳定去重 ID，图片节点的 provenance
保留 object 边界。

扫描页启发式固定为：少于 8 个非空白、非控制的原生字符，且 image object 的保守 union
覆盖率至少 50%。覆盖以固定 64×64 网格计算，只有完整落入图片矩形的格子才计入，重叠只计
一次，因此近阈值只可能低估而不会因采样产生 false positive。空白页因没有覆盖图片不会误判；混合页始终同时保留文本和图片。`ocr=auto` 只为扫描页
创建至多 4096 像素边长的页面 render asset，`ocr=always` 明确为每页创建，`off` 不 render。
本层只准备 OCR 输入资产，不伪造 OCR 成功。PDFium 无法可靠区分缺密码和错误密码时，两者
都映射为稳定 `encrypted`；损坏文件为 `malformed`，超页数为 `resourceLimit`。

## RTF

RTF 转换器要求严格的 `{\\rtfN` 根签名，并使用有界单遍状态机解析 group、destination、
控制字、ANSI codepage、font charset、Unicode fallback、行内样式、段落、列表、表格、metadata
和 `pict`。块级 provenance 始终保存原始 RTF 的半开 byte range；转义、hex 与 Unicode
解码不会用解码字符位置冒充源偏移。allowlist 包含 Windows-1252、GBK、Big5 与 Shift-JIS；
未知 codepage/font charset 稳定拒绝。

PNG/JPEG `pict` 在单项/总资源、解码字节、尺寸与内存预算内实际解码审计后作为 Asset；
EMF/WMF 只产生安全降级诊断。object、objdata、filetbl、datastore、field instruction、HTML
及未知 ignorable destination 不执行、不读路径、不联网。只有 canonical HTTP(S) hyperlink
保留结构化链接，危险 target 降级为纯文本并诊断。组深度、控制数、数字位数、Unicode
膨胀、段落/inline/table/cell、图片、诊断与所有主动 Vec/String/Map capacity 均有硬上限，
长循环执行 request checkpoint。

## PresentationML

PPTX、PPTM、PPSX、PPSM 与 POTX 共用严格的离线 OPC/PresentationML 转换器。幻灯片标题、
文本框、富文本、列表、表格、PNG/JPEG、图表缓存文字与 speaker notes 进入统一 Document IR；
每张幻灯片使用 `Slide` 边界，内容节点 provenance 保存原始 part、幻灯片号以及合成
layout/master/group 变换后的最终显示 bounds。任意角度旋转按 shape 中心计算轴对齐显示区域；
互不重叠的区域按最终几何恢复阅读顺序；AABB 只作候选筛选，凸四边形 SAT 确认真实面积
相交后才形成重叠连通分量，零面积的点或线不建立绘制顺序耦合；分量内部严格保留
`spTree` 绘制顺序，原始
z-order 还以 `presentation.zOrder.<node-id>` namespaced metadata 可追溯。富文本语言以
`presentation.languages.<node-id>` metadata 保留。
Slide→Layout 按 placeholder `idx`（缺省为 0）唯一绑定；Layout→Master 不沿用 `idx`，而是按
规范化 placeholder class/type 投影：`title`/`ctrTitle`→title，正文与内容类→body，
`dt`/`ftr`/`sldNum` 各保持同类。合成时 layout 优先于 master；重复 layout `idx`、
重复投影类或其他歧义均 fail closed。
off/ext/rot/flip 按 slide→layout→master 逐属性继承后才组合 group transform；rich style 保留
absent/true/false 三态，并按 run→paragraph→slide→layout→master 合成，master `txStyles` 的
title/body/other 1–9 级会参与列表与样式继承；`txStyles` 及其三种 section 重复时稳定拒绝。theme 关系和 XML 会严格
验证，theme 名称保存在 namespaced metadata，但当前 IR 不表达 theme 字体或颜色，因而不声称
恢复这些视觉属性。三者都不作为正文单独输出；隐藏幻灯片确定性省略并诊断，隐藏 shape 及
隐藏 group 的全部子 shape 确定性省略。当前表格实现不推测损坏或歧义的合并结构，列表用源
level marker、bullet 字符或编号 scheme 保留层级与标记；图表只读取内嵌 cache 的文字/数值，
不计算公式或读取外部 workbook；没有受支持 chart/table payload 的其他 graphicFrame 稳定拒绝。

转换器先检查 ZIP 目录和 `[Content_Types].xml`，之后只沿 root officeDocument、slide order 与
实际授权关系按需解压 main/slide/layout/master/theme/notes/chart/image；未引用 payload 不解压。
PPTM/PPSM 的 VBA、ActiveX、OLE 与嵌入包按 content type 和关系 type 在目标解压前隔离，绝不
读取或执行。外部关系在本转换器中统一 fail closed（包括 hyperlink、media 与 embedded object），
因此不会发起网络或输出不可验证的外部对象。加密 OLE 包、Strict OOXML namespace、DTD/实体、
损坏/重复关系、错误 content type、路径逃逸以及超出 ZIP/XML/IR/图片/内存预算的输入均稳定拒绝。

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

## Wikipedia / MediaWiki

该实现保留为可显式构造的插件 API，不由默认 Engine 静态注册，也不进入核心 formats、doctor
或发布能力清单。插件宿主可显式注册 resolver、detector 与 converter；以下安全约束保持不变。

标准 Wikipedia `http(s)://<lang>.wikipedia.org/wiki/<title>` 会进入专用远程来源；其他
MediaWiki host 必须使用显式 `mediawiki+http(s)://host/wiki/<title>` opt-in。只接受根
`/wiki/` article path，并固定派生同 origin `/w/api.php?action=parse`，因此普通站点的
`/assets/wiki/`、`.json` 或自定义 script prefix 不会被猜测为 MediaWiki。

网络默认关闭。解析器复用公共 HTTP transport 的 allowlist、逐跳 DNS/IP/私网检查、redirect、
wire/decoded size、deadline 和 cancellation，并在转换前要求最终响应仍为同 origin 的
`/w/api.php` 与严格 `application/json`。JSON 在 typed serde 前以 checkpointed、非递归 shape
scan 验证完整语法、全部对象的重复键、request nesting、集合和字符串上限。返回的 HTML 继续
使用统一 HTML 语义提取；正文、章节、链接和页面内主要图片进入 Document/Asset IR，图片仍只作
external-only audit 引用，不产生第二次网络请求。

单篇结果使用一个文档级 `mediawiki.*` source record：`provider`、无凭据/无 query 的 canonical
`sourceUrl`、`pageId`、`revisionId` 与 UTC `retrievedAt`。全部递归 block provenance 使用稳定
`builtin.converter.mediawiki` provider，并清除无法由 API HTML 证明的 byte locator；引擎导出的
provenance inventory 可按该 provider 关联唯一 source record。当前 Asset IR 没有独立 provenance，
所以图片通过所属 Document source record 与其 `externalUri` 审计，而不伪造图片来源坐标。

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

## RSS 与 Atom

Feed 转换器只接受本地或已经解析到内存的 RSS 2.0 与 Atom 1.0。RSS 根必须是无命名空间的
`rss version="2.0"`，并且只含一个 `channel`；Atom 根、`entry` 与 text construct 必须使用
`http://www.w3.org/2005/Atom`。`content:encoded` 仅识别其标准 namespace。Atom 的
title、subtitle、summary 与 content 均遵守 Text Construct：`type=text` 保持文本，
`type=html` 复用 HTML 安全转换器，`type=xhtml` 必须只含一个 XHTML namespace 的 `div`
（空 `div` 合法）。外层 `div` 与内部元素的 `xml:base` 作用域均在交给 HTML 转换器前安全解析。
RSS `description` 与标准 namespace 的 `content:encoded` 统一经过同一 HTML 安全路径，不依赖
标签字符串启发式；script、style、template 等 active content 被排除后不会以原始文本回显。

条目按源顺序输出，并通过 `feed.sourceOrder` 诊断声明；时间不会触发重排。Feed 层级的链接与
更新时间作为文档开头的可见字段保留，避免用不可精确计量节点容量的映射容器暂存。Atom 时间严格接受
RFC 3339，RSS 时间严格接受一至两位日期、四位年份、两位时分秒和 numeric/GMT/UT zone 的
RFC 822 形式，统一规范化
为 UTC；leap second、obsolete alphabetic zone、折行空白、两位年份与非法日期保留原值并产生
诊断。重复条目按 `guid/id`、canonical alternate link、带字段长度前缀的内容 SHA-256 依次选择
去重键，保留源序中首次出现的条目并诊断后续重复。

相对 URL 依据受信 source URI、feed 层级 `xml:base` 解析。普通 entry link 可安全保留 HTTP(S)
query/fragment；HTML 图片仍遵守更严格的 external-only Asset canonical 规则。URL 解析仅生成
审计数据，不发起请求。解析器共享 XML 的 UTF-8/UTF-16、XML 1.0 character、预定义/numeric
entity 与 declaration 契约，拒绝 DTD、DOCTYPE、自定义/外部实体、错误 namespace 和混淆 root。
entry、nesting、事件、文本、nested HTML、asset、diagnostic、IR 与输出字符串共用贯穿 Feed
生命周期的聚合逻辑内存预算，取消与 deadline 长循环 checkpoint 也有硬边界。合并每段 HTML
时递归重编号所有 BlockNode 与 Asset 引用，并保留 Feed entry span 和 HTML provider 来源链。
Feed 的已解码 UTF-8 fragment 不再复制进入 HTML charset decoder。由于固定的 html5ever 0.39.0
没有 allocator hook，转换器在构造 parser 前以无堆、checked arithmetic 扫描预付协作式逻辑
工作区：模型固定到 servo/html5ever commit `ce64836c685025a5fef0860fa2e9c80b2683e8d0`
与 tendril commit `d64dfd4c21cf2451649107ade7eaf042d95fbc5a`，覆盖 markup5ever 16-slot
`BufferQueue`、tokenizer 的 9 类 tendril/attribute、TreeBuilder 四类 vector、Vec 的 2 倍增长、
tendril 的 next-power-of-two（小于 2 倍）及 adoption-agency
全部 8 轮。parser、DOM 与最终 Document 使用同一个 Feed lease；parser/DOM 析构后才释放未转为
持久输出的预付空间。该边界不代表 allocator metadata 或进程 RSS。任意 fragment 错误会先析构
parser、DOM 与局部输出，再完整恢复 memory、对象、字符串和输出字节快照；随后产生的最小 Feed
诊断独立计费。
Feed XML 本身不再用 start-tag 长度倍率估算持久对象：每个 element/attribute 的 expanded-name、
解码值、`xml:base` 结果、URL 与 diagnostic 都在构造前从共享 lease 预扣，自有容器按 allocator
返回的真实 capacity 补差。Atom XHTML 不使用带隐藏增长的通用 writer；逐事件写入预算化 String，
CDATA 与 attribute escape 先无堆计算精确扩张长度。任何属性、URL、诊断或 XHTML 事件失败都会先
析构局部值，再恢复该操作的完整预算快照。

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
