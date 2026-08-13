# 命令行设计

`into-md` 同时面向交互式终端、Shell 管道、批处理任务和自动化系统。主产物始终
写入 stdout 或显式输出路径；日志、进度、警告和诊断写入 stderr。命令不得交互式
询问密码或联网授权。

## 命令结构

转换不使用 `convert` 子命令：

```text
into-md [选项] <输入...>
```

管理命令为：

```text
into-md ui
into-md formats
into-md models
into-md providers
into-md plugins
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

### 本地 Web 服务

```text
into-md ui [--port <0..65535>] [--no-open] [--data-dir <目录>]
```

`ui` 固定绑定 `127.0.0.1`；默认 `--port 0` 由操作系统分配空闲端口，不存在 host、
外部监听地址或外部会话令牌选项。`--no-open` 禁止启动浏览器。`--data-dir` 指定私有
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
- 资源策略默认 `extract`。本地单文件输出到 stdout 时，资源写到输入同级
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
  每个路径段中的空格、`#`、`?`、`%`、Unicode、反斜杠字面量和控制字节按 UTF-8
  百分号编码，渲染器保留这些 `%HH` 而不二次编码。文件输出以 Markdown 文件父目录
  为基准；stdout 以当前工作目录为基准。
- 多输入输出保留相对于各输入根的目录结构；不同输入根产生同名输出时先加输入根名
  前缀，仍冲突时再使用稳定数字后缀，所有消歧均在调度前完成。
- `--report` 写入带 `schemaVersion` 的 JSON 报告，包含逐项输入、输出、状态、
  格式、诊断、警告和错误码。
- `--dry-run` 只展开输入、验证配置和计算输出路径，不转换、不联网、不写任何文件。

### OCR 与 AI

```text
--ocr <off|auto|always>
--ocr-model <BUNDLE_ID>
--ocr-language <BCP47>
--ocr-min-confidence <0..1>

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

每项能力的模式为 `off`、`fallback`、`prefer` 或 `only`，默认全部关闭。启用任意
AI 能力必须选择已配置 Provider，并在本次调用显式传入 `--allow-network`。

### 网络与资源边界

```text
--allow-network
--allow-private-network
--allow-host <HOST>
--max-redirects <N>
--max-input-size <SIZE>
--max-decompressed-size <SIZE>
--max-archive-entries <N>
--max-depth <N>
--max-pages <N>
--max-asset-size <SIZE>
--max-total-asset-size <SIZE>
--max-memory-size <SIZE>
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
- `--timeout-ms` 是覆盖解析、检测、探测、转换、OCR、AI 与渲染的总时限；超时返回
  稳定的 `timeout` 错误码；值必须大于零。无法由平台单调时钟表示的极大 library
  `Duration` 按无 deadline 处理，不会意外变成立即超时。内存限制统计执行上下文中
  显式预留的内存，临时空间限制统计请求临时文件实际写入的字节。
- 网络实现必须在 DNS 解析及每次重定向后重新执行地址与主机策略。

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
`message` 和 `exitCode`，字段名、错误码和状态值不随界面语言变化。

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

### 模型

```text
into-md models [--json]
into-md models show <ID> [--json]
into-md models install [ID]
into-md models verify [ID] [--json]
into-md models remove <ID>
into-md models path <ID>
```

转换过程不会自动下载模型。下载必须使用固定来源和 SHA-256，先校验再原子安装。
只读发布模型不能被删除。`list`、`show`、`verify`、`path` 全程离线；JSON 输出包含
`schemaVersion`、availability、state、ownership 和安装路径。当前清单只有上游
source archives，尚无经审核的最终 ONNX/字符表文件，因此显示为
`planned` / `unavailable`；`install`、`verify`、`path`、`remove` 对该 source-only
条目均稳定返回 `componentUnavailable`。完整的清单、数据目录、事务和威胁边界见
[本地模型管理](models.md)。

### AI Provider

```text
into-md providers [--json]
into-md providers show <NAME> [--json]
into-md providers add <NAME> --type openai-compatible \
  --base-url <URL> --model <MODEL> --api-key-env <ENV> \
  [--capability <NAME>] [--timeout <DURATION>] [--scope <global|project>]
into-md providers remove <NAME> [--scope <global|project>]
into-md providers set-default <NAME> [--scope <global|project>]
into-md providers capabilities <NAME> [--json]
into-md providers test <NAME> --allow-network [--allow-private-network]
                            [--allow-host <HOST>]...
```

CLI 只接受 API Key 的环境变量名，不提供明文 `--api-key`。`providers test` 只允许
发送最小能力探测请求，不发送文档内容。远程输入、转换选定的 AI Provider 与
`providers test` 共用相同的 URL 策略：仅允许无用户信息的 HTTP(S) URL，并执行配置
与当前调用的主机交集及私网双重授权检查，之后才能进入具体后端。
Provider base URL 还必须是字节级 canonical 且不含 query/fragment。公网目标仅接受 HTTPS；
HTTP 只用于本次命令同时传入 `--allow-network --allow-private-network` 的非公网目标。
探测返回固定 `schemaVersion`、配置模型是否可见、模型数量和配置 capability；不会跟随
重定向或把 Authorization 转发到其它 origin。

### 插件

```text
into-md plugins [--json]
into-md plugins show <ID> [--json]
into-md plugins install <PATH|URL> [--sha256 <HASH>] [--scope <global|project>]
into-md plugins verify [ID] [--json]
into-md plugins enable <ID> [--scope <global|project>]
into-md plugins disable <ID> [--scope <global|project>]
into-md plugins remove <ID> [--scope <global|project>]
```

URL 安装仅接受 HTTPS 且必须提供 SHA-256；本地包记录计算后的哈希。允许的协议为
`process-v1` 和 `wasi-v1`，不加载 Rust 动态 ABI。

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

当前工程仍是转换后端脚手架。格式、模型运行时产物、插件或 Provider 后端缺失时，命令返回
稳定错误，不会执行网络操作、创建虚假安装状态或 panic。

`ir-json` 使用 Document IR 的公共版本化契约；`result-json`、Bundle manifest 和
`--report` 使用与未来 HTTP 服务共享的公共 DTO。Bundle schema 1 内的
diagnostics/provenance 保持裸数组成员形状，其版本由 manifest 统辖；独立 HTTP 响应
使用带 `schemaVersion` 的 envelope。
`result-json` 通过公共 DTO 的 `Pretty` 借用写接口生成：完整缩进后 wire 预算在任何
base64 编码前验证，资源内容再以固定小缓冲写入唯一最终输出缓冲，不构造 owned DTO 或 base64
String 副本。
字段、兼容及验证规则以[稳定数据传输契约](dto.md)为准，CLI 不维护另一套私有 JSON
结构。
