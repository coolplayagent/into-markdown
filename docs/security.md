# 安全模型

文档、归档文件、标记语言、媒体、模型文件、提供者响应和 URL 均为不可信输入。

- `ResourceLimits` 限制输入大小、解压后字节数、归档条目数、嵌套深度、页数和
  保留资源数，并限制实现显式计费的内存与请求临时文件字节数。所有累加使用 checked
  arithmetic，临时文件由执行上下文负责 RAII 清理。
- 归档路径必须规范化，拒绝路径穿越和绝对路径，解压时不得跟随符号链接。
- 本地输入打开在 handle 层拒绝 Unix symlink 或 Windows reparse point，并只接受 regular
  file，不能把 CLI 规划时的路径检查当作最终安全边界。
- XML 解析必须禁用外部实体以及 DTD/网络解析。
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
取消与总 timeout 同样贯穿每个 SPI；长循环和外部服务等待必须设置协作检查点，不能
依赖引擎外层检查来中断内部工作。
