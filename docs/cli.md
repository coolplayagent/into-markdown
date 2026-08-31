# 命令行设计

`into-md` 同时面向交互式终端、Shell 管道、批处理任务和自动化系统。主产物始终
写入 stdout 或显式输出路径；日志、进度、警告和诊断写入 stderr。命令不得交互式
询问密码或联网授权。

## 命令结构

转换命令语法为：

```text
into-md [选项] <输入...>
```

管理命令为：

```text
into-md ui
into-md formats
into-md capabilities
into-md setup
into-md providers
into-md plugins
into-md transcript
into-md config
into-md doctor
into-md completions
into-md version
```

若文件名与管理命令同名，应在输入前使用 `--`；例如名为 `ui` 的输入仍可转换：

```shell
into-md -- formats
into-md -- ui
```

无参数且 stdin 连接管道时自动读取 stdin；无参数且 stdin 为终端时显示帮助。
`into-md -` 始终显式表示 stdin。stdin 不能与其他输入组合。

全部公共命令和当前可用格式的可执行示例见[命令与格式示例](cli-examples.md)。该文档由
CI 对真实 CLI 发现结果、语法、格式 catalog 和基础转换逐项校验。

### 本地 Web 服务

```text
into-md ui [--port <0..65535>] [--no-open] [--data-dir <目录>]
```

`ui` 固定绑定 `127.0.0.1`；默认 `--port 0` 由操作系统分配空闲端口。公开选项只用于
端口选择、浏览器启动和本地数据目录；`--no-open` 禁止启动浏览器，`--data-dir` 指定私有
本地状态根；Unix 上逐级通过不跟随符号链接的目录句柄创建或打开，并要求最终目录
仅当前用户可访问；Windows 上拒绝 reparse point。完整安全与生命周期契约见
[本地 Web 服务](ui.md)。

帮助和人工诊断默认使用英文；`--language zh-CN` 切换中文。JSON 字段、状态和错误码
始终使用稳定英文标识，不随界面语言变化。

## 转换

### 输入与批量

```text
into-md <INPUT...>
  -r, --recursive
      --include <GLOB>
      --exclude <GLOB>
      --hidden
      --jobs <N>
```

- 输入可以是本地文件、目录、`-` 或 HTTP(S) URI。
- 目录必须配合 `--recursive`；遍历顺序稳定，不跟随符号链接。
- `--include` 和 `--exclude` 相对于每个目录输入根匹配，可重复指定；默认跳过隐藏
  文件和隐藏目录。
- 多输入和目录输入必须指定 `--output-dir`。
- 批量任务按 `--jobs` 并发执行，默认使用系统可用并行度；单项失败不阻止其他项，
  最终以批量部分失败退出码结束。

### 格式提示

```text
-f, --format <FORMAT>
    --extension <EXT>
    --mime-type <TYPE>
    --charset <CHARSET>
    --encoding-errors <strict|replace>
    --table-header <auto|always|never>
    --ragged-rows <strict|pad>
```

显式格式优先于检测结果。扩展名可以带或不带前导点，Office 变体扩展名会映射到
对应格式族。TXT 的字符集 allowlist 与别名见[格式矩阵](formats.md)。字符集提示权威；
无效序列默认 `strict` 失败，`replace` 会为每个替换片段输出原始 byte range 诊断。
字符集同样适用于 CSV/TSV。表头默认保守自动识别；不等宽记录默认严格失败，`pad`
只补齐比首条记录短的行并输出诊断。

### 输出

```text
-o, --output <PATH>
    --output-dir <DIR>
    --emit <markdown|ir-json|result-json|bundle>
    --assets-dir <DIR>
    --asset-mode <extract|embed|omit>
    --conflict <rename|error|overwrite>
    --report <REPORT.json>
    --dry-run
```

- `--emit` 默认 `markdown`。
- 单输入未指定输出路径时，主产物写入 stdout。`-o` 仅用于单输入，
  `--output-dir` 用于批量，也可用于单输入。
- `ir-json` 输出带 `schemaVersion` 的统一 IR；`result-json` 额外包含 Markdown、
  诊断、溯源和 base64 资源。
- `bundle` 生成 `.mdpkg.zip`，固定包含 `manifest.json`、`document.md`、
  `document.ir.json`、`diagnostics.json`、`provenance.json` 和 `assets/`。归档路径
  会被净化并稳定排序。bundle 内的 Markdown 固定引用 `assets/` 下的条目；无论
  输出到文件还是 stdout，bundle 都不会因默认值或显式 `--assets-dir` 再向外部
  文件系统抽取一份资源。ZIP central directory 中普通文件固定为 Unix mode `0644`，
  `assets/` 目录固定为 `0755`，因此跨平台解压后目录保持可遍历且文件保持只读语义。
- 资源策略默认 `extract`。本地单文件输出到 stdout 时，资源写到当前工作目录的
  `<文档名>_assets/`；stdin 和 URI 若产生资源，必须指定 `--assets-dir`。
- 文件冲突默认改名为 `name-1.ext` 并发出 warning；`error` 拒绝写入，
  `overwrite` 通过同目录临时文件原子替换。
- extract 资源使用 `asset-<完整 SHA-256(bytes)>.<MIME 权威扩展名>` 稳定命名；
  相同内容与 MIME 的多个 ID 只写一个物理文件，并在主产物
  或任何资源写入前预检全部非空资源。为了防止 Markdown 链接因改名漂移，资源目标
  已存在时 `rename` 安全降级为 `assetConflict`；`overwrite` 才会原子替换稳定目标。
  `rename` 与 `error` 的最终写入使用原子 no-clobber；预检后出现的竞态文件会令该项
  失败而不会被覆盖。主产物与全部资源完整 stage、fsync 并写入持久事务 journal 后
  作为一个集合提交；每个物理目标父目录以身份绑定的固定 lease 保守互斥，相关输出开始
  前只从这些父目录受限恢复中断事务并有界重做预检，不扫描祖先。失败会恢复完整旧集合，恢复
  本身失败则返回 `rollbackFailed` 并保留 journal/备份，`overwrite` 不产生新旧混合
  集合。stdout 的外部资源使用同一事务，先 stage，stdout 成功或 EPIPE 后提交；非
  EPIPE 失败不落资源，但已经写出的 stdout 字节无法撤回。跨文件系统、符号链接、
  非 regular file 提前拒绝。安全输出事务当前在 Unix 可用；Windows 返回稳定
  `componentUnavailable`，但路径规划与 bundle 编码仍可使用。
- CLI 先按 POSIX、Windows drive 或 UNC 语法对 Markdown 基准目录和资源目录做词法
  规范化，再在同一 root/drive/share 内生成相对 URI path reference；合法的同卷上级
  目录使用 `../`。不同 drive/share/root、drive-relative 路径或不完整 UNC 返回稳定
  `assetPathUnsupported`，绝不输出会被解释为自定义 scheme 或网络 host 的目标。
  每个路径段保留合法 Unicode 与括号；空格、`#`、`?`、`%`、`&`、反斜杠字面量、
  Unicode 空白和控制字节按 UTF-8 百分号编码。文件名中真实的 `%20` 编码为 `%2520`；
  API 提供的 URI 中已有 `%HH` 保持稳定。文件输出以 Markdown 文件父目录
  为基准；stdout 以当前工作目录为基准。
- 多输入输出保留相对于各输入根的目录结构；不同输入根产生同名输出时先加输入根名
  前缀，仍冲突时再使用稳定数字后缀，所有消歧均在调度前完成。
- `--report` 写入带 `schemaVersion` 的 JSON 报告，包含逐项输入、输出、状态、
  `outcome`、格式、诊断、警告、错误码和细分 `reasonCode`。成功结果为 `complete` 或
  `degraded`，失败为 `failed`；真正空源使用 `complete` / `emptySource`，未证明可用的
  空产物使用 `malformed` / `emptyContent`。报告还包含逐项 `durationMs`、成功项的
  `processingDurationMs` 和独立计量的整批 `wallDurationMs`；逐项耗时排除队列与批量
  内存准入等待，整批墙钟时间不得由并发项耗时求和。顶层 `resourceUsage` 报告整个
  invocation 的共享 lease budget 与历史 peak；budget 不随 `--jobs` 倍增。OCR auto/always
  还报告过滤、去重并合并后的 `recognizedRegions` / `recognizedChars`，未命中或组件不可用
  为明确的零值，不能用候选图片或最终 Markdown 反推。
- Engine 的空结果判定发生在 stdout 编码及文件事务 stage/commit 之前。除明确
  `emptySource` 外，成功或 degraded 的 Markdown 目标必须存在且非空；`emptyContent`
  不创建目标，并使批处理失败计数和退出码与报告一致。asset-only 结果只有在所选
  输出能表示每项资源时成功：`result-json` 可保留 payload 或外部 URI；bundle 和带
  `--asset-mode extract` 的 `ir-json` 要求本地 payload。
- `--dry-run` 只展开输入、验证配置和计算输出路径，不转换、不联网、不写任何文件。

### OCR 与 AI

```text
--ocr <off|auto|always>
--ocr-language <BCP47>
--ocr-min-confidence <0..1>

--asr-language <WHISPER_LANGUAGE>
--chinese-script <preserve|simplified|traditional>
--asr-threads <1..8>
--asr-max-duration-ms <MILLISECONDS>

--ai <CAPABILITY=MODE>
--ai-provider <NAME>
--ai-model <MODEL>
--ai-prompt <CAPABILITY=FILE>
```

OCR 默认 `auto`。`--ocr-language`、`--ai` 和 `--ai-prompt` 均可重复。AI 能力为：

```text
vision-ocr
image-description
layout-repair
table-repair
formula-repair
audio-transcription
markdown-postprocess
```

每项能力的模式为 `off`、`fallback`、`prefer` 或 `only`，默认全部关闭。
`audio-transcription` 启用随包的离线 Whisper small，不需要 Provider 或网络授权；它要求
已验证的模型和发布物内固定 FFmpeg runtime。其他 AI 能力必须选择已配置 Provider，并在
本次调用显式传入 `--allow-network`。

音频与视频输入先由经 authority 校验的 FFmpeg 转成 16 kHz 单声道 PCM，再由 Whisper
生成带毫秒时间范围、语言、语言置信度和 token 平均置信度的统一 IR 节点。macOS
发布包在非沙箱进程中优先使用 Metal；受限沙箱直接使用 CPU，初始化或推理返回失败时
也会在同一任务内回退 CPU。未指定
`--asr-language` 时执行模型语言检测；`--chinese-script` 在本地确定性规范化中文字符，
不会改写时间范围。线程最多 8；默认不按分钟数拒绝，取消、总 deadline、内存、临时空间、
显式时长和 segment 上限贯穿解码及推理。转换过程不会安装模型。

### 网络与资源边界

```text
--allow-network
--allow-private-network
--allow-host <HOST>
--error-policy <best-effort|strict>
--zip-charset <CHARSET>
--max-redirects <N>
--max-input-size <SIZE>
--max-decompressed-size <SIZE>
--max-archive-entries <N>
--max-depth <N>
--max-pages <N>
--max-presentation-xml-events <N>
--max-pdf-page-objects <N>
--max-pdf-total-objects <N>
--max-pdf-layout-comparisons <N>
--max-asset-size <SIZE>
--max-total-asset-size <SIZE>
--max-memory-size <SIZE|auto>
--max-temporary-size <SIZE>
--timeout-ms <MILLISECONDS>
```

- 配置文件不能启用联网；只有当前命令行的 `--allow-network` 可以授权远程输入和
  Provider。
- 回环与私网目标还需要 `--allow-private-network`。
- `--allow-host` 只能收窄已授权范围。
- 配置与命令行的主机列表同时存在时取规范化交集；空交集在联网前拒绝。主机比较忽略
  DNS ASCII 大小写和单个尾随点，统一 IDN/Punycode 与 IP 文本形式；列表项不含端口，
  目标 URL 端口不参与匹配。
- 大小接受整数或 `KiB`、`MiB`、`GiB` 后缀。
- PPTX 每个 XML 部件默认最多 2,000,000 个非 EOF 事件（开始、空元素、结束、文本等），
  包含未选中的 MCE 分支；`--max-presentation-xml-events` 可调整且必须大于零。
  深度、几何、解压、内存与最终 IR 限制继续独立生效。Web 请求最多取默认值，可下调。
- PDF 单页原始对象默认上限 100,000，单份 PDF 累计原始对象默认上限 10,000,000，
  版面比较默认上限 12,000,000。对应 `--max-pdf-page-objects`、
  `--max-pdf-total-objects`、`--max-pdf-layout-comparisons`；均拒绝零值，单页最多
  10,000,000。最终 IR、资产、页数、内存和执行时间继续独立限制。
- 本地 CLI 的 `auto` 在每次调用开始时探测一次总内存 T 与可用内存 A，系统余量
  R = max(1 GiB, T / 8)，共享预算为 min(T / 2, A − R)。A 不足以覆盖余量时拒绝准入。
  仅探测到 T 时采用 min(2 GiB, T / 4)，仅探测到 A 时采用 min(2 GiB, A − 1 GiB)，
  两者均不可得时采用 2 GiB；缺失值在预算快照中保留为 null。预算覆盖整个批处理，
  不随 `--jobs` 倍增。CLI 参数优先于配置，显式数值保持原值。
- 超过统一 IR 节点阈值的大型 XLS、XLSX 与 XLSB 会按工作簿顺序切成每块最多 2048 行的 TSV fenced
  block，所有块仍写入同一个最终 Markdown，并报告 `spreadsheet.largeTablePaged`。普通
  工作簿继续输出 GFM table；分页不会放宽 `max_table_rows`、`max_table_cells` 或 ZIP
  解压边界，发布门禁可显式提高这些结构上限，同时继续由共享内存预算约束实际处理。
- `best-effort` 是默认错误策略，可恢复非关键格式兼容问题。有效 `auto` 下，若当前页面或
  内容单元已有原生正文，经过隔离 provider 结构化报告的 `ocrRecognitionMemory` 可逐图
  跳过，并产生带引用位置的 `ocr.optionalRecognitionMemorySkipped` 诊断；正文、资产及
  其他图片成功识别的贡献保留。同内容图片在本次转换内共用成功或跳过结果。
  `strict`、强制 OCR 和承载必要正文的图片保持失败语义。旧版通用 `resourceLimit`、
  结构/张量上限、全局预算、帧/协议故障、进程异常、取消与超时继续失败。既有 auto
  组件不可用诊断与动画图片路由保持原有边界。外部资源不会下载，也不自动重试。
- `--zip-charset` 只在 ZIP 缺少有效 Unicode Path extra field 和 UTF-8 标志时生效；支持
  `encoding_rs` 标签（包括 GB18030 与 Shift-JIS），未指定时使用 CP437。
- `--timeout-ms` 是覆盖解析、检测、探测、转换、OCR、AI 与渲染的总时限；超时返回
  稳定的 `timeout` 错误码；值必须大于零。无法由平台单调时钟表示的极大 library
  `Duration` 按无 deadline 处理，不会意外变成立即超时。内存限制统计执行上下文中
  显式预留的内存，临时空间限制统计请求临时文件实际写入的字节。
- 网络实现必须在 DNS 解析及每次重定向后重新执行地址与主机策略。
- HTTP(S) source 只执行 GET，不读取 proxy 环境，不缓存响应；`gzip` 会流式解压并同时受
  wire 与最终输入上限约束，其他 Content-Encoding 稳定拒绝。URL query 可以用于 signed
  source 请求，但不会进入日志、诊断、最终 URL metadata 或 redirect provenance。
- HTTP 明文目标必须同时获得网络与私网授权，并且 DNS 的全部结果均为非全局地址；公网
  source 必须使用固定 roots 的 HTTPS。服务器返回的 MIME 与文件名只是检测提示，不能
  覆盖内容 magic。

### 通用界面

```text
--config <PATH>
--no-config
--profile <NAME>
--language <en|zh-CN>
-q, --quiet
-v, --verbose
--color <auto|always|never>
--progress <auto|always|never>
--log-format <text|json>
-h, --help
-V, --version
```

`--log-format json` 将 stderr 事件编码为单行 JSON；错误对象固定包含 `code`、
`message` 和 `exitCode`，字段名、错误码和状态值不随界面语言变化。转换完成后，文本
模式在 stderr 显示逐项总耗时、Engine 转换耗时和整批墙钟时间；JSON 模式使用
`itemTiming`、`batchTiming` 事件。`--quiet` 只抑制这些显示，不移除报告中的计时字段。

下游提前关闭管道产生 `EPIPE` 时按成功处理。`--quiet` 不会抑制主产物或失败退出码。

## 管理命令

### 格式

```text
into-md formats [--family <FAMILY>] [--status <STATUS>] [--json]
into-md formats show <FORMAT> [--json]
into-md formats detect <INPUT> [-f <FORMAT>] [--extension <EXT>]
                            [--mime-type <TYPE>] [--charset <CHARSET>] [--json]
```

`detect` 只解析输入并输出候选格式，不选择转换器、不生成 Markdown。
文本输出包含 `DETECTOR` 和 `DIAGNOSTICS` 列；JSON 候选包含稳定的
`detectorId`、`reason` 和 `diagnostics` 字段。候选顺序是稳定契约。

### 能力

```text
into-md capabilities [--json]
into-md capabilities show <ocr|transcription|diarization> [--json]
into-md capabilities use <CAPABILITY> --source <SOURCE_REF> [--scope <global|project>]
into-md capabilities reset <CAPABILITY> [--scope <global|project>]
```

`SOURCE_REF` 使用 `plugin:<插件ID>/<能力ID>`、`provider:<Provider ID>/<能力ID>` 或 `off`。
OCR 可选择本地插件或远端 Provider；转写和说话人能力分别路由，因此可以组合远端转写与
本地说话人识别。Office 97–2003 由 Core 直接转换，不进入能力路由。JSON 输出包含当前来源、全部候选来源、
本地插件状态与版本；模型身份记录在经过签名的 provider 描述中，并随完整插件管理。

官方能力插件下载支持 CONNECT 代理路由：`INTO_MD_HTTPS_PROXY`（回退
`HTTPS_PROXY`、`https_proxy`）取 `http://[user:pass@]host:port`，`INTO_MD_NO_PROXY`
（回退 `NO_PROXY`、`no_proxy`）支持 `*`、精确 host 或 `.suffix` 域后缀豁免；空值视为
未设置，非法值在下载前稳定失败。`doctor` 的 `capabilityDownloads` 项会报告当前路由
（direct、代理 host:port 或非法变量及原因）。
系统证书库无法覆盖特殊 TLS 环境时，可为单次 setup 显式传入 `--insecure`，或设置
`INTO_MD_INSECURE=1`；环境变量只接受 `0`、`1`、`false`、`true`（大小写不敏感），
其他值会在下载前报配置错误。该选项会关闭 TLS 证书和握手签名验证，只应临时使用；
插件包仍必须通过目录固定的 SHA-256、发布者签名、文件清单和运行时 authority 校验。
完整的数据目录、事务和威胁边界见[能力插件](capability-plugins.md)。

官方本地能力准备入口为：

```text
into-md setup ocr [--insecure] [--allow-private-network]
into-md setup media [--insecure] [--allow-private-network]
```

这些命令显式安装并验证对应的完整能力插件。

### 转写后说话人重标记

```text
into-md transcript relabel <INPUT.md|RESULT.json> --mapping <FROM=TO>... \
  [-o <OUTPUT>] [--conflict <error|overwrite|rename>] [--json]
```

`transcript relabel` 只修改已有转写产物中的说话人标签，不重新解码媒体、不执行 ASR 或
说话人分离，也不访问网络。输入、mapping 和输出结构会完整校验；默认使用冲突保护。

### AI Provider

```text
into-md providers [--json]
into-md providers show <NAME> [--json]
into-md providers add <NAME> --type openai-compatible \
  --base-url <URL> --model <MODEL> --api-key-env <ENV> \
  [--capability <NAME>] [--model-map <CAPABILITY=MODEL>] \
  [--timeout <DURATION>] [--scope <global|project>]
into-md providers remove <NAME> [--scope <global|project>]
into-md providers set-default <NAME> [--scope <global|project>]
into-md providers capabilities <NAME> [--json]
into-md providers test <NAME> --allow-network [--allow-private-network]
                            [--allow-host <HOST>]...
```

CLI 只接受 API Key 的环境变量名，不提供明文 `--api-key`。`--model` 是未映射能力的默认
远端模型；每个 `--model-map` 必须对应同一次命令声明的
`--capability`。例如百炼 Provider 可把 `vision-ocr` 映射到 OCR 模型，同时把
`audio-transcription` 映射到语音模型。本地插件内置模型不出现在此映射或 Provider 页面。
`providers test` 只允许
发送最小能力探测请求，不发送文档内容。远程输入、转换选定的 AI Provider 与
`providers test` 共用相同的 URL 策略：仅允许无用户信息的 HTTP(S) URL，并执行配置
与当前调用的主机交集及私网双重授权检查，之后才能进入具体后端。
Provider base URL 还必须是字节级 canonical 且不含 query/fragment。公网目标仅接受 HTTPS；
HTTP 只用于本次命令同时传入 `--allow-network --allow-private-network` 的非公网目标。
探测返回固定 `schemaVersion`、配置模型是否可见、模型数量和经服务端证据与配置 allowlist
相交后的 capability。`/models` 本身不证明具体多模态或修复能力，因此该探测不会仅凭配置
扩大 capability；分页未完整时返回稳定 incomplete 错误。传输不会跟随重定向或把
Authorization 转发到其它 origin。

### 插件

```text
into-md plugins [--json]
into-md plugins show <ID> [--json]
into-md plugins install <PATH-OR-HTTPS-URL> [--sha256 <HASH>] \
  [--signing-key-id <ID> --signing-key-sha256 <SHA256>] \
  [--scope <global|project>]
into-md plugins verify [ID] [--json] [--scope <global|project>]
into-md plugins enable <ID> [--scope <global|project>]
into-md plugins disable <ID> [--scope <global|project>]
into-md plugins run <ID> <INPUT> --input-format <FORMAT> \
  [--scope <global|project>]
into-md plugins remove <ID> [--scope <global|project>]
```

本地包可用已经信任的发行者安装，也可同时传入签名 key ID 与 Ed25519 公钥指纹建立全局
信任；HTTPS 安装还必须固定完整包 SHA-256。包内 `plugin.json`、目标、文件集合、可执行
权限、大小与摘要在安装和加载时都会重新验证。OCR/音频能力描述位于包内
`provider.json`，执行仍统一走 `process-v1`；普通插件还可使用 `wasi-v1`。所有形式都不
加载 Rust 动态 ABI。详见
[OCR 与音频能力插件](capability-plugins.md)。

### 配置、诊断与补全

```text
into-md config paths [--json]
into-md config show [--resolved] [--format <toml|json>]
into-md config init --scope <global|project> [--force]
into-md config validate [PATH]
into-md config get <KEY>
into-md config set <KEY> <VALUE> [--scope <global|project>]
into-md config unset <KEY> [--scope <global|project>]
into-md config profile list
into-md config profile create <NAME> [--from <NAME>] [--scope <global|project>]
into-md config profile remove <NAME> [--scope <global|project>]

into-md doctor [--json] [--allow-network] [--allow-private-network]
into-md completions <bash|zsh|fish|powershell|elvish>
into-md version [--json]
```

`doctor` 默认离线检查配置、平台、模型清单、推理运行时、Provider 环境变量和临时
目录。网络探测只有在当前命令显式授权时才允许运行。

## 退出码

| 退出码 | 含义 |
| ---: | --- |
| 0 | 成功 |
| 2 | 用法或配置错误 |
| 3 | 不支持、无转换器、损坏或加密输入 |
| 4 | 本地 I/O 或输出错误 |
| 5 | 安全策略或资源限制 |
| 6 | OCR、模型或推理运行时错误 |
| 7 | AI Provider 错误 |
| 8 | 网络错误 |
| 9 | 模型、插件供应链或组件不可用 |
| 10 | 批量任务部分失败 |
| 70 | 内部错误 |
| 130 | 用户取消 |

当前 Core、格式 catalog、能力路由和插件生命周期均为公开产品接口。格式、能力插件或
Provider 后端缺失时，命令返回稳定错误，不会执行未授权网络操作、创建虚假安装状态或 panic。

`ir-json` 使用 Document IR 的公共版本化契约；`result-json`、Bundle manifest 和
`--report` 使用与本地 Web 服务共享的公共 DTO。Bundle schema 1 内的
diagnostics/provenance 保持裸数组成员形状，其版本由 manifest 统辖；独立 HTTP 响应
使用带 `schemaVersion` 的 envelope。
`result-json` 通过公共 DTO 的 `Pretty` 借用写接口生成：完整缩进后 wire 预算在任何
base64 编码前验证，资源内容再以固定小缓冲写入唯一最终输出缓冲，不构造 owned DTO 或 base64
String 副本。
字段、兼容及验证规则以[稳定数据传输契约](dto.md)为准，CLI 不维护另一套私有 JSON
结构。
