# 规划格式矩阵

运行时的权威列表以 `into-md formats` 输出为准。TXT 状态为 `available`，其余尚未
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

无 BOM 自动检测只读取至多 64 KiB 样本，先通过 NUL、控制字符、替换字符和可打印文本
阈值，再使用固定检测器选择 allowlist 编码。带 BOM 的输入同样必须对有界样本做严格
解码并通过文本安全阈值，BOM 本身不会无条件形成高置信候选；样本尾部的截断序列仍由
转换阶段返回稳定编码错误，带 BOM 的二进制伪装则不作为 TXT 候选。

JSON、XML、HTML 等结构化候选先于普通文本。JSON 即使超过 1 MiB 探测上限，只要有界
前缀是尚未闭合的合法 JSON 结构，仍保留为 JSON 候选；至少两行且字段数一致的逗号或
Tab 分隔内容保留为 planned CSV/TSV 候选，自动 TXT probe 返回 `NotApplicable`。显式
`--format text` 或 `--charset` 仍表示用户有意按文本解释输入。已知二进制 magic 也不会
降级为 TXT。

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
