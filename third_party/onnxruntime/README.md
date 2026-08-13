# ONNX Runtime

生产 OCR 运行时固定使用官方 ONNX Runtime 1.29.0 CPU 包，支持 macOS ARM64、
Linux x86_64、Linux ARM64 和 Windows x86_64。`manifest.json` 记录上游
GitHub Release 发布的产物及其 SHA-256。

相关 filegroup 标记为 `manual`：普通构建和测试既不下载也不链接运行时；显式
native 验证 target 才根据目标平台选择一个固定包。`onnxruntime_worker` 二进制本身
不携带 native archive，运行时库绝对路径只能由调用方从受信 runfiles 显式传入。

运行时与模型只在隔离 worker 中加载。Linux/macOS worker 在解析 authority、接收模型或
调用 `dlopen` 前安装并复核 `RLIMIT_AS`；Windows 进程以 suspended 状态创建，加入带
process-memory hard limit 与 kill-on-close 的 Job Object 后才恢复。清单中的
`worker_address_space_overhead_bytes` 是平台级虚拟地址空间 ceiling 的固定基线，独立于
每个模型的 session/run 预算与调用方 `max_session_bytes`。macOS ARM64 的 1 TiB 基线用于
容纳 dyld/shared-cache/allocator 的稀疏虚拟地址预留，不代表 RSS 或可用物理内存；显式
测试以约 8 TiB 的真实 Expand 输出证明 ceiling 生效并保持父进程存活。

项目有意不提供 macOS x64 仓库或目标。
