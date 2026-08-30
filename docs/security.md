# 安全模型

WASI 插件是非信任输入边界：固定 runtime/hash、默认无 ambient 能力、显式 capability、
fuel/epoch/memory/output/resource 限制，以及统一 IR/provenance 重验共同生效。详细威胁模型、
稳定错误和真实 guest 门禁见 [`wasi-plugins.md`](wasi-plugins.md)。

本地 Web 入口的攻击者、剩余风险和验证矩阵见[本地 Web 控制台威胁模型](web-security-threat-model.md)，
请求与产品契约见[本地 Web 服务](ui.md)。该入口只监听
IPv4 回环地址，不把转换网络授权扩展到 Web 服务，也不接受配置文件提供监听地址或
会话秘密。

文档、归档文件、标记语言、媒体、模型文件、提供者响应和 URL 均为不可信输入。

媒体字节只进入 FFmpeg 子进程。工具必须位于显式可信根下的绝对规范路径，不能是符号
链接，并在执行前匹配经过认证的大小、哈希和构建配置。子进程使用空环境、私有工作目录、
仅 pipe 协议及有界 stdin/stdout/stderr。PCM 容量在启动前由时长、采样率、通道数和
16-bit 样本宽度精确计算并计费；取消或超时始终 kill 后 wait，超量输出或不完整帧失败。
Unix 子进程还设置地址空间、CPU、文件大小、文件描述符及 core dump 的 rlimit；Windows
使用 suspended process 与 kill-on-close、process-memory Job Object。rlimit 不是完整 OS
sandbox：部署方仍应使用平台 sandbox/container 加固，FFmpeg 的最小协议与组件面是主隔离层。

- `ResourceLimits` 限制输入大小、解压后字节数、归档条目数、嵌套深度、页数和
  保留资源数，并限制实现显式计费的内存与请求临时文件字节数。所有累加使用 checked
  arithmetic，临时文件由执行上下文负责 RAII 清理。
- 归档路径必须规范化，拒绝路径穿越和绝对路径，解压时不得跟随符号链接。
- Outlook MSG 的 CFB/OLE reader 对 DIFAT、FAT、miniFAT、directory sibling/child 图和每条
  stream chain 分别检测循环、越界、重复 sector 所有权与交叉重叠；directory slot、MAPI
  property、附件、嵌套深度、展开字节和工作量共享聚合预算。String8 仅按受支持 codepage
  无替换解码，Unicode、property length、recipient/attachment count 与路径名不一致均 fail
  closed。stream chain 必须与声明长度所需 sector 数完全一致。CID 和 HTML 外链不触发网络；
  只有 exact canonical CID 引用可绑定通过 MIME 与结构审计的唯一 PNG/JPEG，未引用或非图片
  CID 按普通附件处理；附件只接受安全名称的 by-value 或受深度限制的嵌套 MSG。
- Office 97–2003 与 MSG 共用 Core 内受限 CFB/OLE reader；DIFAT、FAT、miniFAT、目录树、
  stream chain、sector 所有权、循环、重叠与截断在格式前端运行前统一校验。DOC piece table、
  PPT record/persist 图和 XLS BIFF8 record 均接入请求取消、内存、结构、表格与 Asset 预算；
  加密、损坏和超限分别返回稳定的 `encrypted`、`malformed` 与 `resourceLimit`。解析器不创建
  进程、不读取 PATH 或系统 Office、不访问网络，也不执行宏、ActiveX、公式或嵌入式可执行对象；
  可安全恢复的内容带结构化 diagnostic，其余内容稳定跳过。
- PresentationML 只沿 root officeDocument 与 slide order 可达的受授权关系图按需解压；未知或
  未引用 payload 不进入内存。VBA、ActiveX、OLE 与 embedded package 目标同时按 content type
  和关系 type 在解压前隔离，即使目标重命名或伪装为 octet-stream 也不能绕过。所有外部关系
  均 fail closed，转换器不执行宏、外部对象、链接或网络访问。图片还必须同时匹配关系 type、
  OPC content type、扩展名和完整 PNG/JPEG codec 验证。隐藏 slide/shape 以及 layout、master、
  notes 中随后不输出的引用同样先验证关系授权与目标存在性，但不会因此解压不可见目标。
- PresentationML 在构造 ZIP 解析器前以无分配 EOCD/ZIP64 central-directory preflight 计算
  条目、名称和可变元数据上界并完成 reservation；zip-rs 的统计随后必须与计划一致。实际 part
  以固定小块解压、逐块检查取消状态，并在 declared size+1、CRC 或 ratio 异常时拒绝。part
  byte buffer、parser-retained 结果与 image codec working set 使用独立许可；解析完成先释放
  buffer，重复图片先销毁临时 Vec 再 shrink，唯一图片只把实际 capacity 与 IR/index 许可转交
  opaque output lease。
- 本地输入打开在 handle 层拒绝 Unix symlink 或 Windows reparse point，并只接受 regular
  file，不能把 CLI 规划时的路径检查当作最终安全边界。
- XML streaming 解析禁用 DOCTYPE、DTD、自定义/外部实体与网络 resolver，仅接受五个预定义
  实体和合法 numeric character reference；namespace 作用域、重复 expanded attribute、
  closing tag、深度、事件数、属性/文本与扩张预算均在构造 IR 前校验。
  PresentationML 的 `mc:AlternateContent` 对全部分支做上述安全 preflight，但语义解析只选择
  第一个 `Requires` 前缀全部映射到已理解 URI 的 Choice，否则选择唯一 Fallback；未选分支的
  relationship ID 不会授权或 materialize 目标。
- RSS/Atom 复用同一 XML 解码、XML 1.0 character 与 entity 边界，并要求 RSS 2.0/Atom 1.0
  root、namespace 和 channel/entry 结构的强证据。`xml:base` 与相对 URL 只做离线数据解析；
  entry link 不触发请求，nested HTML 统一进入 HTML 安全转换器。Feed 对 entry、事件、深度、
  累计文本、nested HTML、asset、diagnostic、IR、字符串与输出采用贯穿解析、去重和合并阶段的
  聚合逻辑内存预算，长循环持续 checkpoint；DTD、外部 entity、namespace 混淆、Atom XHTML
  foreign element 与 active content 均 fail closed，过滤后的原始 markup 不会作为 fallback 回显。
  nested HTML 使用固定 html5ever 0.39.0 / tendril 0.5.1 source commit 模型，在 parser 构造前通过无堆扫描
  保守预付 tokenizer、TreeBuilder、8 轮 adoption-agency、tendril 与 DOM workspace；Feed-owned
  lease 贯穿 parser、DOM 和最终 Document。预付是协作式逻辑上界，并非 allocator metadata 或
  进程 RSS。fragment 失败只在局部对象全部析构后回滚完整预算快照，避免失败片段留下幽灵计费。
  Feed XML 的 element/attribute expanded-name、解码值、`xml:base`/URL、diagnostic 及 Atom XHTML
  序列化同样使用该 lease：自有 Vec/String 在增长前预留目标 capacity，并在 allocator 返回后按
  真实 capacity 补差。属性不进入另一个未计费的树/集合；XHTML 逐事件写入预算化 String，CDATA
  与 attribute escaping 先无堆计算扩张长度再申请，失败对象析构后恢复完整事务快照。
- Markdown 解析固定离线，不读取相对图片、不获取 HTTP(S) 图片、不解码 data URI。
  external-only 图片 URI 必须是 canonical HTTP(S)，且没有 userinfo、query 或 fragment；
  该 URI 只进入 IR/Markdown，转换过程不会访问网络。远程 SVG 额外产生 active-content
  诊断，因为后续 Markdown 消费者主动打开链接时适用消费者自身的网络与 SVG 安全模型。
  raw HTML 与 blockquote 只进入不可执行代码降级节点，危险标签不会作为 HTML 执行。
- HTML 使用 HTML5 容错树构造，但转换器从不执行脚本、事件处理器、CSS、SVG/MathML 或表单
  控件，也不调用网络。`script`、`style`、`template`、`noscript`、隐藏/inert/aria-hidden 内容
  在语义遍历边界整体丢弃；SVG/MathML 内部链接和图片不能穿透为资源。`base` 只解析引用，
  不代表网络授权；外部图片仅作为 canonical HTTP(S) audit Asset，bytes 为空且不会自动 fetch。
  HTML 输入、tree event、DOM node、nesting、IR inline/node、table 与自有逻辑内存分别受硬限制
  并定期 checkpoint。parser logical work 是协作式预算，不声称覆盖 html5ever 内部 allocator、
  metadata 或进程 RSS。预算错误后 TreeSink 进入 poisoned 状态，后续回调保持 O(1)、不分配且
  不改变树，最初的 limit/cancel/deadline 错误保持权威。
- RTF 使用自研有界状态机，不调用 Office、OLE、HTML 或脚本执行器。object、objdata、filetbl、
  datastore、active/ignorable destination 只按 brace 结构跳过；field instruction 只允许
  canonical HTTP(S) hyperlink 数据变换，绝不授权网络。PNG/JPEG pict 在保留前受尺寸、
  单项/总资源、完整像素解码和请求内存预算约束；EMF/WMF 不解析。group、control、数字、
  Unicode fallback、decoded text、IR/table/asset/diagnostic 与 heap capacity 均有 checked
  hard limit，并在长扫描循环 checkpoint。font table 最多接受 4096 项，使用分配前计费并按
  实际 capacity 补差的 `Vec`；destination 结束后原地排序/去重，正文只做 binary search。
  容器内 RTF helper 不接受 `Services`，不能重建 context 或重置 limit，返回值继续持有同一
  request memory lease。
- TXT 自动探测按候选字符集增量解码完整输入；除 TAB、LF、CR 外，NUL、C0、DEL 或 C1
  都会拒绝自动候选，多字节编码不能借原始字节形态绕过规则。BOM 仅决定候选编码，
  不能绕过完整控制字符扫描、有界严格解码与文本安全阈值。结构化文本、具备三行及
  表头/数字列证据的 CSV/TSV 启发式和
  已知二进制 magic 优先。传统字符集只允许固定 allowlist，不能把任意字节流作为
  `windows-1252` 静默吞入。replacement 不是静默容错，必须按 decoder 实际错误数插入
  U+FFFD；相邻错误可合并诊断，但每条诊断都必须保留原始 byte range、编码名和替换数。
- TXT 转换器在创建节点前以 checked arithmetic 核算 block、inline 与解码文本预算，
  超限直接返回 `resourceLimit`，不能依赖 Engine 的 IR 验证把预算错误改写为内部错误。
- CSV/TSV 把 `= + - @` 开头的字段保留为普通文本，不解释或执行公式。解析循环定期检查
  取消与 deadline，并在构造 IR 前检查输入、行、列、单元格、单字段、文本、inline、
  node 与内存预算；畸形引号和不等宽记录返回受控错误。
  内存计费使用逻辑 heap capacity：`String` 的请求字节容量与 `Vec<T>` 的请求元素槽位在
  allocation 前通过同一个 RAII reservation 增长；不包含 allocator metadata 与 size-class
  slack。ResolvedInput 的只读 `Arc` 由 Engine lease 或 API 调用者持有，converter 不复制也
  不重复计费；无论输入来自 Engine 还是第三方调用者，字符集选择、64 KiB probe sample、
  decoder、压缩 byte map、record/field 缓冲及 Table IR 的每项新增 allocation 都先计费。
  临时解码缓冲仅在 allocation drop 后退款，转换 guard 保持到 IR 构建完成。
- JSON 格式保护使用非递归状态机扫描完整输入，定期执行 `ExecutionContext` checkpoint，
  nesting 的 checked 上限独立返回 `resourceLimit`；`\u` escape 还必须验证 UTF-16
  surrogate pairing。不能用递归 DOM parse 作为格式 guard。
- 只有策略允许时，Office 宏和内嵌可执行文件才能作为惰性资源保留；它们永远
  不会被执行。
- XLSX/XLSM/XLSB 在调用 Calamine 前由自有 OPC、XML 与 BIFF12 扫描器认证解析器可见的
  条目、记录、字符串、样式、公式、图片与 worksheet 范围，并核算它们和累计 IR、验证、
  provenance 共存的内存峰值。特别地，XLSB 公式读取会按 Calamine 0.36.1 的
  `BrtWsDim.len().min(1_000_000)` 暂态容量单独计费，即使声明范围陈旧且实际 sheet 为空。
  Calamine API 是同步调用，无法在单次调用中途抢占；转换器在构造 parser 以及每次 values、
  formula 和 merge 调用前后执行 checkpoint。因此取消/timeout 的最坏延迟是一段已经由前置
  扫描和资源上限约束的同步调用，而不是整本 workbook；高敌意部署仍应在具备 OS 内存与
  时间硬限制的隔离进程运行。
- PDFium 仅从认证路径加载 authority 中精确 hash/size/ABI 的当前平台制品。Windows CLI 的默认路径
  由已经规范化的可执行文件位置推导为 `lib/pdfium/pdfium.dll`；显式 `PDFIUM_LIBRARY`、
  `with_runtime_path()` 与 resolver 结果进入相同的 pinned loader。加载前逐一检查所有已存在路径
  component，任何 symlink 或 Windows reparse point 都 fail closed；不搜索当前目录、system PDFium、
  `PATH` 或动态加载器 fallback，也不联网。所有 native count/length、页、字符、
  object、link、图片 stride/format/pixel bytes 与 render 尺寸在 Rust 分配或 slice 前有界检查，
  文档打开使用消费式 preflight plan：输入 `Arc` 不复制，密码的 NUL 结尾临时副本按
  `password.len()+1` 的 exact boxed backing 在 materialize 前计入请求 memory credit，超过限制或
  含 NUL 时不会调用 native loader。runtime snapshot 的固定、已审计制品加载内存属于进程级
  loader 生命周期，不混入请求 credit；
  句柄受 Rust 父子生命周期约束且 native 调用串行化。转换循环在页、图片与编码行执行取消/
  deadline checkpoint，图片和页面 render 同时受 PDFium 上限、`ExecutionContext` memory、
  单 asset 与总 asset 上限控制。转换器把 IR/asset 的 live memory lease 随 `ConverterOutput`
  转交 Engine，并继续随 `ConversionResult` 保持到调用方释放。Engine 在 converter/renderer 调用前
  取得经过同一 context 认证的 preflight reservation，并按完整 retained IR、asset、diagnostic、
  Markdown 与 provenance inventory 缩减/转移，避免交接空窗或重复计费。URI 仅作为经 allowlist
  校验的数据，不触发访问。
  但 PDFium 仍在本进程解析不可信 native PDF；私有 runtime snapshot 只保护供应链和加载竞态，
  并不是 PDF 解析 sandbox。高敌意部署仍应在具备 OS hard limit 与 sandbox 的隔离进程运行。
- 网络访问默认关闭，HTTP(S) source 在未显式传入 `--allow-network` 时不会进入 DNS；
  回环、私网及其他非全局地址还必须由同一次调用传入 `--allow-private-network`。配置与
  CLI host allowlist 先取规范化交集，resolver 在每次重定向逐跳重新检查 host、DNS
  返回的全部 IP 和端口；连接只使用该次检查所得的 exact IP，而 HTTP `Host` 与 TLS SNI
  保留原 canonical host，避免 DNS rebinding。HTTP 明文只允许已双重授权且全部地址均为
  私网的目标；HTTPS 固定使用随程序审核的 WebPKI roots，不读取代理、系统证书、`PATH`
  或相关环境变量。
- HTTP/1.0/1.1 response parser 拒绝 C0/DEL、obs-fold、空或非法 header name、重复敏感头、
  `Content-Length`/`Transfer-Encoding` 冲突、chunk extension/trailer、未知 transfer/content
  encoding 和主动内容语法。客户端只声明 `gzip`；wire bytes、header、chunk、解压后 source
  bytes 与所有中间/final backing 都受请求预算和总 deadline/cancel 控制，超限立即断开，
  不创建缓存或完整临时文件。重定向只接受 301/302/303/307/308 的相对或 HTTP(S)
  `Location`，限制跳数并检测 cycle；每跳重新授权，不携带 Authorization 或其他凭据。
  source URL 可以包含签名 query 以支持受控下载，但 metadata、redirect provenance、错误、
  日志和诊断只保留移除 userinfo/query/fragment 后的 canonical URL。
- `Content-Type` 只作为格式提示；服务器文件名仅接受严格 quoted/RFC 5987 UTF-8、NFC、
  长度受限的 portable name，并拒绝路径、设备名和控制字符。扩展名不能覆盖 magic/container
  检测。HTML、DOCX 等文档中的外部链接仍只是数据，不会触发第二次 source 网络请求。
- Wikipedia 自动识别只覆盖受限 `*.wikipedia.org` 的根 `/wiki/<title>`；通用 MediaWiki 必须
  使用 `mediawiki+http(s)` 显式 opt-in。API redirect 即使逐跳通过公共 SSRF 策略，最终仍须保持
  原 origin 与 `/w/api.php` endpoint，且响应必须声明严格 JSON MIME。专用 detector 还要求
  MediaWiki resolver 在验证后附加的进程内 identity；transport 会先剥离全部服务器 MIME 参数，
  普通 HTTP `/w/api.php` 响应即使回送同名参数也只能进入通用 JSON 检测。typed JSON parse 前的
  checkpointed shape pass 拒绝全部层级的重复键、过深/过宽集合和超长字段；resolver-owned URL、
  policy、redirect metadata 与 body 分用持续 lease，失败、取消和 handoff drop 均释放完整计费。
- AI 响应是不可信的结构化输入，必须验证补丁或 Schema、溯源、节点引用和资源。
- 可恢复任务 checkpoint 只使用规范随机 token 定位本地普通文件；Unix store 持有根
  目录 handle 与 dev/inode identity，所有阶段 open/link/unlink 均为相对 no-follow 操作，
  root/祖先替换不能重定向 I/O。最终 root 还须通过已打开 handle 的 `fstat` 确认由
  当前 euid 拥有且 group/other 零权限；已有 0750/0777 root 直接拒绝而不静默
  `chmod`，公开祖先不受此限制。每次公开 store 操作前后重验 handle owner/mode，因此
  打开后的权限放宽也会 fail closed。恢复前重新计算输入与完整转换配置指纹；token 持久锁
  保证并发调用只有一个 winner，其他调用返回该持久结果。未知 schema、截断 JSON、阶段
  与 payload 不一致、伪造成功历史均返回稳定 `recovery` 错误。阶段通过同目录私有临时
  文件、文件 `fsync` 和 no-replace hard link 发布；临时残留永远不代表成功，写入同时
  受请求 temporary budget 与 2 GiB 上限约束。固定 4 KiB 状态尾块允许 payload-free
  inspect；完整读取在 typed serde 前做 size/depth/width/value 预检并预留原始、字符串和
  结构内存。资源字节使用声明解码长度的规范 padded base64，在分配前验证编码、
  单资源与总资源上限，typed wire/base64/解码字节的共存峰值也受同一内存预算。
  恢复 succeeded 还会重验资源 ID/MIME/外部 URI、嵌套引用、diagnostics、
  provenance，并重渲染逐字节比较 Markdown。整个协议不访问网络。
- 模型通过 HTTPS 下载并固定 SHA-256，同时携带许可证元数据。
- OCR 检测不接收编码图片，只接收带 checked width/height/row stride/格式/方向的借用
  像素视图；像素数、stride 乘加、tensor shape/元素数、概率范围、contour 总点数、
  candidate 数、累计 score pixels/work 和 polygon offset 点数都在使用前后有界，
  NaN/Infinity 和 `[0,1]` 外概率稳定拒绝。概率验证、bitmap 构造、score 扫描、长预处理
  与候选循环执行协作式 checkpoint。独立图像转换器对 PNG chunk/CRC、JPEG marker/entropy、
  WebP RIFF/frame、BMP file/DIB/pixel range、TIFF/BigTIFF IFD/strip/tile range 做完整 envelope
  审计，所有长扫描每 4 KiB checkpoint 后才进入固定 Rust decoder；ICC/profile、Exif/XMP
  自由文本和 active payload 均不执行，仅提取受界数字方向与 DPI。调用 `imageproc`/`clipper2-rust`
  前按 model pixels 和最大几何结构保留请求逻辑内存，并把 tensor reservation 保持到
  runtime 与后处理结束；这只表示请求 heap capacity 的保守逻辑计费，不是 allocator
  metadata、RSS 或防止系统 OOM 的声明。边界扫描一次只构造一个 contour，每次扩容
  前增长 reservation，释放对应 Vec 后才退款；扫描行、边界跟踪和 minimum rectangle
  工作都执行 checkpoint，恶意小岛受 contour event 硬上限约束。round offset 在第三方
  调用前后 checkpoint，并以固定四点输入、104 点输出、108 个 header 和 104² work
  上界预留；调用内部不可轮询，但该常量上界限制单次延迟。算法阈值、候选数和
  unclip ratio 只来自内嵌 authority，公开配置只能收紧资源上限。尺寸被限制在不会溢出
  `imageproc` i32 orientation arithmetic 的范围；实现不使用 panic 捕获冒充内存安全。
- OCR IR merge 把 detection/recognition 数量、唯一 `source_index`、页码、provider/model
  identity、有限凸四边形与两类 confidence 当作不可信输入。page/region/text/identity、
  Document node/inline、canonical NFC string、native span index 和 comparison work 都在
  建立候选索引前 checked 计数；Document validation working set 也在 validator 分配前由
  同一 `ExecutionContext` 保留。候选按 X interval 剪枝，配置的 comparison 上限同时覆盖
  OCR 重叠去重、native span 比较和 line clustering 的保守上界。循环周期 checkpoint；
  算法不写临时文件，失败时局部 Document、索引、诊断和 reservation 一并释放，调用方
  不可能观察 partial publish。原始 polygon 与 ordered evidence chain 只进入结构化 IR/DTO，
  Markdown renderer 不输出 model identity。
- ONNX Runtime 只接受调用方从 Bazel runfiles 得到的显式绝对路径和受信根；不查询
  cwd、`PATH`、`LD_LIBRARY_PATH`、`DYLD_*` 或其它隐式环境。路径逐段拒绝 symlink/
  reparse point，源文件以 no-follow handle 打开并核对文件身份，再按 authority 中的
  解包动态库 SHA-256 复制到进程私有目录。只有该私有副本会进入动态加载器；加载后
  `GetVersionString` 必须与 authority 版本完全相等，且 `GetApi` 必须接受 authority 的
  C API level。authority 同时记录固定文件大小、格式、架构、SONAME/install name、
  `NEEDED`/`LC_LOAD_DYLIB`/PE import、RPATH/RUNPATH 和 companion 的相对路径与哈希。
  loader 在 `dlopen`/`LoadLibraryExW` 以及任何 native constructor 前用受审 `object`
  parser 对实际主库做有界解析，与 authority 双向精确比对；只接受私有目录语义明确的
  `$ORIGIN` 或 `@loader_path`，当前官方 CPU 包没有 companion。Unix 同时拒绝 loader
  环境变量，并以已加载对象的真实 SONAME/install name 和文件哈希审计冲突，而非按
  basename 推断；私有目录只含主库，Windows 使用
  `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`。主库 load identity
  已在 worker 中出现时同样拒绝，防止 loader 复用任意同名对象。父进程不执行 `dlopen`
  或持有 ORT API；动态句柄、API table、禁用 telemetry 的环境、session 和 tensor 全部
  由隔离 worker 按严格析构顺序持有。所有 FFI unsafe 集中在
  `into-markdown-onnxruntime` crate，并启用
  `deny(unsafe_op_in_unsafe_fn)` 与逐块 SAFETY invariant。
- 模型 source archives 与最终 runtime files 分开建模，缺少最终文件、字符表、大小、
  平台或许可证审核时禁止安装。运行时清单必须与权威下载清单逐项一致。归档 transport
  必须交付原始 TAR 并声明 acquisition identity；管理器流式双验归档与成员大小/SHA-256，
  同时要求条目顺序、路径、类型、header checksum、padding、两个结束块和 trailer 精确。
  traversal、symlink/hardlink、重复/未知条目、截断与 size bomb 在 staging 发布前拒绝。
- 模型写入使用同文件系统 staging、逐文件大小/哈希校验、`fsync`、版本化持久 journal
  与锁内原子切换；install/remove 或显式恢复会在锁内恢复中断事务，查询与校验只读并在
  发现未完成事务时 fail closed，损坏或歧义 journal 一律 fail closed；
  journal 先完整写入并同步根身份绑定的临时文件，再以 no-replace rename 发布；Windows
  x86_64 在固定本地 NTFS/ReFS 上持有受保护 DACL 与物理 FileId 绑定的根 handle，使用
  `MoveFileExW(MOVEFILE_WRITE_THROUGH)` no-replace 发布，并将待删对象先原子退休到唯一
  garbage 名称后再 best-effort 清理；
  校验和删除拒绝符号链接及非普通对象，随包只读模型不能删除。取消、超时、临时空间
  预算和内存预算由统一 ExecutionContext 执行。
- OCR 识别只消费检测器给出的 raw-source `CropDescriptor`，不会再次应用 EXIF orientation。
  四点轴、面积、单 crop pixels 和宽高比先校验；官方 cubic/replicate 透视中间图、resize
  tensor、排序槽位、runtime 输出和 CTC String/Vec 均在分配前以 checked arithmetic 预留。
  batch width 上限 3200，输出只接受 finite float32 `[N,T,6906]`，class/timestep/decoded bytes
  具有独立硬上限。所有长循环、batch 和 runtime 前后执行 cancellation/deadline checkpoint；
  任一失败不写缓存、不返回部分识别结果。
- 日志和诊断不得包含文档原始字节、凭据、签名 URL 或不受限制的提供者载荷。

资源限制错误是权威错误，绝不能作为可恢复的解析器错误被吞掉。

## 输出资源与文件系统

- 资源名称只来自完整内容 SHA-256 与 MIME allowlist 扩展，不采用源文件路径、Unicode
  名称、盘符、UNC、ADS 或保留设备名。相同内容的 MIME 声明冲突会失败，不按扩展猜测。
- `asset_uri_prefix` 仅接受 portable 相对 URI path；拒绝绝对路径、scheme-relative、
  query、fragment、控制字符与 scheme。`data:` 仅由 embed renderer 内部生成。
- CLI 在变更目标前完成路径、冲突、同文件系统与符号链接检查，并把主产物及资源完整
  stage 和 fsync。覆盖目标由 no-follow handle 确认为 regular file 并复核身份；目录、
  FIFO、设备、符号链接和 Windows reparse point 均拒绝。
- 输出事务只恢复带随机 nonce、固定签名、版本、精确 root/相对目标清单和排他锁的
  私有 registry 条目。每个物理目标父目录包含固定名称、由 no-replace hard link 发布的
  管理器 lease，绑定父目录 dev/inode、root 身份和事务内受签名标记；既有目标身份另绑定
  dev/inode，缺失目标保守互斥整个父目录。相关写出通过已认证父目录 handle 读取 lease，
  不扫描祖先或不相关目录；恢复后有界重做完整预检，超限返回 `recoveryLimit`。Unix 变更只使用
  已认证目录 handle 上的相对 `*at` 操作；Windows 输出事务稳定返回
  `componentUnavailable`。journal generation 的每次转换都在继续文件变更前
  持久化；未提交事务恢复旧集合，已提交事务验证完整新集合。恢复失败保留 journal 与
  备份并返回 `rollbackFailed`，不会递归删除目录或触碰相似名称路径。
- bundle manifest schema 2 的 `sourceAssetIds` 为每个 Document 资源 ID 提供唯一物理
  映射；ZIP 条目与 manifest path 双向一致，portable path 比较包含大小写折叠规则。
取消与总 timeout 同样贯穿每个 SPI；长循环和外部服务等待必须设置协作检查点，不能
依赖引擎外层检查来中断内部工作。

ONNX session 固定使用 CPU provider、顺序图执行、显式 intra/inter-op 线程数和关闭的
memory pattern；CPU arena 默认关闭并可经 C API 显式设置。模型 authority 必须给出
保守的 session 与每次 run 上界；session 上界在 loading 前按 live count/bytes 计入缓存，
不会因仍在使用的条目被移出 LRU 而提前释放。这些是请求/缓存的协作式逻辑预算，不声称
测量 ORT 物理 RSS。除此之外，Linux/macOS worker 在任何 authority/model/ORT load 前安装
并复核 `RLIMIT_AS`，Windows worker 在 suspended 状态加入 process-memory hard-limit、
kill-on-close Job Object 后才 resume；任何无法证明限制已生效的平台稳定返回 component
unavailable。平台 address-space ceiling 与模型 session/run、`max_session_bytes` 相互独立。
macOS ARM64 的 1 TiB ceiling 用于容纳 dyld/shared-cache/allocator 的稀疏虚拟地址预留，
不表示 RSS 或物理可用内存；约 8 TiB Expand fixture 验证攻击输出明显高于该硬边界。
`ExecutionContext` 在克隆输入前预留输入值/shape 副本、输入槽位与 ORT backing，在执行前
按输出 contract 的最大 shape 预留 native 输出、返回值/shape 副本，并全程持有 run scratch
预算；contract metadata 的名称、shape 和结构容量计入 session 上界。输入/输出各最多 64
个，名称最多 256 UTF-8 字节，rank 最多 16；所有元素数和字节数用 checked arithmetic。
worker 直接经 `ort-sys` C API 先把 IO count 读入标量，名称读入固定 257-byte allocator，
rank/dim 读入 `[i64; 16]`，检查后才分配 metadata。ORT 返回后同样先把输出 rank/dim 读入
固定栈，在任何 `GetTensorMutableData`、Rust slice/`Vec` 或值复制前验证 Exact/Dynamic
上界、元素数和字节数；越界时直接释放 native tensor。无最大值的动态维度不是合法
contract。IPC 具有固定 header、version、单调 request id、消息数/载荷/模型/tensor 上限；
stderr 最多保留 64 KiB。创建前后、等待 single-flight、IPC 等待和推理前后均检查取消和
deadline；失败会 kill 并 wait/reap worker，不留下孤儿进程。GraphProto IO 在 prost decode
前经字段数、长度、count、rank 和递归深度有界的 wire preflight，并与 authority/native
metadata 三向精确核对。

## TaskStore 数据与路径边界

TaskStore 复用 UI/RecoveryStore 的 Unix 私有目录策略：路径规范为绝对路径，从 `/` 的目录
句柄逐组件 `openat(O_DIRECTORY|O_NOFOLLOW)`，缺失目录只以 `0700` 创建；已存在 caller 目录
绝不 chmod。最终 root 必须由 effective uid 拥有且拒绝 group/other access。数据库、WAL、SHM、
journal、临时和 backup 必须是同一 root 内当前 uid 的私有普通文件。主 DB 与 backup SQLite
connection 均使用 `SQLITE_OPEN_NOFOLLOW`；连接保存主 DB dev/ino，
每个公开读写操作前后复核 root namespace、权限、主 DB identity 与 companion file 类型；
symlink、权限放宽、root/db 替换均 fail closed，并在 mutation 前拒绝。

SQLite 本身仍按 pathname 延迟打开 WAL/SHM；本实现确保它们在 `open` 配置 WAL 时创建并在
后续调用前复核，但没有自定义 SQLite VFS。因而同 uid 恶意进程在 pathname open 与复核之间的
极窄竞态，以及 backup pathname open 与 identity 后验之间的 same-uid race，属于当前用户信任
边界，不能描述为对同 uid attacker 的完整 capability confinement。
跨 uid/public directory、symlink 和静态 namespace 替换由 retained handle 检查阻断。

Windows build 保留类型/API，但 `TaskStore::open` 明确返回 `PlatformUnavailable`：尚未实现能
同时证明 DACL 私有性、reparse-point 拒绝和 SQLite companion identity 的 VFS/handle 路径，
因此不沿用 UI 目录检查后虚报安全。Linux x64/ARM64 与 Windows x64 可静态交叉编译；只有有
对应 runner 时才声称 native filesystem 行为验证。

secret redaction 使用结构化 allowlist。schema 没有 provider key/token、Authorization、环境
来源、URL/query 或自由文本 diagnostic 字段；input/artifact location 仅接受固定长度 opaque
hex reference。`ConfigurationSnapshot` 拒绝未知字段。测试以 canary 尝试加入 `apiKey`，并
扫描 database 与存在的 WAL/SHM 字节确认未编码。

## Fixture 语料安全边界

Fixture 语料只允许 `fixtures/small/` 下的小写 ASCII portable 相对路径；审计拒绝绝对路径、
父目录跳转、反斜杠、盘符、NUL、非 ASCII 字符、重复 ID、symlink 和非普通文件。
每个文件在执行测试前核对 manifest 中的大小与 SHA-256。malicious 样本均由仓库安全合成，
不含真实凭据、个人数据或外部可访问秘密；DOCX 外链使用 `.invalid` 保留域，关系被文档正文
真实引用，并通过注入 OCR/transcriber/AI 服务计数器验证转换期间零可选服务调用；
HTML/XML/notebook 活动内容只验证解析与惰性展示边界。

大 fixture 输入不在普通依赖图中。显式 manual target 只接受固定 HTTPS URL、单一 host
allowlist、不可变 SHA-256、大小且固定拒绝 redirect；普通测试、license check 和 release
audit 都不得触发下载。release audit 进一步要求这些 `fixture-input` inventory 项保持
`included_in_release=false`，避免测试生成工具或模型意外进入产品归档。

## OpenAI-compatible Provider 传输

Provider 配置只保存环境变量名。命令完成 URL、host allowlist、DNS 结果和每一个地址的
公网/私网分类后才读取环境变量；空值、非 Unicode、控制字节和超限值均拒绝。密钥由不可
Clone、不可 Debug/Display 的请求局部对象持有，Authorization 请求缓冲和密钥在释放时清零，
错误、JSON 输出、日志和配置没有响应自由文本或密钥字段。

传输固定使用 Rustls 0.23.32、ring、socket2 0.6.5 和 webpki-roots 1.0.3 的 Mozilla 根集合，四个目标平台
使用同一根策略；传输库本身不读取平台证书库、HTTP(S)_PROXY、PATH 或代理环境变量。DNS 在有界工作
线程池及有界队列解析；空结果、超量/超容量/端口不符结果，或同时包含公网与未授权私网地址
均 fail closed。连接只使用已检查的具体
SocketAddr，Host 与 TLS SNI 使用同一个 canonical hostname。HTTP 只允许已显式双重授权的
非公网地址；公网 Provider 必须使用 HTTPS。重定向和 protocol upgrade 均拒绝，因此
Authorization 不会跨 origin 转发。

官方能力插件下载是唯一使用该代理入口的产品流程：操作者可用 `INTO_MD_HTTPS_PROXY`（回退到 curl 风格的
`HTTPS_PROXY`、`https_proxy`）显式注入一个 `http://[user:pass@]host:port` 的 CONNECT 代理，
并用 `INTO_MD_NO_PROXY`（回退 `NO_PROXY`、`no_proxy`）按 `*`、精确 host 或域后缀豁免。
代理仅承载策略已独立授权并解析的 HTTPS origin 隧道；TLS 仍在 origin 端到端终止，SNI 与
证书校验对象不变，WebPKI 根集合不放宽，代理凭证只进入 `Proxy-Authorization` 头且在任何
输出与 provenance 中脱敏。明文 HTTP origin 永不进入隧道，保持原直连公网限定路径；库的
确定性测试不受环境变量影响（默认客户端行为不变）。代理响应头复用同一严格 HTTP/1.1
parser，2xx 以外的 CONNECT 应答、head 后的提前隧道字节（smuggling）与畸形应答均稳定拒绝。

HTTP/1.1 parser 对 header、header 数量、Content-Length、chunk、压缩体、解压体和 JSON
结构设硬限，拒绝 obs-fold、重复长度、CL/TE 冲突、未知 transfer/content encoding、trailer、
非法 charset 和 gzip bomb。socket 轮询、DNS、connect、TLS、retry wait、header/body 读取均
检查 ExecutionContext 取消与总 deadline。GET 能力探测以及携带显式 idempotency key 的 POST
才允许对 429/5xx 有限重试；Retry-After 只接受有界 delta-seconds，缺省退避使用有界
指数延迟和可注入测试的进程内 jitter。`providers test` 固定只发
`GET /models`，不包含用户文档或 prompt；服务端字段不能扩大本地配置的 capability allowlist。
`/models` 只能证明模型存在，不能证明图像或修复能力，因此不会仅凭配置声明这些能力；声明
存在后续页的列表返回稳定 incomplete 错误。Chat Completions 与 Responses 使用相互独立的
REST DTO；Responses 图像输入使用字符串 `image_url`，输出只读取
`output[].content[].type = output_text`，不信任 SDK convenience 字段。

配置变更通过已认证父目录句柄执行 no-follow 临时文件创建、文件/目录 fsync、目标与临时文件
identity 复核及 fd-relative rename。父目录、目标、临时文件或符号链接竞态均 fail closed；
替换已有配置时保留其权限，并要求文件属于当前用户。
## Web 上传、队列与产物状态机

浏览器文件名仅是最多 255 bytes 的 display metadata；分隔符、控制字符、`.` 和 `..` 均
拒绝，永不解释为自由路径或 URL。请求 body 逐 chunk 消费，每次实际写入前检查单文件
512 MiB 与 managed root 14,356 MiB ceiling；断连会 drop guard 并清理未绑定任务的 incoming
capability。数据库不保存输入正文或 Provider secret。

durable 状态机是 `pending -> running -> converted -> succeeded`。RecoveryStore 的
converted/succeeded checkpoint 只证明 conversion payload，最多把 TaskStore 恢复为
converted，不能独自证明外部产物已提交。Markdown、完整 Document IR、结构化 diagnostics、
assets 和 bundle 在同一 task 私有 stage 内逐文件 fsync；bounded manifest 同步后，以
no-replace rename 和父目录 fsync 一次发布。只有每个 opaque key 的 size/SHA-256 与实际
普通单链接文件一致，TaskStore 才事务性索引并进入 succeeded。重启对 running/converted
重放 publication；若 durable succeeded 的 published set 缺失、额外、重复或摘要不符，
服务 fail closed。下载先在同一配额内复制并同步验证 TaskStore byte count 与 SHA-256，随后
unlink 临时名称，只从 retained 匿名只读 fd 流式响应；原 published inode 的后续覆盖或 rename
不会越过校验，响应断连/Drop 会释放匿名 inode 的精确 quota charge。

通用 TaskStore 的 `succeeded` 继续表示 RecoveryStore checkpoint 已 durable，不把旧 schema
记录重解释为 Web publication。schema v1/v2 中缺少 AssetId/filename/media-type 的 legacy asset
会无损迁移并保留 `None`；v4 另记录首次进入 terminal 状态的时间，固定操作不会重置保留年龄；
新写入必须带完整 canonical metadata。Web 后端另在成功、重启、
查询与下载边界要求恰好一份 Markdown/Document IR/diagnostics/bundle、最多 124 assets、唯一
storage key/AssetId；通用 TaskStore 累计最多 2 GiB，Web profile publication 累计最多
512 MiB，且 manifest 与 TaskStore 逐字段相同。损坏的非成功
published set 只在 descriptor-bound 验证所有成员为私有普通单链接文件后 quarantine 删除，任务
稳定为 failed；持久化本身不可用时停止 dequeue，worker 不 panic，也不伪造成功。

队列以接收/恢复顺序 FIFO dequeue，最多四 worker 并行；每项有独立 cancellation token、
30 分钟 deadline 和 Engine ResourceLimits。单项 error/panic 隔离为 failed。最后一个 backend
handle drop 会取消在途任务、唤醒并 join 全部 worker。启动前只清理 canonical incoming/stage
残留；遇到额外成员、symlink、special file 或外部 hardlink 会拒绝而非递归删除。

Web profile 把 request memory 与 temporary 上限约束为 256 MiB、总 assets 为 128 MiB；每个
执行任务在首次 TaskStore 状态 mutation 前保守预留 1 GiB payload 加 4 MiB metadata headroom：
两个可同时存在的 checkpoint 各由 256 MiB temporary ceiling 约束，Web publication 另限
512 MiB，TaskStore/WAL/manifest bookkeeping 另留 4 MiB。lease 覆盖转换、RecoveryStore
提交、stage、rename 与最终 TaskStore transition；每个 lease 释放时，即使仍有其他任务在途，
也在 quota mutex 内重新 descriptor-bound 测量 managed tree；若并发 publication 使全树测量
暂时不可用，则把该 lease 的完整计划计入 `used`。这样未计入
`used` 的在途写始终由 `reserved` 覆盖，持续满载 worker 也不能复用已落盘 reservation，
全局 ceiling 精确由 10 GiB retained history、四笔各 1,028 MiB 的 worker reservation 和
4 MiB SQLite headroom 组成；同时四个任务可取得完整保守运行资源。quota wait 使用有界 timed wait，
计入 30 分钟总 deadline，并在 cancel/owner shutdown 时唤醒；真实清理会通知等待者，清理失败
不虚减 quota。

14,356 MiB managed ceiling 中永久保留 4 MiB 给 TaskStore/SQLite WAL；每次没有 execution lease 的
create 或 terminal/recovery mutation 在 quota mutex 内串行预留 1 MiB；没有 execution lease 的
exact-set success reconciliation 预留完整 4 MiB。每次写入前都以 retained root 重测物理占用与
所有活跃 reservation，写后再次重测且验证实际增长不超过 reservation。
admission failure 的批量收敛遇到首个 reservation 或持久化错误就停止 backend 和后续 mutation，
不会吞错后继续消耗应急空间。
upload、snapshot 和 conversion lease 只能消费其余 data ceiling。这样 dequeue 前取消、quota
等待取消/超时、恢复缺对象，以及 admission failure 的 Failed/Cancelled/Interrupted transition
在 data ceiling 已满时仍有物理写入空间。每次这类 transition 后立即在 quota mutex 内重测
managed tree，把实际数据库增长转入 `used`；重测失败则保守计入完整 metadata headroom，令后续
data reservation 只能使用扣除该最坏增长后的余额，不能重复消费应急空间。

HTTP upload 具有 30 秒 idle 与 30 分钟 total deadline，并监听 server shutdown；artifact response
具有相同 total deadline和 shutdown cancellation。每个 response 另有独立 timer 持有可撤销的
snapshot slot；即使慢 reader 填满 socket 后 runtime 不再 poll Body，timer 仍会在绝对 deadline
或 shutdown 时关闭 snapshot capability 并归还 quota，后续 Body poll 得到 timeout。graceful
shutdown 最多等待 5 秒，随后 drop server future 与慢连接，使 Upload guard、匿名 artifact
snapshot、quota charge 和 backend worker ownership 都能有界释放。

TaskStore/RecoveryStore 继续继承其组件文档化的 same-uid pathname 打开窄竞态：Web backend
在打开前后用已持有 root capability 复核 namespace identity，发现替换便拒绝采用该 store；这
保证 backend 不会跨两个 namespace 继续运行，但不宣称 pathname-based store open 在攻击窗口
内绝无短暂文件系统副作用。完整消除该窗口仍需 store 自身提供 descriptor-relative open API。
