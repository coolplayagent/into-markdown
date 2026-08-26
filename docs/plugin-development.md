# 插件开发

Into Markdown 只接受经过签名、显式安装并在隔离边界内运行的插件。插件不能绕过统一
Document IR、资源校验、输出事务或当前调用的网络授权。面向最终用户的 OCR 与语音能力以
包含运行时、模型、许可和 SBOM 的完整 `.imp` 包交付，并以完整插件为管理单元。Office
97–2003 解析由 Core 原生提供。

## 选择协议

| 协议 | 适用场景 | 默认权限 | 结果边界 |
| --- | --- | --- | --- |
| `process-v1` | 需要原生库、受认证 helper 或平台运行时的转换器与能力 provider | 空环境、无网络、仅请求私有输入与临时目录 | 长度前缀 JSON、稳定事件、`ResultDto` 或类型化能力 DTO |
| `wasi-v1` | 可编译为 WASI Preview 2 command component 的可移植转换器 | 文件、时钟、随机和网络全部关闭，逐项授予 | 有界 JSON envelope、统一 IR 与资源清单 |

详细的线协议和沙箱约束分别见 [`process-plugins.md`](process-plugins.md) 与
[`wasi-plugins.md`](wasi-plugins.md)。能力 provider 还必须遵守
[`capability-plugins.md`](capability-plugins.md) 中的身份、readiness、DTO 和路由规则。

## 开发与验证 `process-v1`

插件入口必须实现握手、一次请求、严格递增事件和一个终态响应。不要解释 shell 命令、读取
继承环境或接受任意宿主路径。先用仓库 fixture 和真实管理器端到端门禁验证协议：

```sh
cargo test --locked -p into-markdown-process-plugin
bazel test //crates/process-plugin:process_plugin_test
bazel test //crates/plugin-manager:plugin_manager_process_e2e_test
```

能力 provider 的 `provider.json` 与入口、helper、固定模型、字典、许可和 SBOM 一起进入
`plugin.json` 的签名文件清单。返回值必须通过公共 DTO 校验；插件不能直接生成未验证的
Markdown 来代替 IR。

## 开发与验证 `wasi-v1`

component 必须实现 `wasi:cli/run@0.2.x`，并在 manifest 中固定 `wasiPreview`、Wasmtime
版本、component SHA-256 和支持的 host target。先重建 checked-in fixture，再运行真实
component：

```sh
python crates/plugin-wasi/tests/verify_fixture.py --rebuild
cargo test --locked -p into-markdown-plugin-wasi --test runtime -j1
bazel test //crates/plugin-wasi:plugin_wasi_runtime_test --jobs=1 --local_resources=memory=4096
```

只有业务确实需要时才在 manifest 中授予 preopen、clock、random 或精确 IP/端口网络访问；
私网目标还必须设置独立的 `allowPrivate`。宿主会在运行时继续执行路径、资源、IR 和
provenance 校验。

## 生成签名包

源码目录只包含运行所需的普通文件。`manifest-template.json` 声明包 ID、版本、协议、支持
target、入口和可选 runtime manifest；发布私钥不得提交到仓库。

```sh
openssl genpkey -algorithm Ed25519 -outform DER -out developer-ed25519.pk8
cargo run --locked -p into-markdown-plugin-manager --bin package_plugin -- \
  plugin-root manifest-template.json developer-ed25519.pk8 developer.example example.imp
```

打包器稳定排序文件，拒绝链接、特殊文件、不安全路径、已有输出和未声明内容，并签署完整
文件清单。传输用 `.imp` SHA-256 与签名公钥指纹是两个独立校验值；发布流程必须同时记录。
完整 schema 与签名字节见 [`plugin-management.md`](plugin-management.md)。

## 从安装到移除的验收

在隔离的用户数据目录中，以真实输入验证完整生命周期：

```sh
into-md plugins install ./example.imp --sha256 <PACKAGE_SHA256> \
  --signing-key-id developer.example --signing-key-sha256 <PUBLIC_KEY_SHA256> --scope global
into-md plugins verify <PLUGIN_ID> --scope global --json
into-md plugins enable <PLUGIN_ID> --scope global
into-md plugins run <PLUGIN_ID> sample.bin --input-format application/example --scope global
into-md plugins disable <PLUGIN_ID> --scope global
into-md plugins remove <PLUGIN_ID> --scope global
```

测试应覆盖错误签名、文件增删或变更、目标不匹配、能力缺失、取消、超时、资源上限、默认
网络拒绝和显式授权。原生插件还要在每个声明平台运行真实二进制；交叉编译不能替代运行证据。
发布包必须包含第三方许可、SBOM、来源和 runtime inventory，并通过仓库 license、插件管理器、
installed-smoke 与 archive-check 门禁。
