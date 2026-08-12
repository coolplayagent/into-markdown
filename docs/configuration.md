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

[conversion.ocr]
policy = "auto"
model_bundle = "pp-ocrv6-tiny-zh-en"
languages = ["zh-Hans", "zh-Hant", "en"]
minimum_confidence = 0.70

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
可能包含签名参数的 URL 中移除 query 与 fragment。

## Profile 语义

Profile 是同一配置层中的覆盖表。选定 profile 后，先合并该层的普通配置，再合并
profile 内容，然后继续处理更高优先级配置层。Profile 可以覆盖转换、CLI、
Provider 和插件配置，但不能赋予联网或私网权限。

若指定的 profile 在所有加载层中都不存在，命令返回配置错误，不静默回退。

## 环境变量

| 环境变量 | 用途 |
| --- | --- |
| `INTO_MD_PROFILE` | 未传 `--profile` 时选择命名 profile |
| `INTO_MD_LANGUAGE` | 未传 `--language` 时选择 `en` 或 `zh-CN` |
| Provider 的 `api_key_env` 值 | 由对应 Provider 在执行时读取 API Key |

环境变量中的 Provider 密钥不得写入日志、JSON 结果、诊断、溯源或 Bundle。
