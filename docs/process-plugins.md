# `process-v1` 隔离插件

`into-markdown-process-plugin` 用于执行已经由安装层认证的第三方转换器和 OCR/音频
capability provider。运行时不搜索
`PATH`、不解释 shell 命令，也不加载进程内动态 ABI。manifest 必须给出规范绝对路径、
包含该文件的 runtime root、精确小写 SHA-256、插件 ID 和显式协议版本；每次启动前都会
重新校验文件类型、路径范围和摘要。宿主以固定大小缓冲区把 runtime tree 复制到每请求
私有目录，对实际复制字节使用 `ExecutionContext` 临时空间账本，并在每个块检查取消和
期限；最终启动文件在私有目录中再次校验入口摘要，目录及账本在所有退出路径由 RAII 回收。
`process-v1` 只认证 manifest 声明的入口文件；整个安装包及其库内容的 authority 由 #44
安装/注册层提供，调用方不得把入口摘要误当成整包摘要。

## 协议

stdin/stdout 仅承载 frame：4 字节 little-endian `u32` JSON 长度，随后为精确数量的
UTF-8 JSON 字节。绝对上限为 64 MiB，部署策略可以进一步收紧；长度在分配前检查，宿主
同时向 `ExecutionContext` 预占双向 frame 内存。JSON 后的额外值、未知字段、零长、截断
和超长 frame 均失败关闭。stderr 与协议分离、持续排空且不进入外部错误 envelope。

状态机固定为：

1. 宿主发送支持版本、预期插件 ID 和请求 nonce；插件只能回一个匹配的握手。
2. 宿主发送格式、请求 ID、输出上限，以及内联 base64 或请求私有 `source.bin` 二选一的输入。
3. 插件可以发送严格递增 sequence 的进度或单条已验证诊断。
4. 插件发送且只能发送一个 `ResultDto` 响应或稳定错误，然后以成功状态退出。

重复握手、错 request ID/version、倒序事件、响应后的 frame、提前 EOF/崩溃都属于协议
错误。取消和超时会先发送 `cancel`；短暂协作期后宿主终止整个 Unix 进程组或 Windows
Job，并等待回收。插件成功响应仍会经过 `ResultDto` 解码及 Document、资源、诊断、
provenance 的公共验证。

## 能力与平台边界

`process-v1` 默认且强制离线。小输入和返回资源经 pipe 传输；大输入只暴露请求私有、
只读的 `source.bin`，不会暴露任意宿主路径。子进程环境从空集合构造，仅加入
`INTO_MARKDOWN_PLUGIN_PROTOCOL=process-v1` 和调用方明确声明的有限变量；代理、凭据、
HOME 和 loader 变量不会继承。原生 provider 也必须把该协议变量视为受限进程标记，
在进入不能从沙箱错误中恢复的 GPU/runtime 路径前选择可恢复的后端。工作目录为每请求
私有临时目录。

- Linux：`RLIMIT_AS/FSIZE/NOFILE/CORE`，Landlock 只读 runtime/动态加载器目录且只允许
  私有临时目录写入，seccomp 拒绝 socket、跨进程信号、fork、namespace、mount、ptrace
  等调用；只允许同进程线程 clone。
- macOS：固定虚拟地址上限、父进程物理内存监控和 deny-default Seatbelt；拒绝网络，
  只读 runtime/模型/系统 dyld 支持目录，只写私有临时目录。只有签名 manifest 显式声明
  时才允许从已认证 runtime 启动 helper。
- Windows：安装层预创建并 ACL 授权的 AppContainer SID；启动时 capability 数量为零，
  仅继承三根协议 handle。主线程在 Job 的单进程、内存和 close-kill 限制安装并复核 token
  SID 后才恢复。cwd 必须与该 SID 的 storage identity 精确一致。

某个平台不能建立这些边界时，运行时返回 `pluginSandboxUnavailable`，不会降级为普通
子进程。文件大小、句柄、内存、握手时间、不可关闭的硬请求期限和 frame/output 上限均由宿主策略限制。

## 构建与审计

Cargo 目标为 `into-markdown-process-plugin`，Bazel 目标为
`//crates/process-plugin:process_plugin`。跨平台 fixture 同时构建为
`process-plugin-fixture`，真实启动门禁覆盖错误 frame、超大 frame、事件乱序、崩溃、
超时/取消、父环境秘密、外部文件和 loopback 网络。该 crate 仅使用 Cargo.lock 中已有的
依赖；workspace manifest 与 lock 摘要由
`third_party/licenses/cargo-normal-runtime.json` 绑定，发布 license/SBOM 投影继续由
`license-check` 从同一 normal-runtime authority 推导。

签名 `.imp` 安装、类型化 OCR/转写/说话人分离 DTO、readiness 路由和官方包见
[`capability-plugins.md`](capability-plugins.md)。
从协议选择、实现、签名到真实生命周期验收见[插件开发](plugin-development.md)。
