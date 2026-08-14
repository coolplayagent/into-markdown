# into-markdown

[English](README.en.md)

`into-markdown` 是一个使用 Rust 开发、由 Bazel 构建的文档转 Markdown
平台。仓库当前提供架构设计、公共服务提供者接口、注册表与转换流水线、确定性
GFM 渲染器、TXT/Markdown/CSV/TSV/JSON/XML 与字符集转换器、固定的 ONNX Runtime CPU 安全运行层、
PNG/JPEG/TIFF/WebP/BMP 图片转换器、可安装的 PP-OCRv6 tiny 检测加识别 pipeline、命令行程序及
契约测试。受控 library transport 只安装固定官方 ONNX TAR、字符表和已审运行时；显式配置且按
invocation 授权的 OpenAI-compatible 适配器可执行受控图片描述，网络与 AI 默认关闭。

本项目完全独立于相邻的 `anydoc` 和 `markitdown` 项目实现。包括 PDF、OCR
和 AI 生成内容在内的所有输入，都必须先进入带溯源信息的统一中间表示（IR），
再由中央渲染器生成 GitHub Flavored Markdown（GFM）。

## 构建

```shell
bazel build //...
bazel test //...
cargo check --workspace
```

支持的目标平台为 macOS ARM64、Linux x86_64、Linux ARM64 和 Windows
x86_64。项目明确不支持 macOS x86_64。

## 命令行

```shell
bazel run //apps/cli:into-md -- report.pdf
bazel run //apps/cli:into-md -- notes.txt
printf 'caf\351\n' | bazel run //apps/cli:into-md -- --charset windows-1252 -
bazel run //apps/cli:into-md -- table.csv
printf 'name\tage\nAlice\t42\n' | bazel run //apps/cli:into-md -- --format tsv -
bazel run //apps/cli:into-md -- report.pdf -o report.md
bazel run //apps/cli:into-md -- documents/ --recursive --output-dir markdown/
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- models show pp-ocrv6-tiny-zh-en --json
bazel run //apps/cli:into-md -- doctor
bazel run //apps/cli:into-md -- ui
```

CLI 采用直接输入形式，不提供 `convert` 子命令。支持多文件与目录批量处理、stdin、
URI、OCR/AI 策略、结构化 JSON、资源 Bundle、分层配置、Provider、模型与插件管理。
联网与 AI 默认关闭，远程输入和 Provider 每次都需要显式 `--allow-network`。

`into-md ui` 启动固定监听 `127.0.0.1` 的本地 Web 安全入口和嵌入式 React 控制台壳，默认使用系统分配端口并
打开浏览器。它生成独立的高熵会话值，通过 URL fragment 交给嵌入页面；API 同时校验
精确 Host、Origin 和会话 Header。当前状态页包含响应式布局、主题、简体中文/英文、键盘与焦点支持，并明确报告尚不可用的
文档业务能力；它不包含任务、数据库、工作台、预览或管理功能。所有 content-hash 资产由
Bazel 离线构建并直接嵌入 Rust 发布物，不依赖 CDN。用法和威胁边界见[本地 Web 服务](docs/ui.md)。

转换结果和批量报告使用 CLI 与未来 HTTP 服务共享的公共 DTO schema 1。例如
`--emit result-json` 返回 `markdown`、版本化
`document`、base64 `assets`、`diagnostics` 和 `provenance`。协议细节、兼容策略与
不可信 JSON 资源预算见[稳定数据传输契约](docs/dto.md)。

TXT 转换可用，支持 UTF-8、带 BOM 的 UTF-16 与受限的常见字符集自动检测；显式
`--charset` 支持 `windows-1252`、`gb18030`、`big5` 和 `shift_jis`。无效序列默认
严格失败，`--encoding-errors replace` 会替换并输出带原始字节范围和替换数量的诊断。
自动检测会为完整 JSON 及具备三行和类型证据的 CSV/TSV 候选让路；带 BOM 的输入也会
按实际字符集解码完整内容，除 TAB、LF、CR 外的 C0、DEL 或 C1 都会拒绝自动 TXT 候选。

CSV 与 TSV 转换支持 RFC 4180 引号、双引号转义、字段内换行、空单元格和 UTF-8/UTF-16
BOM，并复用 TXT 的安全字符集解码。`--table-header auto|always|never` 控制表头，
`--ragged-rows strict|pad` 控制不等宽记录；默认保守识别表头并严格拒绝不等宽记录。
所有值作为文本进入 IR，中央 GFM 渲染器负责 pipe 与换行转义。

JSON 与 XML 转换可用。JSON 严格校验 RFC 8259、拒绝重复键、保留对象源顺序与数字
lexeme；XML 支持 UTF-8 和 UTF-16LE/BE，保留 QName、namespace、属性源顺序、mixed text、
CDATA 与 PI，并把注释记录在文档 metadata。XML 的 DTD、自定义/外部实体稳定拒绝，
两种格式的 provenance 都使用原始输入字节范围。完整 JSON 顶层标量也会自动识别；XML 每个
属性的 QName/value 具有独立原始 byte span，UTF-16 映射复用 compact run decoder。

HTML 转换可用。固定版本 HTML5 容错 parser 提取正文、标题、链接、图片、列表、表格、代码与
metadata；确定性正文选择会诊断降级并排除导航、广告和隐藏内容。脚本、样式、模板与
SVG/MathML active content 不执行也不穿透资源；`base` 只解析引用。外部图片仅作为 canonical
HTTP(S) audit Asset 保留，转换全程离线且绝不自动获取。

Wikipedia/MediaWiki 作为显式组装的站点插件 API 提供，不进入默认 Engine 或核心发布清单；
网络仍默认关闭。Wikipedia 标准根 `/wiki/<title>` 可由插件识别，其他 host 必须使用
`mediawiki+http(s)` 显式 opt-in。最终 API URL/MIME、同源 endpoint、
完整 JSON shape 与预算均在 HTML 语义提取前验证；block provenance 通过稳定 MediaWiki provider
关联文档级 source URL、page/revision ID 和 retrieved-at 记录，不伪造 API HTML byte locator。

RSS 2.0 与 Atom 1.0 Feed 转换可用，提取标题、作者、时间、链接、摘要与正文，并保留每个条目的
原始 byte provenance。HTML、`content:encoded` 与 Atom HTML/XHTML text construct 复用同一
安全 HTML 转换器，active markup 被过滤后不会经纯文本 fallback 回显；相对 URL 按 source URI
及嵌套 `xml:base` 离线解析，绝不获取。条目保持源顺序，
严格解析 RFC 822/RFC 3339 时间并诊断非法值；按 ID、canonical link、长度分隔内容摘要稳定去重。
DTD、外部实体、错误 namespace、RSS/Atom 混淆与超限输入稳定拒绝。
Nested HTML 在 parser 构造前预付固定 html5ever 0.39.0 的保守逻辑工作区，并与 Feed 最终输出
共享同一 lease；失败 fragment 析构后完整回滚。该预算不表示 allocator metadata 或进程 RSS。
Feed XML 的 expanded-name/attribute、`xml:base`/URL、diagnostic 与 XHTML 逐事件写入也共享该
lease；自有 Vec/String 按真实 capacity 在分配前计费，CDATA/attribute escaping 在写入前计算扩张。

Markdown/GFM 转换支持标题、强调和删除线、链接与 autolink、嵌套列表和任务列表、
表格、代码块及脚注，并保存 UTF-8 原始字节范围。独立段落的安全 HTTP(S) 图片以
external-only Asset 保持结构化且不下载；inline、相对或危险目标明确诊断并安全降级。
raw HTML 与 blockquote 在现有 IR 中保存为不可执行代码容器并产生诊断。
详细检测、编码、资源与降级规则见[格式矩阵](docs/formats.md)。

DOCX 与 DOCM 转换可用，覆盖标题样式、富文本、编号、表格、链接、图片、脚注、页眉页脚、
批注、字段和公式。宏部件永不读取或执行；加密、损坏及超出 ZIP/XML/资源预算的输入稳定拒绝。

RTF 转换可用，支持字符集与 Unicode escape、行内样式、段落、列表、表格、metadata 和
经完整解码审计的 PNG/JPEG 图片。内嵌 object、文件路径、HTML 与 active destination 永不
执行或联网；危险 hyperlink 和 EMF/WMF 安全降级并产生结构化诊断。
PPTX、PPTM、PPSX、PPSM 与 POTX 转换可用，覆盖 slide 边界、标题/文本、富文本、列表、
表格、PNG/JPEG、图表缓存文字、speaker notes、layout/master placeholder 继承及 theme 元数据。
内容按最终几何恢复阅读顺序，真实重叠元素保留 `spTree` z-order，并保留 slide+bounds
provenance；宏、ActiveX、OLE、嵌入包与所有
外部关系不读取、不执行且不联网，损坏、加密和资源越界输入 fail closed。

DOC、PPT/PPS/POT 与 XLS 通过固定随包路径中的 `legacy-office-worker` 隔离转换为对应 OOXML，
再以同一请求的取消、超时、资源预算和离线策略进入内置 OOXML 转换器。父进程不搜索系统
LibreOffice、`PATH`、代理或 loader 环境；未安装当前平台的精确 authority/runtime 时稳定返回
`componentUnavailable`。Windows 只消费安装器预置且 SID 精确匹配、零 capability 的
AppContainer，不在转换路径创建或删除持久 profile。运行时制品的组装和发布许可清单由平台包装
任务独立交付。worker/kit 与非系统依赖只从 authority 校验后的请求私有只读快照 exec/load，
输出必须通过 exact ZIP、CRC、内容类型与根 relationship 的 DOCX/PPTX/XLSX family 审计。

模型查询、显式安装、离线校验、路径和安全清理后端已实现。完整
`pp-ocrv6-tiny-zh-en` pipeline 绑定可安装的 detector 与 recognizer 组件；两者分别固定官方
ONNX/TAR/config，recognizer 另外固定字符表，全部经过 SHA-256、归档结构、license 与安装事务
校验。只有 `models install` 会使用固定 host、固定大小与固定 hash 的模型 transport；普通转换
不会自动下载。发布布局中的固定 ONNX Runtime 与 worker、已安装或随包模型均验证通过后，
CLI/API 才装配真实 OCR 服务。其他尚未可用的格式转换和插件执行后端仍明确拒绝。
Windows 模型安装在 reparse-safe 目录 handle 持久同步完成审计前
同样 fail closed；目录解析和离线元数据查询不受影响。

抽取资源按完整内容 SHA-256 去重并使用 MIME 权威扩展名；主 Markdown 与全部资源
通过带持久 journal 的同一输出事务提交，进程中断后会恢复为完整旧集合或完成完整
新集合。每个物理目标父目录使用身份绑定的固定 lease，下一次相关输出会在写出前恢复
并有界重做预检，不扫描祖先或无关目录。该安全写出事务当前在 Unix 平台可用；Windows 返回稳定
`componentUnavailable`，资源规划与 bundle 编码不受影响。portable bundle manifest 使用
`schemaVersion: 2`，以
`sourceAssetIds` 表达多个文档资源 ID 到一个物理条目的映射。

实现路线详见[架构设计](docs/architecture.md)、[接口契约](docs/interfaces.md)、
[格式矩阵](docs/formats.md)、[OCR 与 AI](docs/ocr-and-ai.md)、
[本地模型管理](docs/models.md)、
[安全模型](docs/security.md)和[测试策略](docs/testing.md)。
命令与配置契约详见[命令行设计](docs/cli.md)和[配置文件](docs/configuration.md)，
许可证、第三方来源和发布审计详见[许可证治理](docs/licensing.md)。
