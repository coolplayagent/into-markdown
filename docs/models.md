# 本地模型管理

`models/manifest.json` 描述模型 bundle、上游版本、语言、四个受支持目标、字符表
来源、许可证与运行时产物；`third_party/licenses/downloads.json` 是所有可下载
文件的权威 URL、SHA-256、文件名和字节数清单。运行时会交叉校验两份随包清单，
未知 schema 字段、重复 ID、路径分隔符、清单漂移或不完整元数据都会 fail closed。

当前 PP-OCRv6 条目固定的是 Paddle source archives，不是可直接执行的 ONNX 模型。
在生成的 ONNX 文件、字符表、逐文件哈希、大小、平台和许可证审核完成前，
`models list/show` 将其显示为 `planned` / `unavailable`，`models install` 返回
`componentUnavailable`，不会下载 source archives 后伪装成已安装状态。

## 命令与状态

    into-md models --json
    into-md models show pp-ocrv6-tiny-zh-en --json
    into-md models verify pp-ocrv6-tiny-zh-en --json
    into-md models path pp-ocrv6-tiny-zh-en
    into-md models remove pp-ocrv6-tiny-zh-en
    into-md models install pp-ocrv6-tiny-zh-en

查询和校验始终离线。状态为 `unavailable`、`not-installed`、`installed` 或
`corrupt`；ownership 为 `none`、`user` 或 `bundled-read-only`。`path` 只返回
完整且重新通过逐文件大小与 SHA-256 校验的目录。随发布物提供的只读模型不能删除。

平台数据目录为：

- macOS ARM64：`~/Library/Application Support/into-markdown/models`
- Linux x86_64 / ARM64：`$XDG_DATA_HOME/into-markdown/models`，未设置时使用
  `~/.local/share/into-markdown/models`
- Windows x86_64：`%LOCALAPPDATA%\into-markdown\models`

## 安装事务与安全边界

只有 `models install` 是模型下载授权。只有清单中经审核的最终 runtime files
才能进入安装事务；传输层还必须逐跳执行 HTTPS、固定 host、重定向次数、响应大小、
DNS 和地址边界，连接固定到已验证地址并保留原始 TLS SNI/Host。当前没有完整
runtime file 清单，因此产品命令不会创建网络连接。

安装器在目标数据目录内创建唯一 staging 目录，在进程间锁内流式写入并同时检查
ExecutionContext 的取消、总超时、内存与临时空间预算。每个文件必须精确匹配清单
字节数和 SHA-256，再 `fsync` 文件、完成标记及目录。发布时先保留旧完整目录，
再以同文件系统 rename 切换；失败会恢复旧目录。错误、哈希不符、截断、预算不足、
磁盘满或取消只会留下旧完整状态或无安装状态。管理器只清理由当前操作创建且名称
精确已知的 staging/backup，不扫描或删除所有权不明的目录。

所有路径组件均来自严格 ID/文件名校验；查询、校验和清理拒绝符号链接及非普通
对象。install/remove 共用同一锁，避免并发发布与清理发生 TOCTOU。
