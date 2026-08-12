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
into-md formats
into-md models
into-md providers
into-md plugins
into-md config
into-md doctor
into-md completions
into-md version
```

若文件名与管理命令同名，应在输入前使用 `--`：

```shell
into-md -- formats
```

无参数且 stdin 连接管道时自动读取 stdin；无参数且 stdin 为终端时显示帮助。
`into-md -` 始终显式表示 stdin。stdin 不能与其他输入组合。

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
```

显式格式优先于检测结果。扩展名可以带或不带前导点，Office 变体扩展名会映射到
对应格式族。

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
  会被净化并稳定排序。
- 资源策略默认 `extract`。本地单文件输出到 stdout 时，资源写到输入同级
  `<文档名>_assets/`；stdin 和 URI 若产生资源，必须指定 `--assets-dir`。
- 文件冲突默认改名为 `name-1.ext` 并发出 warning；`error` 拒绝写入，
  `overwrite` 通过同目录临时文件原子替换。
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
```

- 配置文件不能启用联网；只有当前命令行的 `--allow-network` 可以授权远程输入和
  Provider。
- 回环与私网目标还需要 `--allow-private-network`。
- `--allow-host` 只能收窄已授权范围。
- 配置与命令行的主机列表同时存在时取规范化交集；空交集在联网前拒绝。主机比较忽略
  DNS ASCII 大小写和单个尾随点，统一 IDN/Punycode 与 IP 文本形式；列表项不含端口，
  目标 URL 端口不参与匹配。
- 大小接受整数或 `KiB`、`MiB`、`GiB` 后缀。
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
只读发布模型不能被删除。

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

当前工程仍是转换后端脚手架。格式、模型、插件或 Provider 后端缺失时，命令返回
稳定错误，不会执行网络操作、创建虚假安装状态或 panic。

`ir-json` 使用 Document IR 的公共版本化契约；`result-json`、Bundle manifest、Bundle
内的 diagnostics/provenance 和 `--report` 使用与未来 HTTP 服务共享的公共 DTO。
字段、兼容及验证规则以[稳定数据传输契约](dto.md)为准，CLI 不维护另一套私有 JSON
结构。
