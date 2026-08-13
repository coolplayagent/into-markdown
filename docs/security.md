# 安全模型

本地 Web 入口的权威威胁边界与请求校验规则见[本地 Web 服务](ui.md)。该入口只监听
IPv4 回环地址，不把转换网络授权扩展到 Web 服务，也不接受配置文件提供监听地址或
会话秘密。

文档、归档文件、标记语言、媒体、模型文件、提供者响应和 URL 均为不可信输入。

- `ResourceLimits` 限制输入大小、解压后字节数、归档条目数、嵌套深度、页数和
  保留资源数，并限制实现显式计费的内存与请求临时文件字节数。所有累加使用 checked
  arithmetic，临时文件由执行上下文负责 RAII 清理。
- 归档路径必须规范化，拒绝路径穿越和绝对路径，解压时不得跟随符号链接。
- 本地输入打开在 handle 层拒绝 Unix symlink 或 Windows reparse point，并只接受 regular
  file，不能把 CLI 规划时的路径检查当作最终安全边界。
- XML streaming 解析禁用 DOCTYPE、DTD、自定义/外部实体与网络 resolver，仅接受五个预定义
  实体和合法 numeric character reference；namespace 作用域、重复 expanded attribute、
  closing tag、深度、事件数、属性/文本与扩张预算均在构造 IR 前校验。
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
- 网络访问默认关闭。未来 HTTP 解析器必须解析并验证每次重定向，拒绝回环、
  私有、链路本地、组播和云元数据地址，在配置时执行白名单，并限制响应字节数。
- AI 响应是不可信的结构化输入，必须验证补丁或 Schema、溯源、节点引用和资源。
- 模型通过 HTTPS 下载并固定 SHA-256，同时携带许可证元数据。
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
  平台或许可证审核时禁止安装。运行时清单必须与权威下载清单逐项一致。
- 模型写入使用同文件系统 staging、逐文件大小/哈希校验、`fsync`、版本化持久 journal
  与锁内原子切换；每次磁盘操作前恢复中断事务，损坏或歧义 journal 一律 fail closed；
  journal 先完整写入并同步根身份绑定的临时文件，再以 no-replace rename 发布；Windows
  目录 handle 持久同步尚未审计，因此 Windows 安装稳定拒绝而不伪报成功；
  校验和删除拒绝符号链接及非普通对象，随包只读模型不能删除。取消、超时、临时空间
  预算和内存预算由统一 ExecutionContext 执行。
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
