# WASI 插件运行时

`into-markdown-plugin-wasi` 运行实现 `wasi:cli/run@0.2.x` 的真实 WASI Preview 2
command component。宿主固定 Wasmtime 39.0.1，插件清单必须声明 `protocol =
"wasi-v1"`、`wasiPreview = "preview2"`、该精确 runtime 版本、component SHA-256
和当前四平台 target。未知字段、版本、target 或非小写 64 位摘要均 fail closed。

## 线协议

宿主向 component stdin 写入一个 version 1 JSON request：`protocolVersion`、非秘密
`sourceName` 与输入字节。component 从 stdout 返回 version 1 JSON response：
`protocolVersion`、包含统一 IR JSON 的有界 `documentJson` 字符串和可选 `resources`。
每个资源必须有 portable ASCII 相对路径（`/` 分段、每段最多 240 bytes、无 Windows 设备
名或大小写别名）、严格无参数 ASCII `type/subtype` MIME、字节和匹配的 SHA-256；资源数量、总字节、stdout 和 stderr 都分别受清单上限
约束。宿主在返回前重新验证 Document IR、资源与 provenance，插件不能绕过中央 IR 或
renderer 契约。

## 默认权限和授予

所有权限默认关闭。清单可以逐项授予：

- `preopens` 只接受绝对 host directory 和无 `.`、`..`、反斜杠或空段的绝对 guest
  path。宿主从卷根句柄逐段 `open_dir_nofollow`，将最终 pinned directory descriptor 直接
  注入 WASI preopens，绝不在 guest 启动前按 manifest path 重开；目录可只读或读写。
- `clocks` 和 `random` 为布尔授予。接口保持真实 WASI 类型兼容，但未授权调用稳定 trap
  为 `capabilityDenied`，不会返回伪造时间或随机数。
- `network` 只接受精确 literal IP + TCP port。DNS、UDP 和 listen 永远关闭；私网、
  link-local、loopback 或 unspecified 地址还必须逐项声明 `allowPrivate`。socket policy
  在 connect 时再次核对完整地址。

没有隐式 ambient 文件、环境变量或网络权限。preopen guest traversal、symlink escape、
默认 socket 拒绝和精确 loopback grant 都由真实 component 集成测试覆盖。

## 执行边界和错误

每次调用同时受确定性 fuel、epoch deadline、请求取消、线性内存、stdout/stderr、资源
数量与资源字节上限约束。输入同时受请求 `maxInputBytes` 和 128 MiB 绝对上限；component
在 hash/compile 前必须匹配声明长度并小于等于 16 MiB。epoch watcher 独立于 guest yield；
无法创建 watcher 时执行不会开始。

Wasmtime epoch 是 engine 级计数，因此一个 runtime 以 RAII 单作业 gate 串行执行 guest，
防止取消一个 store 误中断另一个 store。等待者每 5 ms 检查自己的取消/deadline；gate
poison 会恢复，success、error、timeout 和 unwind 都释放 gate。

宿主以 checked arithmetic 预留 serialized-input worst case（序列化后缩至实际值）、
stdout、stderr、guest linear memory，以及 component bytes 的 32 倍 compile allowance。
每次请求都持有该 RAII 额度并在 success/error/cancel 释放。进程级 compile cache 严格只有
一个、来源不超过 16 MiB 的条目，不同摘要会替换旧条目；32 倍是拒绝请求所用的保守
accounting allowance，不声称精确测量 Wasmtime 内部 allocator。stdout 以 move 而非 clone
交给解析器；宿主先借用 raw envelope 流式核对 resource count/bytes，再按 JSON 结构上界、
decoded buffers、portable alias keys/BTree nodes 和 typed IR 峰值追加预留，成功后才构造 owned
response。returned resources 已包含在该受限 envelope 中，不重复虚构一份额度。

稳定错误包括 `invalidManifest`、`unsupportedPlatform`、`hashMismatch`、
`protocolMismatch`、`capabilityDenied`、`invalidHostcall`、`fuelExhausted`、
`resourceLimit`、`memoryOutOfBounds`、`cancelled`、`timeout`、`guestFailure`、
`invalidOutput`、`invalidIr`、`io` 和 `runtime`。非 WASI import 不会被动态链接，归类为
`invalidHostcall`；fuel、越界、输出溢出、取消和 deadline 有独立稳定分类。

## 可复现 fixture 与四平台门禁

fixture 源码、独立 lockfile、工具链 commit、构建命令、component bytes/摘要和四个 host
target 绑定在 `crates/plugin-wasi/tests/fixtures/authority.json`。以下命令从源码重建并
逐字节比较 checked-in component：

```shell
python crates/plugin-wasi/tests/verify_fixture.py --rebuild
cargo test --locked -p into-markdown-plugin-wasi --test runtime -j1
bazel test //crates/plugin-wasi:plugin_wasi_runtime_test --jobs=1 --local_resources=memory=4096
```

上述专项命令在 Windows x86-64、Linux x86-64、Linux ARM64 和 macOS ARM64 原生环境按需
执行，分别记录重建、Cargo 与 Bazel 的实际结果。常规 PR 仅运行四个 fast job，见
[测试策略](testing.md)。Wasmtime source/tag/commit、crate checksums/features、完整 license 与四 target
authority 位于 `third_party/wasmtime/`，由 license-check mutation tests 绑定。

从协议选择、manifest、签名到安装与移除验收见[插件开发](plugin-development.md)。
