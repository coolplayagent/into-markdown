# 规划格式矩阵

运行时的权威列表以 `into-md formats` 输出为准。TXT、CSV、TSV、JSON、XML 状态为 `available`，其余尚未
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
