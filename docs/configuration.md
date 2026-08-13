# 配置文件

`into-md` 使用严格、带 Schema 版本的 TOML 配置。未知字段直接报错，避免拼写错误
被静默忽略。配置中只保存 Provider 密钥的环境变量名，不保存密钥值。

## 加载顺序

配置按以下顺序合并，后者覆盖前者：

1. 内置安全默认值。
2. 操作系统全局配置。
3. 从当前目录向上找到的最近 `.into-markdown.toml`。
4. 按命令行出现顺序加载的 `--config <PATH>`。
5. 每个配置层中选定的命名 profile。
6. 命令行参数。

`--no-config` 关闭全局和项目自动发现，但仍加载显式 `--config`。`--profile` 优先于
`INTO_MD_PROFILE`。

项目配置可以启用已安装插件和设置 Provider，但以下权限不能写入任何配置：

- 联网授权；
- 回环或私网授权；
- 明文 API Key。

每次需要远程来源或 Provider 时，都必须在当前调用传入 `--allow-network`。私网或
本机服务还需要 `--allow-private-network`。

## 示例

```toml
schema_version = 1
default_provider = "local-vision"

[cli]
language = "zh-CN"
jobs = 8
color = "auto"
progress = "auto"
log_format = "text"

[conversion]
timeout_ms = 120000

[conversion.ocr]
policy = "auto"
model_bundle = "pp-ocrv6-tiny-zh-en"
languages = ["zh-Hans", "zh-Hant", "en"]
minimum_confidence = 0.70

[conversion.text]
# strict（默认）或 replace；replace 会逐段输出带原始字节范围的诊断。
decoding_mode = "strict"

[conversion.delimited_text]
header = "auto" # auto、always 或 never
ragged_rows = "strict" # strict 或 pad

[conversion.ai]
vision_ocr = "fallback"
image_description = "off"
layout_repair = "off"
table_repair = "fallback"
formula_repair = "fallback"
audio_transcription = "off"
markdown_postprocess = "off"
provider = "local-vision"

[conversion.network]
max_redirects = 3
allowed_hosts = ["api.example.com"]
deny_private_networks = true

[conversion.limits]
max_input_bytes = 536870912
max_decompressed_bytes = 1073741824
max_archive_entries = 100000
max_nesting_depth = 256
max_pages = 10000
max_asset_bytes = 268435456
max_total_asset_bytes = 1073741824
max_memory_bytes = 1073741824
max_temporary_bytes = 1073741824
max_table_rows = 100000
max_table_columns = 16384
max_table_cells = 1000000
max_field_bytes = 16777216

[conversion.output]
emit = "markdown"
asset_mode = "extract"
conflict = "rename"
asset_directory_suffix = "_assets"
include_provenance = true

[providers.local-vision]
type = "openai-compatible"
base_url = "http://127.0.0.1:11434/v1"
model = "vision-model"
api_key_env = "LOCAL_VISION_API_KEY"
timeout_ms = 60000
capabilities = ["vision-ocr", "image-description", "table-repair"]

[plugins.corporate-parser]
source = "/opt/into-markdown/plugins/corporate-parser.wasm"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
protocol = "wasi-v1"
enabled = true

[profiles.quality.conversion.ocr]
policy = "always"
minimum_confidence = 0.85

[profiles.quality.conversion.ai]
layout_repair = "fallback"
table_repair = "prefer"
formula_repair = "prefer"
```

配置中的 `allowed_hosts`、重定向和私网策略只能收窄权限，不能启用网络。上例调用
本机 Provider 时仍需：

```shell
into-md report.pdf --profile quality --allow-network --allow-private-network
```

若配置与当前调用都指定主机 allowlist，最终范围是两者规范化后的交集；只有一方非空
时保留该方。交集为空会在联网前以 `hostAllowlistConflict` 拒绝，不会把命令行列表
当成对配置限制的替换。主机按 URL 标准规范化：DNS 名不区分 ASCII 大小写，允许一个
表示 DNS 根的尾随点，IDN 转为 ASCII Punycode，IP 地址使用标准文本形式。allowlist
条目只接受主机或方括号包裹的 IPv6，不接受 scheme、路径或端口；目标 URL 的端口不
参与主机匹配。

回环、IPv4 私网与链路本地地址、IPv6 ULA (`fc00::/7`) 与链路本地地址
(`fe80::/10`) 均视为私网。Provider 传输只把保守判定为 global-only 的地址视为公网；
IPv4-mapped/translated IPv6、6to4、ORCHID、benchmark、文档和其它 IANA special-purpose
范围都需要额外私网授权。Provider URL 与远程输入采用同一套规则，且 URL 不得包含用户名
或密码。远程输入允许 query 以支持 signed URL，但 source metadata、redirect provenance、
日志和诊断都会移除 userinfo、query 与 fragment。每个 redirect 的目标必须继续落在有效
host allowlist 内，并重新通过 DNS/IP 与私网授权；配置不能放宽这些检查。公网 HTTP 明文
稳定拒绝，HTTP 仅用于当前调用明确双授权的非全局目标。

## 配置操作

```shell
into-md config paths
into-md config init --scope global
into-md config init --scope project
into-md config validate
into-md config show --resolved
into-md config get conversion.ocr.policy
into-md config set conversion.ocr.policy '"always"' --scope project
into-md config unset conversion.ai.provider --scope project
into-md config profile create quality --scope project
```

`config set` 首先把值解析为 TOML 标量、数组或内联表；无法解析时按字符串处理。
写入使用同目录临时文件和原子替换。`config show --resolved` 会遮蔽秘密字段，并从
可能包含签名参数的 URL（包括 Provider URL 与插件来源）中移除用户信息、query 与
fragment。resolved 输出的 `_sources` 表按点分字段名记录最终值来自内置默认值、
具体配置文件、该文件中的 Profile、环境变量还是当前命令行。该表是解释元数据，
不是可写回的配置字段。

显式配置的相对路径以调用时的当前目录解析；绝对路径保持不变。配置值中的 POSIX
路径与 Windows 路径按原字符串保存，不做平台相关改写。

## Profile 语义

Profile 是同一配置层中的覆盖表。选定 profile 后，先合并该层的普通配置，再合并
profile 内容，然后继续处理更高优先级配置层。Profile 可以覆盖转换、CLI、
Provider 和插件配置，但不能赋予联网或私网权限。

普通配置层和 Profile 都可以只写 Provider 的部分字段，以覆盖更低层的同名
Provider。每一层仍会拒绝未知字段并校验该层实际提供的 URL、类型、环境变量名和能力；
全部层合并后，Provider 必须包含 `type`、`base_url`、`model` 与 `api_key_env`，否则
配置整体无效。单独校验一个不完整文件也会失败。

`conversion.network.deny_private_networks` 只能省略或设为 `true`；设为 `false` 会被
拒绝。配置文件和 Profile 中的 `allow_network`、`allow_private_network` 及任何未知
字段同样会被拒绝。联网和私网授权不参与配置合并，只读取当前调用的命令行参数。

若指定的 profile 在所有加载层中都不存在，命令返回配置错误，不静默回退。

## 环境变量

| 环境变量 | 用途 |
| --- | --- |
| `INTO_MD_PROFILE` | 未传 `--profile` 时选择命名 profile |
| `INTO_MD_LANGUAGE` | 未传 `--language` 时选择 `en` 或 `zh-CN` |
| Provider 的 `api_key_env` 值 | 由对应 Provider 在执行时读取 API Key |

环境变量中的 Provider 密钥不得写入日志、JSON 结果、诊断、溯源或 Bundle。
