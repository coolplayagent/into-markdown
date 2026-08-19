# 本地 Web 服务

`into-md ui` 提供安全的本机 HTTP 入口和嵌入式 React + TypeScript 控制台壳。控制台
包含响应式批量转换工作台、格式/模型/Provider/插件/配置/doctor 管理页、服务状态路由、主题、简体中文/英文、错误边界与受约束 API
客户端。状态响应把 `localApi.available` 与 `documentConsole.available` 都标为 `true`。

## 命令与监听

```text
into-md ui [--port <0..65535>] [--no-open] [--data-dir <目录>]
```

- 服务无条件绑定 `127.0.0.1`。默认端口为 `0`，由操作系统原子分配；显式端口已占用
  时返回 `uiBindFailed`。没有 host、`0.0.0.0`、IPv6 或外部会话值配置。
- `--no-open` 不启动浏览器，并在当前终端提供私有启动 URL。正常启动器按平台使用
  参数数组调用 macOS `open`、Windows `rundll32 url.dll,FileProtocolHandler`，或
  Linux/BSD `xdg-open`，不经过 shell。启动器失败只诊断命令类型，服务继续运行。
- Ctrl-C 触发 Axum graceful shutdown；测试通过注入的 shutdown future 取消服务，
  不替换进程级信号处理器。

## 会话交接与请求边界

每次启动从操作系统 CSPRNG 生成 32 字节会话值，并编码为无 padding、固定 43 字符的
base64url。浏览器启动地址形如：

```text
http://127.0.0.1:<port>/#into-md-session=<private-value>
```

fragment 不会进入 HTTP request target。外部 content-hash bootstrap 脚本只接受完整且
唯一的 `#into-md-session=<值>`，值的固定长度和字符集都必须合法。脚本同步通过
`history.replaceState` 清除整个 fragment，之后才动态导入 React 应用；非法交接在不加载
应用和不发请求的情况下显示安全错误。会话值只保存在 bootstrap 闭包与 API client 内存中，
不写入 localStorage、sessionStorage、DOM、日志或错误消息。静态资产不嵌入会话值；响应设置
`Referrer-Policy: no-referrer`、`Cache-Control: no-store`、
`X-Content-Type-Options: nosniff`，CSP 仅允许同源脚本和连接并禁止 framing、base URI
及表单提交；样式也只允许同源外部 CSS，不允许 inline style 或 `eval`。

合法交接后的动态模块加载或同步启动失败使用独立的通用启动错误，不复述交接错误、异常或
会话值。React 树外的 bootstrap 负责这类失败；React `ErrorBoundary` 负责 Provider、render
与 lifecycle 的同步异常，并把焦点移到 fallback 标题。React production root 显式覆盖
`onCaughtError`、`onRecoverableError` 与 `onUncaughtError`：前两者静默丢弃可能包含不可信
会话数据的原始异常，未捕获异常只显示不含原始 message/stack 的通用启动错误。异步 API 拒绝不会被 React 边界捕获，
而是在状态页显示可重试的受控错误。三类错误路径分别测试，均不把会话值写入 DOM 或日志。

所有 API 路由统一要求：

1. `Host` 恰好为本次监听的 `127.0.0.1:<port>`；
2. `Origin` 是单一 ASCII 值，逐字节等于 `http://127.0.0.1:<port>`；
3. `X-Into-Md-Session` 是固定长度并经固定时长比较验证的会话值。

因此 `localhost`、IPv6、尾点主机、userinfo、`null` Origin、逗号拼接值、重复 Header
及其他端口都会被拒绝。查询参数、请求体和 Cookie 不能提供授权。静态入口无需会话
Header，但仍强制精确 Host；任务、制品和事件流 API 复用同一中间件。

服务不记录 access log，普通启动诊断只包含不带 fragment 的 origin。显式
`--no-open` 或浏览器启动失败时，私有 URL 只作为当前终端的人工交接输出；不得把该行
转发到日志、工单或聊天记录。

## 本地数据目录

`--data-dir` 只指定本地状态根，不改变监听或网络授权。Unix 实现从根目录描述符开始，
逐组件使用 `openat` 的 directory/no-follow 约束，并以 `0700` 创建缺失目录；最终目录
若允许 group/other 访问则 fail closed。操作都依赖已验证目录句柄，创建后重新打开并
同步父目录。Windows 实现逐组件拒绝 reparse point 和非目录，持有目录句柄并在创建后
重新打开、核对 volume 与 file identity；目录访问控制沿用 Windows 创建目录时继承的
ACL。任何不安全路径返回 `unsafeDataDirectory`，服务不会在降级检查下继续。

## HTTP DTO

`POST /api/status` 使用空请求体且不接受 `Content-Type`，浏览器会为该非安全方法自然
携带 `Origin`；页面脚本不能也不尝试自行设置该浏览器受控 Header。响应 JSON envelope
的顶层 `schemaVersion` 固定为 `1`。API 在分派 method 前先鉴权：缺少授权的 GET 等方法
返回鉴权错误，授权正确但方法不符时返回 schema 1 的 `405 methodNotAllowed`。未知路由、
鉴权拒绝和其他错误也返回带 `schemaVersion: 1` 的稳定 envelope；它们不复用内部 Rust
布局。外层响应中间件为所有 method、fallback、2xx 和 4xx 统一添加安全 Header 与
`no-store`，不依赖具体 handler 正常返回。

`GET /api/admin` 返回 schema 1 的有界管理快照。格式来自 core catalog；模型状态来自
与 CLI 相同的 `ModelManager`；Provider 只返回脱敏 URL、环境变量名及“是否存在”，永不读取
或返回环境变量值；配置使用 `LoadedConfig::display_value` 的脱敏结果。插件按 global/project
精确作用域列出，并通过与 `plugins verify` 相同的 PluginManager authority 复核安装树、发布者
签名、协议/target 和配置中的 package hash/signing-key pin；doctor 展示同一验证结果，快照不会
联网。`POST /api/admin` 接受最多 16 KiB、拒绝未知字段的 action DTO。插件 verify、enable、
disable 和 remove 直接复用 CLI 的 scope identity、崩溃恢复、CAS 与 store/config 联合事务，
不会走单独的 Web 配置写入路径。模型/Provider 联网操作必须在该请求携带
`authorizeNetwork`；删除等危险操作必须携带 `authorizeDangerous`。页面先向
`POST /api/admin/grant` 申请 30 秒有效的服务端 grant；grant 绑定当前 session、action、scope、
target、source、SHA-256、signer 和全部网络/危险标志，并在 action 校验时原子单次消费。成功、
后端失败或客户端中止都不会恢复 grant；重放和参数替换均拒绝。单作业 administration gate
在并发 snapshot/grant/action 时返回稳定的 `adminBusy`，busy 请求不会消费 grant。页面也在发起
请求前清零复选框并用同步 in-flight guard 防双击，但客户端状态不构成安全边界。安全配置编辑器只公开非秘密键，服务端也拒绝 secret、token、password 或
明文 API key 形态的写入；API key 只能由命名环境变量提供。

默认自动配置上下文列出 global/project 原始 override 和一条完整 effective 记录。原始记录仅能
修改/删除自身作用域；effective Provider 仅能测试，effective 插件仅能按物理配置文件 identity
选出的 package scope 做合并 pin 验证。目录名中的 `#`、路径别名和 Windows 大小写不会被字符串
切分误判。若 UI 由 `--config`、`--profile`、`INTO_MD_PROFILE`、`--no-config` 或其组合启动，
服务只返回 `scope=effective` 的只读视图，不恢复或打开自动 global/project 插件 store，也不下发
action/package scope；Provider/插件测试、验证和全部配置 mutation 返回
`adminConfigContextReadOnly`。格式检测和模型只读/安装动作仍复用该次启动的 timeout/resource
上下文。

管理能力与 CLI 的映射如下；`plugins run` 是文档执行能力，不属于管理面：

| Web action | CLI authority | 一次性授权 |
|---|---|---|
| `format.detect`（format/extension/MIME/charset/host/private） | `formats detect` | 网络；private 另需 dangerous |
| `model.show/path/verify/install/remove` | `models` 同名子命令 | install 网络；private/insecure 与 remove 需 dangerous |
| `provider.add/test/set-default/remove` | `providers` 同名子命令 | add、set-default、remove 需 dangerous；test 需网络，private 另需 dangerous |
| `plugin.install/verify/enable/disable/remove` | #44 manager + `plugins` 同名子命令 | local install/enable/remove 需 dangerous；HTTPS install 另需网络 |
| `config.paths/show/init/validate/get/set/unset` | `config` 同名子命令；show 明确选择 merged 或 resolved 且始终脱敏 | init force、set、unset 需 dangerous；managed namespaces 只能走专用 action |
| `profile.show/create/remove` | `profile` 同名子命令 | create/replace 与 remove 需 dangerous；show 始终递归脱敏 |
| `doctor.run` | 共享 `collect_doctor_checks` 服务 | 网络探测需网络，private 另需 dangerous |

所有 snapshot 与成功 operation result 在服务端序列化后限制为 1 MiB；客户端使用相同限制并对
DTO 枚举、长度、集合数量、hash、scope 与只读交叉字段 fail-closed。Profile 和插件 URL 会移除
userinfo、query 与 fragment；Provider 只显示环境变量名和存在状态。

## 控制台与静态资产

`/workbench`（以及 `/`）提供多文件拖放、文件与目录选择、批量选项、队列进度、取消、
失败重试和产物下载；`/status` 显示本地 API 状态，其他页面显示 404。每批最多 100 个文件、
总计 1 GiB，单文件上限由批次 Engine 选项控制且不得超过 512 MiB。刷新后通过
`/admin/formats`、`/admin/models`、`/admin/providers`、`/admin/plugins`、
`/admin/configuration` 与 `/admin/doctor` 覆盖本地管理能力。`GET /api/tasks` 恢复最近 100 个 durable 任务；原文件不写入浏览器存储，因此刷新后的失败
重试要求重新选择。客户端路由 fallback 仅处理带 `Accept: text/html` 的 GET/HEAD，并明确
排除 `/api` 与 `/assets`。HTML 使用 `no-store`；文件名带内容 SHA-256 前缀的 JavaScript
与 CSS 使用一年 `immutable` 缓存并带完整 SHA-256 ETag。所有响应都有精确 MIME、
`nosniff` 与 CSP，不使用 CDN、远程字体或运行时网络。

布局提供 skip link、语义 header/nav/main、可见的 `:focus-visible`、路由后主内容焦点、
不小于 44px 的主要按钮、44rem 窄屏重排和 reduced-motion。浅色、深色与系统主题均使用
经自动计算达到 WCAG AA 普通文本阈值的前景/表面 token。主题与语言只保存在当前页面
内存中，不以 Web Storage 持久化。语言切换同步更新根元素的 `lang` 与 `dir`，不会被路由
焦点逻辑抢走当前控件焦点。可访问性测试运行于真实挂载的 App 树；DOM 环境无法完成几何
计算时不会把 axe 的 incomplete 当作通过，颜色对比度由独立数值测试覆盖。

## 可复现构建

Bazel 是前端生产构建权威。Node 24.13.0、pnpm 11.19.0、rules_js、rules_ts、TypeScript
与 esbuild-wasm 均固定精确版本；`pnpm-lock.yaml` 为每个包固定 registry integrity，Bazel
禁用所有 npm lifecycle hook。构建 action 不执行 `npm install`/`pnpm install` 且不联网。
首次仓库解析可由 Bazel downloader 按锁获取工具链与包；缓存预热后可配合
`--repository_disable_download` 完全离线重建。

`//web/console:generated_assets` 在临时输出中生成确定性资产，
`//web/console:assets` 再逐文件名、逐字节比较仓库内 `web/console/dist`。更新 checked-in
发布输入必须显式运行 `bazel run //web/console:update_assets` 并审查 manifest；该目标只复制
Bazel sandbox 中 `generated_assets` 的权威字节，不在工作区重新解析或构建 npm 模块。更新器对
工作区路径组件、现有 `dist`、临时目录与备份逐项执行 no-follow 类型/identity 检查，拒绝
符号链接。真正的文件系统更新由受 Bazel 管理的 Rust helper 完成：macOS/Linux 全程持有可信
父目录 `dirfd`，所有 chmod、复制、清理和发布分别使用句柄或 `*at` 相对操作；已有目标通过
`RENAME_EXCHANGE` 原子切换，空目标与备份使用 no-replace rename。验证后的 temp、父路径、
目标或备份被并发替换都会 fail closed，测试以确定性 barrier 证明仓库外只读文件的内容、inode
和权限不变。Windows 等未实现等价句柄相对语义的平台稳定返回 `assetUpdateUnavailable`，不使用
路径降级实现。Rust 不使用 build.rs，也不在运行时
读取源树；四个支持平台的 CLI 都通过 `include_bytes!` 嵌入同一组 checked-in bytes。

同一中间件保护任务后端：`POST /api/tasks` 流式接收单文件（UTF-8 display name 以
base64url 放入 `X-Into-Md-Filename-B64`；旧 ASCII Header 仍兼容），`GET /api/tasks/{taskId}`
读取 durable 状态，`DELETE` 请求取消，
`GET /api/tasks/{taskId}/artifacts/{artifactId}` 仅以 opaque ID 流式下载已验证产物。
`GET /api/tasks/{taskId}/events` 返回 `text/event-stream`。每个 `snapshot` 或 `progress`
事件使用 schemaVersion 1 DTO，包含 task ID、进程代际内单调 sequence、durable status、
millionths progress、terminal 标记及可选 Engine progress。SSE `id` 为不可猜测的进程代际与
sequence 组合；同一进程内的 `Last-Event-ID` 从每任务 64 项有界窗口回放。客户端落后于窗口或
服务重启时，服务发送当前 durable snapshot，保证最终状态不会因断线丢失。心跳 comment 每
15 秒发送一次。事件广播只使用非阻塞有界队列；慢客户端收到 lag 后以 snapshot 收敛，不占用
转换线程。关闭浏览器连接只移除观察者，不取消任务；取消必须显式 `DELETE`，它幂等触发同一
Engine `CancellationToken`。

上传可在 `X-Into-Md-Request` 携带 base64url 编码、最大 16 KiB 的 schemaVersion 1 JSON。
其中 `format` 和 `options` 直接反序列化为 Engine 的 `InputFormat` 与 `ConversionOptions`，
让 Web 和 CLI 共享 DTO 与服务端校验。联网、私网和 AI/provider 能力必须在本次上传的
`authorization` 中分别确认；授权位在建任务前消费且不写入 durable request。网络默认关闭，
host allowlist、输入/内存/临时空间、页数与资源上限都在上传前 fail closed。未携带 Header
时使用安全默认配置，以兼容已有本地 API 客户端。

## 预览、资源与下载

成功任务公开 Markdown、Document IR、诊断、bundle 和已提取资源的 opaque artifact 引用。
工作台的 Markdown 预览不使用 `innerHTML`，也不生成链接、图片、iframe、object 或 embed；
标题、列表、代码块和普通文本只由 React text node 呈现。因此原始 HTML、`javascript:`、
`file:`、data URI 和远程图片语法都只能显示为不可执行文本，默认不会读取本地或外部资源。
IR（包括 provenance）与诊断使用限深 12、限节点 1000、单容器最多 200 项的折叠树。
Markdown 同时最多构造 2000 个展示 block。
客户端只请求 artifact 的前 256 KiB；大文件明确显示截断提示，不能通过预览路径形成无界 DOM
或内存增长。二进制资源仅显示可信 manifest 中的文件名、media type 和大小，不在页面中解码。

artifact 下载端点保持 capability-bound snapshot 流式读取，每块最多 64 KiB，并支持一个
RFC 7233 `bytes` 区间（显式 `206`、`Content-Range`、`Accept-Ranges`）；非法、多区间或越界
请求稳定返回 `416 invalidRange` 和 `bytes */<length>`。响应按 artifact 类型设置
`text/markdown`、`application/json`、`application/zip` 或已验证资源 media type。
`Content-Disposition` 同时提供无控制字符的 ASCII fallback 和 RFC 5987 UTF-8 文件名；文件名
永不作为路径使用。所有下载仍要求精确 Host、Origin 和 session Header，设置 `no-store`、
`nosniff`、严格 CSP 与 `no-referrer`。bundle 内部路径继续由发布阶段的固定 manifest 控制。

## 任务历史与保留

`GET /api/tasks` 提供按 `(updatedAtMs, id)` 的稳定 newest-first cursor 分页（每页 1–100），
并可按状态和固定标记筛选。历史卡片可查看详情、重新执行仍保存在本机的离线输入、固定/取消固定，
或在明确的“不可恢复”确认后永久删除。需要联网、私网或 Provider 的旧任务不会复用一次性授权，
必须重新上传并再次授权。

默认保留策略为 30 天和 10 GiB：服务启动、接受新上传前和用户确认“立即清理”时，按首次进入
terminal 状态的持久化时间从最旧的、未固定任务开始删除，直到年龄和容量条件同时满足；恰好
30 天或总量恰好超过容量边界的任务参与清理，恰好等于容量时不清理。单个未固定任务即可因
超额被删除；固定项不计为候选，即使因此仍高于目标容量也不会删除；pending/running/converted
任务永不参与。10 GiB retained-history 目标不超过 14,352 MiB data ceiling；另有四笔单任务
1,028 MiB 保守 reservation 与 4 MiB SQLite headroom，使并发转换不会突破 14,356 MiB managed
ceiling。

删除先验证 checkpoint 和 capability-bound 任务树，再把目录以 `taskId.recoveryToken` 原子移动到
私有 `trash`，事务删除 SQLite 主记录及子记录，最后清除 checkpoint 和隔离目录。SQLite commit
前失败会原子恢复任务目录；commit 后 `trash` 名称是 durable deletion intent。若中途崩溃，
下次启动会依据数据库是否仍有该任务来恢复目录或完成清理；任何非私有目录、链接、硬链接、
异常深度、token 所有权冲突或非 canonical 名称都 fail closed，不会跟随用户路径。
