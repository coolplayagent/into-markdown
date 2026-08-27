# ADR 0002：显式注册与隔离插件协议

状态：已接受

内置实现和链接扩展都通过 `RegistryBuilder` 注册。项目不公开 Rust 动态库
ABI。Rust ABI 不足以支持独立升级的插件，加载原生代码也会削弱输入安全边界。

进程插件使用 `process-v1`：双方先在 stdin/stdout 上交换长度前缀 JSON 握手，再进行
一个请求、零个或多个进度/诊断事件，以及唯一终态响应。宿主只接受公共 `ResultDto`，
并在返回调用方前重新验证 Document IR、资源、诊断和溯源。WASI 是独立协议，不与
`process-v1` 隐式兼容。

进程隔离必须先于插件执行生效：Linux 使用 Landlock、seccomp、rlimit 和独立进程组；
macOS 使用 deny-default Seatbelt profile 与 rlimit；Windows 使用零 capability 的
AppContainer，并在恢复主线程前绑定只允许一个进程的 Job。协议 v1 不授予网络或共享
文件系统能力；小输入与资源通过受限 pipe 传递，大输入只能使用请求私有暂存文件。父进程环境不会继承，只有宿主策略
逐项声明的变量可见。任何平台无法安装对应原生边界时都以稳定错误拒绝启动。

隔离插件使用固定 Wasmtime 的真实 WASI Preview 2 command component 和版本 1 JSON
协议，且必须使用相同的公共 IR、错误、资源和溯源规则。能力、资源边界与五目标门禁见
[`../wasi-plugins.md`](../wasi-plugins.md)。OCR、转写和说话人分离的签名包、显式注册及
readiness 路由见 [`../capability-plugins.md`](../capability-plugins.md)。
