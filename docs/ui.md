# 本地 Web 服务

`into-md ui` 提供安全的本机 HTTP 入口和嵌入式静态页面。它只建立入口与安全壳；
文档数据库、任务队列、完整控制台及其业务 API 不属于此命令。状态响应因此把
`localApi.available` 标为 `true`，并把 `documentConsole.available` 标为 `false`、
错误码标为 `componentUnavailable`，不会用占位页面伪装完整产品能力。

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

fragment 不会进入 HTTP request target。嵌入脚本只接受恰好一个
`into-md-session`、固定长度和字符集均合法的值，并在任何 `fetch` 前通过
`history.replaceState` 清除 fragment。静态 HTML 和 JavaScript 不嵌入会话值；响应设置
`Referrer-Policy: no-referrer`、`Cache-Control: no-store`、
`X-Content-Type-Options: nosniff`，CSP 仅允许同源脚本和连接并禁止 framing、base URI
及表单提交。

所有 API 路由统一要求：

1. `Host` 恰好为本次监听的 `127.0.0.1:<port>`；
2. `Origin` 是单一 ASCII 值，逐字节等于 `http://127.0.0.1:<port>`；
3. `X-Into-Md-Session` 是固定长度并经固定时长比较验证的会话值。

因此 `localhost`、IPv6、尾点主机、userinfo、`null` Origin、逗号拼接值、重复 Header
及其他端口都会被拒绝。查询参数、请求体和 Cookie 不能提供授权。静态入口无需会话
Header，但仍强制精确 Host；未来 API 或事件流必须复用同一中间件。

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
