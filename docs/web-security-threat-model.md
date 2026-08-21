# 本地 Web 控制台威胁模型

本文是 `into-md ui` 的权威威胁模型。实现细节和 HTTP 契约见 [`ui.md`](ui.md)，通用解析、
运行时与插件安全边界见 [`security.md`](security.md)。本模型覆盖会话交接、回环 HTTP、上传、
任务与制品存储、Markdown/IR 预览、下载和浏览器运行环境；它不把“只监听本机”视为天然安全。

## 保护目标

- 本地文档、录音、转换产物、任务历史、配置和 Provider 环境变量的机密性。
- 任务、配置、插件、模型和下载制品的完整性。
- 转换服务在敌意输入、慢连接、超限输入、取消和关闭时的可用性及资源有界释放。
- 每次启动的 256-bit 会话值、一次性管理授权和 capability-bound artifact 句柄。

文档正文、文件名、MIME、归档成员、Markdown、Document IR、诊断、Provider 响应、模型、
插件包以及浏览器可访问的其他站点均为不可信输入。源码树、固定并校验的发布资产、当前进程
生成的会话值和通过 descriptor-bound 校验打开的私有存储是受信边界。

## 攻击者与边界

| 攻击者 | 能力 | 必须阻断的结果 |
|---|---|---|
| 恶意网页、广告或 iframe | 向回环端口发起导航、表单、fetch、预检和 DNS rebinding | 调用 API、读取响应、探测任务或下载制品 |
| 同机其他普通用户 | 连接回环端口、猜测端口、观察公开进程信息 | 获取会话值、读取数据目录或复用历史请求 |
| 恶意文档或归档 | 控制正文、文件名、关系、资源、压缩结构和转换输出 | 路径穿越、符号链接逃逸、脚本执行、任意资源加载或无界资源占用 |
| 获得旧会话请求的攻击者 | 重放旧 Header、URL、Cookie、查询参数或管理 grant | 在新进程或 grant 消费后重新取得权限 |
| 慢客户端 | 不完整上传、停止读取下载、在关闭时保持连接 | 无限占用临时文件、匿名 inode、配额或 worker |
| 带页面访问权限的浏览器扩展 | 读取或修改页面、发起同源请求 | 属于剩余风险；用户必须只在可信浏览器配置中使用控制台 |
| 当前用户下的恶意进程、调试器或已提权攻击者 | 读取进程内存、终端、浏览器数据或私有目录 | 超出本地 Web 协议可防御边界，不宣称隔离 |

## 请求认证、CSRF 与 DNS rebinding

服务只绑定 IPv4 `127.0.0.1`，不存在可配置的外部监听地址。所有请求先验证唯一 ASCII `Host`
恰好等于本次 `127.0.0.1:<port>`；因此 `localhost`、IPv6、尾点域名、其他端口、重复值和由
攻击域名重绑定到回环地址的请求均在路由前拒绝。

所有 `/api` 请求还必须同时满足：

1. `Origin` 恰好等于本次 origin；浏览器不发送 `Origin` 的 GET/HEAD 只接受
   `Sec-Fetch-Site: same-origin` 与 `Sec-Fetch-Mode: cors` 的精确组合；
2. `X-Into-Md-Session` 是唯一、固定 43 字符的当前进程会话值，并以固定时长比较；
3. Cookie、URL query、fragment、表单字段和请求体均不能提供认证；
4. CORS 预检和跨源 origin 不得到 `Access-Control-Allow-Origin`；鉴权早于 method/fallback；
5. 响应不包含会话值，并统一使用 `no-store`、`no-referrer`、`nosniff`、严格 CSP、
   `Cross-Origin-Opener-Policy: same-origin` 与 `Cross-Origin-Resource-Policy: same-origin`。

会话只通过启动 URL 的 fragment 交接。bootstrap 在加载应用前清除完整 fragment；当前标签页为
刷新恢复把值保存在 `sessionStorage`，不会进入 localStorage、DOM、HTTP URL、access log 或错误
文本。每次服务启动重新生成值，旧进程请求在重启后返回 `401 invalidSession`。该重放保证针对
进程会话，而非为每个普通读取/转换请求建立 nonce；危险管理操作另使用绑定完整 action 的
30 秒单次 grant，消费、失败或并发竞争后都不能重放。

## 上传、任务与内容安全

- 上传只接收流式 body；展示名称必须是有界 UTF-8 单文件名，拒绝 `/`、`\\`、`.`、`..`、
  控制字符和路径语义。名称永不用于存储路径，任务、输入和 artifact 使用服务生成的 opaque ID。
- `Content-Length`、实际输入、解压、资源、页数、内存、临时空间、worker 数、idle/total deadline
  与全局 managed-tree 配额同时约束。断连、取消、超时和关闭会 drop capability 并清理 stage。
- ZIP、Office、PDF、图像、媒体与插件入口继续执行各自的结构、预算和隔离校验；Web 授权不会
  绕过 Engine 的网络、私网、Provider 或资源策略。
- 任务根、incoming、stage、published、snapshot 与 trash 只允许私有普通单链接文件；路径、
  symlink/reparse、hardlink、额外成员、identity swap 或 manifest 摘要漂移均 fail closed。
- 上传和 API 错误只返回稳定 schema/code，不反射文件正文、自由文本错误、会话值、Provider
  secret 或本地绝对路径。

## 预览、下载与浏览器能力

Markdown 预览只创建 React text node 以及固定的标题、段落、列表、表格和代码 DOM；不使用
`innerHTML`，不创建 `a`、`img`、`script`、`iframe`、`object`、`embed` 或其他可加载资源的
元素。`javascript:`、`file:`、data URI、远程图片、原始 HTML 和事件属性只作为文本显示。
IR/诊断树限制深度、节点和单容器成员，Markdown 限制 block 数；预览仅请求 artifact 的前
256 KiB。

下载端点只接受任务 manifest 中的 opaque storage key，打开后复制、校验 byte count/SHA-256，
再通过匿名只读 snapshot 流式返回。单区间 Range、MIME 和 `Content-Disposition` 全部由服务端
规范化；不可信文件名不能进入 Header 或路径。所有 API 响应不可被跨源读取，控制台也不加载
CDN、远程字体或远程运行时代码。`Permissions-Policy` 只向同源开放会议所需 microphone 与
display-capture，显式关闭 camera、geolocation、payment 和 USB。

## 本地用户、浏览器扩展与剩余风险

Unix 数据目录逐组件 no-follow 并要求最终目录 `0700`；Windows 拒绝 reparse point 并复核
volume/file identity。会话不出现在进程参数或普通启动日志。同机其他普通用户即使找到端口，
没有当前会话、精确 Origin 和 Host 也不能调用 API；操作系统仍必须正确隔离用户进程和目录。

浏览器扩展若已获得该 origin 的页面读取或脚本注入权限，可能取得 `sessionStorage` 中的会话并
以用户身份操作。当前用户下的恶意进程、调试器、屏幕/终端监控、内核或管理员攻击者同样可能
越过本协议。控制台不尝试对这些已进入用户信任域的攻击者提供密码学隔离；应使用可信浏览器
配置、最小扩展权限和受保护的本地账户，私有启动 URL不得进入聊天、工单或共享日志。

HTTP 回环通信不是 TLS。安全性依赖操作系统回环隔离、每进程高熵会话、精确 Host/Origin 和
私有数据目录；不支持把端口转发、反向代理、容器映射或远程桌面共享后的入口视为等价部署。

## 验证门禁

以下门禁共同构成证据，任何单项都不能替代其他层：

```shell
cargo test --locked -p into-markdown-cli --bin into-md ui::tests -- --test-threads=1
cargo test --locked -p into-markdown-cli web_tasks::tests -- --test-threads=1
bazel test //apps/cli:web_security_test //web/console:web_security_test --test_output=errors
INTO_MD_CLI=target/debug/into-md pnpm --filter @into-markdown/console exec playwright test tests/web-security.e2e.spec.ts --workers=1
```

真实浏览器门禁从新进程打印的 private URL 进入，且不会把该 URL 写入测试报告；它确认 fragment
在首个应用请求前清除，随后
上传包含 raw HTML、脚本、`javascript:`、`file:`、data URI 和远程图片的 Markdown，确认预览
只显示文本、没有外部网络请求和可执行/资源型 DOM，并验证同一标签页刷新恢复。Cargo loopback
门禁负责制品下载、错误 Origin、旧会话重放以及服务关闭后连接释放。测试证据不得包含真实
会话值或用户文档。
