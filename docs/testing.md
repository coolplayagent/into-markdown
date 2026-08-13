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
component 的稳定退出状态。该测试同样由 Cargo 与 Bazel 执行，并以真实文件和 stdin
覆盖默认 Engine 的 TXT 输出与显式字符集。

TXT 契约覆盖 UTF-8 BOM、UTF-16LE/BE BOM、Windows-1252、GB18030、Big5、Shift_JIS、
中英文混排、combining mark、非 BMP scalar、CRLF/LF/CR、空输入、超长行、奇数 UTF-16、
截断多字节序列、严格与 replacement 模式、converter 双重输入预算及二进制伪装。
locator 与 replacement diagnostic 必须断言原始半开 byte range，不能只断言正文。
字符集边界用固定字节覆盖 GB18030 的双字节与四字节序列、Shift_JIS 和 Big5；损坏序列
后跟合法内容时必须保留合法内容，连续相邻损坏既要断言实际 U+FFFD 数量，也要断言合并
后的诊断范围。自动 probe 还要分别覆盖安全长文本、带 BOM 的奇数或截断输入、二进制
伪装，以及位于 64 KiB 解码样本之后的 DEL、UTF-8 C1 与传统字符集 C1；格式检测不得
返回 text，真实转换必须失败。

CSV/TSV 契约覆盖 CRLF/LF/CR、外围引号、doubled quote、字段内换行、尾随空字段、空记录、
UTF-8/UTF-16 BOM、显式传统字符集、表头三种策略、strict/pad 不等宽策略与 GFM pipe
转义。provenance 同时断言 quoted、多字节和补齐空单元格的原始 byte range；损坏 quote、
超宽表、超长字段及行/列/cell 预算断言 `malformed` 或 `resourceLimit`，并通过真实 CLI
覆盖文件、stdin、扩展名、MIME、显式格式与字符集。
共享解码回归还以 100 KiB ASCII 和 450 KiB 逻辑内存预算验证紧凑 identity map，以
100 KiB 自动传统字符集与 UTF-16 验证字符集选择不复制完整输入，并以极低预算的 hook
断言 64 KiB sample 在 decoder 调用前失败。16,384 个交替 UTF-16 映射 run 的比较次数
上界证明 provenance 查询为对数复杂度；Big5 `88 62` 展开的两个 scalar 分别及共同查询
都必须覆盖原始两个字节。格式检测 fixture 还包含 quoted embedded CRLF/LF/CR 及空记录
配合 pad 的自动候选路径。

真实 CLI 回归同时覆盖文件与 stdin：200 层且超过 1 MiB 的合法 JSON、具备表头/数字列
证据的三行 CSV/TSV 不得被 TXT 回退吞入；恰在 1 MiB 边界闭合但后接非空白的内容及
两行逗号散文仍须按普通文本转换。JSON scanner 单元测试覆盖 escape/Unicode、number、
literal、有效开放状态、错误尾部、括号不匹配、trailing comma 与 nesting 资源上限。
JSON string 测试还要覆盖合法 surrogate pair、多个 pair、BMP escape、lone low、
high 后接非 low、EOF high，以及不应被解释为 Unicode escape 的转义反斜杠。
真实子进程还覆盖 4096 层 JSON 在显式 4096 深度预算下成功、4097 层返回 `resourceLimit`，
确保 parser、IR emitter 和析构均不依赖用户深度的调用栈。自动检测回归固定 `true`、`123`、
`"x"` 为 JSON；500 KiB JSON string 在 128 KiB 逻辑内存预算下须在大分配前失败。XML 回归
覆盖普通 text、CDATA、attribute 与 numeric GeneralRef 中的 raw control、`&#1;`、surrogate、
U+FFFE，以及 UTF-8/UTF-16 的独立属性 QName/value 原始 byte span。
500001 行输入必须在创建 IR 节点前以 `resourceLimit` 和退出码 5 失败，不得退化为
`internal`。

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

资源规划测试还覆盖相同字节的跨 ID 去重、MIME 冲突、完整摘要、危险 URI prefix、
悬空引用、单项与总量预算，以及 Windows reserved/ADS、大小写折叠和 Unicode 路径。
输出集合以故障注入覆盖第 N 个 stage、fsync、每个持久 journal/backup/install phase、
commit/rename 竞态、取消、临时预算与 overwrite 回滚；每个 phase 模拟进程终止后由
新管理器实例恢复，并断言只出现完整旧集合或完整新集合。回滚失败测试必须把已安装
目标替换为非空目录，验证稳定 `rollbackFailed`、唯一旧备份和 journal 仍在，移除阻塞
后下一次恢复可以完成。跨文件系统、active lock、恶意 journal/nonce/root/member、
目录/FIFO/设备与 symlink swap 均需 fail closed，且不递归删除非管理器路径。跨
`a/b` 事务中断后，分别从 `a`、`b`、更深子目录与父目录发起的相交写入必须先恢复或
阻塞，绝不能写第三套值；认证完成后的 parent rename+symlink 替换也必须证明外部目录
无文件。物理父目录 lease 测试覆盖既有文件 hard-link 身份、支持时的大小写与
NFC/NFD 别名，以及 130/500 层合法目录无需祖先扫描；CLI 测试从真实崩溃残留开始，
断言单次命令完成恢复、写出并在 report 中记录 success。Windows 检查覆盖稳定
`componentUnavailable`。bundle 重复运行应逐字节一致，manifest
schema 1 读取迁移与 schema 2 aliases/path/ZIP 双向一致均为契约测试。

执行模型的契约测试还覆盖阶段顺序与单调进度、慢速或 panic 的监听器、pending
future 的取消和 deadline 唤醒、多 waiter 竞争、checked 预算累加，以及失败后临时
产物清理。dispatcher 生命周期使用 barrier 确定性覆盖 worker wait 前后关闭、回调中
释放最后一个 context、饱和 mailbox 接受 `Completed` 后 drain、线程创建失败降级和
listener 最终释放。阻塞来源还要覆盖有界工作者过载、deadline 先于系统调用返回、增长
文件只读预算加一字节、Unix symlink 拒绝、Windows 设备 namespace/保留设备拒绝、磁盘
句柄类型与权威句柄替换稳定性、source 分配前预留、scratch 退款后恰好双 payload 的 Vec
到 Arc 峰值、跨 context handoff、旧 `ResolvedInput` literal 与默认 resolver 方法兼容、
abandon 后释放，以及 worker panic 的稳定失败。测试不得依赖某个异步运行时才能触发取消
或 timeout。

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
