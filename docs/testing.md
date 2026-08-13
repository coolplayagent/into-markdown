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

ONNX 安全层测试使用 fake `SessionFactory`/`SessionAdapter`，离线覆盖 runtime 版本
authority、ABI/API mismatch 策略、IR/opset/GraphProto IO protobuf 变异、initializer/
overridable input 与重复 domain、输入输出 name/dtype/rank/shape、非法 C 字符串、
native huge count/name/rank 在分配前拒绝、并发 single-flight、失败重试、按最终 `Arc` 析构
计费的 count/bytes LRU、完整 contract cache key、取消和复制前资源预留。普通
`bazel build/test //...` 不获取 native archive；真正动态库的哈希、版本和 API
探针只由显式 manual target 执行；该 target 启动已安装 OS hard limit 的隔离 worker，
覆盖 factory 释放后的 session 重建、正常退出、取消后的 kill/wait，以及父进程不加载 ORT：

```shell
bazel test --config=macos_arm64 //crates/onnxruntime:native_runtime_validation
```

其它三个配置分别为 `linux_x86_64`、`linux_arm64` 和 `windows_x86_64`。没有 macOS
x86_64 target。fake 测试只证明加载与 session adapter 契约，不宣称完成真实模型推理；
显式 native target 会用完整的极小 Identity `ModelProto` 创建真实 ORT session 并执行一次
CPU 推理，以验证 adapter、factory 重建和退出析构顺序；另一个真实 Expand fixture 请求约
8 TiB float 输出，在 macOS ARM64 的 1 TiB `RLIMIT_AS` worker 中稳定失败为
`resourceLimit`，父进程随后仍能创建 Identity session。该 ceiling 是虚拟地址空间而非
RSS 声明，模型 session/run 和请求预算仍独立检查。fixture 不是产品 OCR 模型；
当前模型 authority 没有 ONNX runtime artifact，因而没有产品模型推理验证。
独立 `native_archive_binary_audit` 显式 target 会下载四个固定官方包，但不执行异平台
代码；它有界解析并精确核对四个平台的格式、架构、SONAME/install name、imports 与
RPATH。普通 `//...` 不包含这些 manual targets。native adapter 的输出检查在任何
`GetTensorMutableData`、slice 或 Rust 值复制前完成，超界输出直接释放 native value。

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
listener 最终释放。deadline timer 使用作用域内 spawner 注入确定性覆盖线程创建失败、
clone 一致错误、取消优先级、多 waiter 即时失败与 listener/timer 状态释放，不依赖耗尽
系统线程额度；零 deadline 与不可表示的超长 deadline 不需要 timer 线程。阻塞来源还要
覆盖有界工作者过载、deadline 先于系统调用返回、增长
文件只读预算加一字节、Unix symlink 拒绝、Windows 设备 namespace/保留设备拒绝、磁盘
句柄类型与权威句柄替换稳定性、source 分配前预留、scratch 退款后恰好双 payload 的 Vec
到 Arc 峰值、跨 context handoff、旧 `ResolvedInput` literal 与默认 resolver 方法兼容、
abandon 后释放，以及 worker panic 的稳定失败。测试不得依赖某个异步运行时才能触发取消
或 timeout。

可恢复任务回归必须跨越 store 和 engine 实例，覆盖 converted 后渲染失败、重启只
重做渲染，以及 succeeded 重启直接返回逐字段一致结果。fixture 至少包含超过历史
1.1 MiB JSON array width 边界的资源和恰好 `MAX_DOCUMENT_DEPTH` 的 Document；相邻负例
覆盖资源上限加一、非规范/损坏 base64 与公共深度加一。加载测试还必须证明
checkpoint envelope、typed wire、base64 字符串和解码资源的共存峰值受请求内存预算约束。

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

## PP-OCRv6 检测 reference golden

检测测试中的概率图由测试代码按矩形、非对称旋转四边形、带 hole 的 ring 和多个
文本岛直接生成，不包含图片、字体、模型输出或第三方数据，因此没有额外 fixture
许可义务。普通测试只调用 fake `TensorRuntime`，不安装 Python、不下载模型，也不依赖
OpenCV。

几何期望值在审查时用临时目录中的 `opencv-python-headless 4.13.0.92`、
`pyclipper 1.4.0` 和 `numpy 2.4.2` 生成，临时目录随后删除。reference 脚本按
PaddleOCR commit `2661c7c0ef5c613e8f93c6e93b2e052399f0f854` 的 DB 路径执行：
`findContours(RETR_LIST, CHAIN_APPROX_SIMPLE)`、`minAreaRect`、四点 left/right 后各自
按 y 排序、`fillPoly` polygon mean、`distance = area * 1.4 / perimeter`、
`PyclipperOffset(JT_ROUND, ET_CLOSEDPOLYGON)` 和第二次 `minAreaRect`。固定 reference 为：

- `96x64` 概率图中的 `[20,20]-[59,39]` 矩形：score
  `0.8999999761581421`，unclip box `[(11,11),(68,11),(68,48),(11,48)]`；
- `112x96` 概率图中的 `[(18,35),(34,14),(88,54),(72,75)]` 非对称旋转四边形：
  fast score `0.8998934356977029`，pyclipper 整数路径的原始 unclip box
  `[(30.7036,-5.6755),(107.0327,51.2244),(74.8757,94.3619),(-1.4535,37.4620)]`；
  映射到 source bounds 并按官方 round 后为 `[(30,0),(107,51),(75,94),(0,37)]`；
- `96x96` ring：`RETR_LIST` 返回 outer 与 inner 两个 contour，inner 经过同一 score
  流程后低于 box threshold，最终只保留 score `0.7603305583705149` 的 outer box。

Rust 的 request-accounted Suzuki–Abe scanner 一次只保留一个正在跟踪的 contour，
在线执行 `CHAIN_APPROX_SIMPLE`，并按 OpenCV 4.13 的 reverse-scan `RETR_LIST` 顺序仅保留
scanner 正向发现序列的最后 3000 个，再整体逆序；这精确对应官方
`contours[:max_candidates]`，不是对 OpenCV 返回序再取 suffix。3600 个隔离岛 reference
明确固定返回前 3 个为 `[(178,178),(175,178),(172,178)]`、第 3000 个为 `(1,31)`。
三个分离岛和带 hole ring 另固定验证顺序与 hole 语义；恶意小岛
另受 contour event 上限约束。fast score 把四点转为 int32 后复刻 `fillPoly` 的 even-odd
内部与 OpenCV 8-connected 整数边界线；倾斜 convex/concave mask reference 要求像素集合
完全相等。closed polygon offset 使用 `clipper2-rust 1.1.0` 的 i64 path，多个输出 path
与官方一样拒绝。

归一化 golden 对 BGR 三个通道分别覆盖全部 256 个 uint8 输入，把固定 NumPy f32
`pixel * scale - mean` 再除 std 的结果 bits 按大端连接并比对 SHA-256；同时逐 bit 固定
pixel 48 的 `0xbfa5e091`、`0xbf990226`、`0xbf77c490`，防止把乘 f32 scale 改写为除法。
资源回归使用多层嵌套 ring，要求累计 score pixels/work 在后续大框扫描前稳定返回
`ResourceLimit`；评分循环在实际工作开始后的 checkpoint 确定性覆盖 cancel 与 timeout。
offset 测试用调用 hook 证明 `max_offset_points=3` 和不足的逻辑内存都会在进入
`inflate_paths_64` 之前失败。

resize reference 还覆盖 3x2 到 7x5 的所有 BGR 输出样本、确定种子的随机 BGR 图、
downsample 和 tiny-image padding 边缘，锁定 OpenCV 4.13 默认
`INTER_LINEAR` 的 uint8 结果，每通道最多允许 1 LSB；不把它表述为跨 OpenCV 版本或
`INTER_LINEAR_EXACT` 等价。`99x2` 固定验证先 padding 后的两阶段 int 截断结果为
`4000x64`。score 要求 `1e-5`，整数映射坐标仅允许 minimum-rectangle 浮点 tie 导致的
1 pixel 差异。

CLI 契约测试还必须覆盖直接输入与保留命令冲突、双语帮助、stdin、目录展开、
配置合并、联网授权、稳定退出码、JSON Schema、Bundle 路径净化、原子输出、冲突
改名和批量失败汇总。尚无后端的管理操作应返回 `componentUnavailable`，不得联网或
创建虚假状态。

公共 DTO 契约测试固定精确 JSON golden，并覆盖双向转换、同版本未知字段、未知版本、
缺失必填字段、非法 base64、重复 ID、不安全 Bundle 路径、非有限数以及 JSON 深度、
条目数和解码后资源总量预算。Cargo 与 Bazel 必须执行同一组测试。

## SQLite TaskStore

TaskStore 测试使用真实 bundled SQLite 和临时私有目录，不 mock SQL，覆盖 schema 0 升级、
重复迁移、未知新版本、损坏 database/enum；非法状态/进度回退/stale CAS/terminal 二次更新；
WAL reader snapshot、双 writer 单胜者、lock deadline 与跨线程运行中取消；子进程分别在 commit
后与未提交 WAL transaction 中 `abort`，并覆盖 migration transaction 中断回滚；unknown-field
secret canary 与 db/WAL/SHM 扫描；public
root、root/db symlink、chmod、root/db identity swap 且验证 mutation 未发生；online backup
一致性、standalone、`0600`、no-replace、发布前 abort 不出现半成品且保留可审计 orphan；真实
converted/succeeded RecoveryStore checkpoint 的跨实例晋升、input/options mismatch、token
重复绑定拒绝、同状态最低进度修复、lost CAS skip、无 checkpoint interrupted，以及瞬态 recovery
error 不改 task。

四产品目标门禁执行 Rust all-targets cross-check 与 Bazel platform build。bundled SQLite 是 C
source build；没有对应目标 native runner 时，交叉编译只证明 toolchain/source/link
compatibility，不能替代 Windows reparse/DACL 或 Linux filesystem runtime 测试。

## 全格式 fixture 语料库

`fixtures/manifest.json` 是小型离线语料的机器权威。它覆盖产品格式 registry 中每个
`available` 格式的 normal、corrupt、limit 场景，并按适用范围加入 encrypted 和
malicious 场景。converter 测试直接读取同一 manifest，与 `planned_formats()` 的动态
available 集合比对，并把每个非 OCR 样本送入真实 converter 和 Markdown renderer。
成功样本核对最终 Markdown SHA-256；失败样本核对稳定错误码；limit 样本还记录精确
`ConversionOptions` 字段、相邻失败/成功值、错误 limit 名和成功输出 hash，防止仅凭文件
名称推断边界。

普通 Cargo/Bazel 图只读取 checked-in `fixtures/small/`，不联网。Noto 字体和 PP-OCRv6
recognizer 是显式 manual target；字体只用于重建 OCR PNG，模型只供后续独立质量目标，
两者均不进入普通测试或发布物。OCR golden 的 NFC、有效字符数、CER 空白/标点规则、
分组阈值、渲染参数和训练污染声明由 license audit 校验。固定 Python/Pillow/FreeType
环境下用 `fixtures/generate.py --verify` 在临时目录重建并逐字节比对；checked-in PNG
始终是权威，不宣称任意平台渲染器都能产生相同字节。详细操作见 `fixtures/README.md`。
