# 测试策略

## 公共契约套件

`tests/contracts` 是下游调用方视角的黑盒公共契约套件。它只通过公开 crate
访问 SPI、Engine、DTO 和安全默认值，不导入私有模块；`Cargo.toml` 与
`tests/contracts/BUILD.bazel` 直接编译同一份 `src/lib.rs` 和 fixtures。独立的
`public-api-consumer` target 固定使用 workspace 的 Rust 1.97.1、edition 2024，
验证两字段 `ResolvedInput` struct literal、`SourceResolver::resolve_accounted` 默认
适配器以及请求构造器。Cargo 测试与 Bazel 构建都会编译该 target，因此只在实现
crate 内保持源码兼容不能通过检查。

契约套件逐项覆盖八个公共 SPI：`SourceResolver`、`FormatDetector`、`Converter`、
`MarkdownRenderer`、`OcrEngine`、`Transcriber`、`TensorRuntime` 和 `AiProvider`。
每个接口必须可形成 `Send + Sync` trait object；异步返回值会被实际轮询至完成、取消
或超时，不使用无法终止的 pending future。Engine 契约覆盖重复 ID、显式 hint、
confidence/priority/稳定 ID 排序、仅 `NotApplicable` 回退、其它错误立即短路、IR
验证早于渲染，以及完成进度的单一终态。

默认安全契约不访问网络、不读取环境秘密且不下载模型。测试断言联网默认关闭、所有
AI 能力默认 `Off`、OCR 的 `Auto` 默认不指定或获取模型，以及 URI 在没有当前调用授权
时返回稳定策略错误。恶意输入 fixture 使用测试侧 `catch_unwind` 包裹，并将异步调用
轮询至 Ready，证明公开边界返回受控错误而不是 panic。DTO 的受预算编解码测试与
`core_doc_test` 的 compile-fail 示例共同保证 DTO 不能绕过受控方法直接进入 serde；
后者同时由 Cargo doctest 和 Bazel `rust_doc_test` 执行。

CLI 的错误分类表在 CLI crate 内穷举全部 `ConversionError`，另由
`apps/cli/tests/exit_contract.rs` 启动真实 `into-md` 进程，验证 usage、policy 与
component 的稳定退出状态。该测试同样由 Cargo 与 Bazel 执行。

常用定向命令如下：

```shell
cargo test -p into-markdown-contracts
cargo test -p into-markdown-cli --test exit_contract
bazel test //tests/contracts:contracts_test //crates/core:core_doc_test //apps/cli:exit_contract_test
bazel build //tests/contracts:public_api_consumer
```

仓库为对象安全 SPI、稳定错误码、确定性注册表校验、显式回退语义、默认
离线、资源预算、模型清单校验、CLI 骨架和 GFM 渲染器提供契约测试。渲染器测试
逐类覆盖全部 IR 节点，并覆盖恶意链接、HTML/Markdown 字符、动态围栏、表格换行、
交错 span、脚注标签、资源模式、空内容、最深合法嵌套、LF 和重复运行确定性。
CommonMark 解析契约还覆盖 character reference 链接绕过、空 code span、富文本边界
空白以及表格内 code/link 的 pipe 语义；CLI 测试验证资源链接与写出目标共享同一
哈希规划，并在冲突时不留下部分资源。bundle 契约使用含图片的真实转换结果验证
默认、显式资源目录和 stdout 路径都只引用实际 ZIP entry；路径 URI 测试覆盖 POSIX
绝对路径、Windows 同盘路径、UNC 同 share、合法 `..`、反斜杠以及特殊字符的
CommonMark href 与 file-URL 回读，并断言跨 root/drive/share 返回稳定错误。
Bundle 权限契约直接检查 central directory 中普通文件 `0100644` 与目录 `040755`，
并在 Unix 临时目录真实解压有资源归档，验证 `assets/` 可遍历且资源可读取。

执行模型的契约测试还覆盖阶段顺序与单调进度、慢速或 panic 的监听器、pending
future 的取消和 deadline 唤醒、多 waiter 竞争、checked 预算累加，以及失败后临时
产物清理。阻塞来源还要覆盖有界工作者过载、deadline 先于系统调用返回、增长文件只读
预算加一字节、Unix symlink 拒绝、Windows 设备 namespace/保留设备拒绝、磁盘句柄类型
与权威句柄替换稳定性、source 分配前预留、scratch 退款后恰好双 payload 的 Vec 到 Arc
峰值、跨 context handoff、旧 `ResolvedInput` literal 与默认 resolver 方法兼容、abandon
后释放，以及 worker panic 的稳定失败。测试不得依赖某个异步运行时才能触发取消或
timeout。

后续实现应增加四层测试：

1. 为每个解析器、渲染器、OCR 前后处理模块和安全边界编写小型、确定性的单元
   测试。
2. 使用许可证兼容的二进制 fixture，对 IR 和 Markdown 同时做快照测试；损坏
   和加密样本必须断言错误码。
3. 增加变异测试和模糊测试目标，证明失败过程受控，不会 panic、挂起或无限制
   分配内存。
4. 为转换完整度、OCR 字符错误率（CER）、版面与表格保真度、峰值内存和延迟
   建立独立的质量与性能语料库。这些目标可以下载模型，但必须与普通
   `bazel test //...` 隔离。

每个支持平台都运行构建、单元测试和 CLI 冒烟测试。模型与原生运行时不会进入
常规构建，因此模型推理测试通过手动触发或定时工作流运行。

CLI 契约测试还必须覆盖直接输入与保留命令冲突、双语帮助、stdin、目录展开、
配置合并、联网授权、稳定退出码、JSON Schema、Bundle 路径净化、原子输出、冲突
改名和批量失败汇总。尚无后端的管理操作应返回 `componentUnavailable`，不得联网或
创建虚假状态。

公共 DTO 契约测试固定精确 JSON golden，并覆盖双向转换、同版本未知字段、未知版本、
缺失必填字段、非法 base64、重复 ID、不安全 Bundle 路径、非有限数以及 JSON 深度、
条目数和解码后资源总量预算。Cargo 与 Bazel 必须执行同一组测试。
