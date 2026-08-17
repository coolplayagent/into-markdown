# 本地模型管理

`models/manifest.json` 描述模型 bundle、上游版本、语言、四个受支持目标、字符表
来源、许可证与运行时产物；`third_party/licenses/downloads.json` 是所有可下载
文件的权威 URL、SHA-256、文件名和字节数清单。运行时会交叉校验两份随包清单，
未知 schema 字段、重复 ID、路径分隔符、清单漂移或不完整元数据都会 fail closed。

清单 schema 2 区分完整 pipeline 与可独立安装的组件。`pp-ocrv6-tiny-zh-en`
仍是 source-only 的 `ocr-pipeline`，显示为 `planned` / `unavailable`；
`pp-ocrv6-tiny-recognizer-onnx` 是 `available` 的 `recognizer-component`，绑定 PaddleOCR
commit `2661c7c0ef5c613e8f93c6e93b2e052399f0f854`、官方 ONNX TAR、最终 ONNX、配置文件和
字符表的精确大小与 SHA-256。旧 schema 1 只兼容 planned source-only 清单；任何 runtime
或 available 语义都会由运行时与 release audit 同样拒绝，不能暗中改变旧清单含义。

## 命令与状态

    into-md models --json
    into-md models show pp-ocrv6-tiny-zh-en --json
    into-md models verify pp-ocrv6-tiny-zh-en --json
    into-md models path pp-ocrv6-tiny-zh-en
    into-md models remove pp-ocrv6-tiny-zh-en
    into-md models show pp-ocrv6-tiny-recognizer-onnx --json

查询和校验始终离线。状态为 `unavailable`、`not-installed`、`installed` 或
`corrupt`；ownership 为 `none`、`user` 或 `bundled-read-only`。`path` 只返回
完整且重新通过逐文件大小与 SHA-256 校验的目录。随发布物提供的只读模型不能删除。

平台数据目录为：

- macOS ARM64：`~/Library/Application Support/into-markdown/models`
- Linux x86_64 / ARM64：`$XDG_DATA_HOME/into-markdown/models`，未设置时使用
  `~/.local/share/into-markdown/models`
- Windows x86_64：`%LOCALAPPDATA%\into-markdown\models`

程序化调用方必须把 `ModelManager` 指向专用的 `models` 叶目录，而不是通用临时目录或其父目录。该叶目录首次使用时可以尚不存在，由管理器创建；如果已经存在，它必须已经具有管理器要求的受保护精确 ACL。继承普通 ACL 的 `tempdir` 或共享目录会 fail closed。测试代码也应传入例如 `tempdir/models` 这样的未创建子目录。

Windows x86_64 在固定本地 NTFS/ReFS 上支持安装、删除和崩溃恢复。事务持有经 ACL、
reparse point 与物理 FileId 校验的根目录 handle；私有文件拒绝 hardlink 与 ADS，发布使用
同卷 `MoveFileExW(MOVEFILE_WRITE_THROUGH)` no-replace rename。目录 handle 可用时额外执行
`FlushFileBuffers`；Windows 返回 `ERROR_INVALID_HANDLE` 的文件系统由 write-through rename
提供 namespace durability。网络盘、可移动盘及其他文件系统稳定返回
`componentUnavailable`。

## 安装事务与安全边界

只有显式安装调用才是模型下载授权。只有清单中经审核的最终 runtime files
才能进入安装事务；传输层还必须逐跳执行 HTTPS、固定 host、重定向次数、响应大小、
DNS 和地址边界，连接固定到已验证地址并保留原始 TLS SNI/Host。library manager 接受
显式 `ModelFetcher`；CLI 当前没有 transport，因此 `models install` 不创建连接并返回
`componentUnavailable`。

归档型 artifact 的 fetch stream 必须是原始官方 TAR，不能伪装成已解压文件。管理器先按
归档 authority 检查获取类型，再在同一受预算边界内验证整包大小与 SHA-256、TAR header
checksum、两个结束块和全零 trailer；条目必须与清单顺序和数量精确一致。绝对路径、`..`、
反斜杠、重复/未知条目、symlink、hardlink、非普通类型、非零 padding、截断和 size bomb
全部在发布前拒绝。`inference.onnx` 与 `inference.yml` 各自再次核验成员大小和 SHA-256；
仅最终 ONNX 写入 staging，source TAR 不会成为 runtime file。

安装器在目标数据目录内创建唯一 staging 目录，在进程间锁内流式写入并同时检查
ExecutionContext 的取消、总超时、内存与临时空间预算。每个文件必须精确匹配清单
字节数和 SHA-256，再 `fsync` 文件、完成标记及目录。发布时先保留旧完整目录，
再以同文件系统 no-replace rename 切换。发布前先将完整 journal 序列化到根身份绑定的
随机临时文件，`write_all` 并 `fsync` 后才以 no-replace rename 发布最终 journal，随后
`fsync` 父目录。最终 journal 携带内容校验和，并绑定 canonical 根路径、Unix dev/inode、
权限受限的持久根 token、bundle ID、nonce 和精确目录名；跨数据根复制会被拒绝。
进程重启后，显式 `recover_with_context` 以及下一次 install/remove 会在持有事务锁时，
根据 journal 与目录拓扑完成发布或恢复旧目录；list/show/status/verify/path 始终只读，
发现未完成事务时稳定要求显式恢复，不会为查询创建根身份、锁文件或修改目录。journal 损坏、路径不匹配或存在
多个残留时 fail closed，不触碰任何候选目录。错误、哈希不符、截断、预算不足、磁盘
满或取消只会留下可由 journal 恢复的旧完整状态、新完整状态或无安装状态。管理器只
清理由有效 journal 精确证明所有权的 staging/backup，不扫描或删除所有权不明的目录。

所有路径组件均来自严格 ID/文件名校验；查询、校验和清理拒绝符号链接及非普通
对象。install/remove 共用同一锁，避免并发发布与清理发生 TOCTOU。

`third_party/onnxruntime/manifest.json` 同样被运行时嵌入；其版本、C API level、四个
target、asset、压缩包 SHA-256、解包后动态库路径与动态库 SHA-256 是运行时权威，
`load_identity` 与固定二进制的系统依赖闭包也属于同一权威，
并与 `downloads.json` 的 URL、strip prefix 和固定 repository 名双向精确一致。
模型运行时文件进入清单时还必须绑定模型哈希、IR version、完整 opset imports、每个
输入输出的精确 name/dtype/rank/固定或有界动态 shape，以及 session/run 保守内存上界。
安全层从 hash-verified `ModelProto` 字节用有界 protobuf parser 独立读取 field 1 和
field 8，规范化默认 domain 并拒绝重复 domain，再与 authority 精确比较；ORT 创建
session 后还会核对图的输入输出元数据。产品 `ManifestModelResolver` 只接受
ModelManager 从同一完整 install-state 重新打开、no-follow 并复核哈希的 recognizer 文件；
字典也从同一组件目录取得。未安装返回 `ModelUnavailable`，损坏状态不会降级为未安装。
pipeline 的 source 角色必须包含 `detector` 与 `recognizer-and-dictionary`；recognizer
组件的 source 角色为 `recognizer-and-dictionary`，runtime 角色必须精确包含 `recognizer`
和 `character-table`。角色不能重复；空 runtime 列表只允许 planned 状态，组件安装不能把
完整 pipeline 误报为 complete。

识别组件的模型权威还绑定 `fixtures/manifest.json#ocr_quality` 的受控结果：简体
0/65、繁体 6/65、英文 1/185、混排 1/116，对应 CER 上限分别为 5%、10%、5%、8%。
显式 native quality target 必须同时复核字符数、错误数和阈值；修改 golden、归一化规则、
隐式模型 fallback 或降低阈值都不能把模型漂移伪装成通过。
