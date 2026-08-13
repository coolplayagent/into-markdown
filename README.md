# into-markdown

[English](README.en.md)

`into-markdown` 是一个使用 Rust 开发、由 Bazel 构建的文档转 Markdown
平台。仓库当前提供架构设计、公共服务提供者接口、注册表与转换流水线、确定性
GFM 渲染器、TXT/Markdown/CSV/TSV/JSON/XML 与字符集转换器、固定的 ONNX Runtime CPU 安全运行层、命令行程序
及契约测试；当前模型清单尚无可执行 ONNX 产物，因此 OCR 请求会稳定报告模型不可用，
不会把 Paddle source archive 当作模型。网络客户端与 LLM 调用尚未实现。

本项目完全独立于相邻的 `anydoc` 和 `markitdown` 项目实现。包括 PDF、OCR
和 AI 生成内容在内的所有输入，都必须先进入带溯源信息的统一中间表示（IR），
再由中央渲染器生成 GitHub Flavored Markdown（GFM）。

## 构建

```shell
bazel build //...
bazel test //...
cargo check --workspace
```

支持的目标平台为 macOS ARM64、Linux x86_64、Linux ARM64 和 Windows
x86_64。项目明确不支持 macOS x86_64。

## 命令行

```shell
bazel run //apps/cli:into-md -- report.pdf
bazel run //apps/cli:into-md -- notes.txt
printf 'caf\351\n' | bazel run //apps/cli:into-md -- --charset windows-1252 -
bazel run //apps/cli:into-md -- table.csv
printf 'name\tage\nAlice\t42\n' | bazel run //apps/cli:into-md -- --format tsv -
bazel run //apps/cli:into-md -- report.pdf -o report.md
bazel run //apps/cli:into-md -- documents/ --recursive --output-dir markdown/
bazel run //apps/cli:into-md -- formats
bazel run //apps/cli:into-md -- models
bazel run //apps/cli:into-md -- models show pp-ocrv6-tiny-zh-en --json
bazel run //apps/cli:into-md -- doctor
bazel run //apps/cli:into-md -- ui
```

CLI 采用直接输入形式，不提供 `convert` 子命令。支持多文件与目录批量处理、stdin、
URI、OCR/AI 策略、结构化 JSON、资源 Bundle、分层配置、Provider、模型与插件管理。
联网与 AI 默认关闭，远程输入和 Provider 每次都需要显式 `--allow-network`。

`into-md ui` 启动固定监听 `127.0.0.1` 的本地 Web 安全入口，默认使用系统分配端口并
打开浏览器。它生成独立的高熵会话值，通过 URL fragment 交给嵌入页面；API 同时校验
精确 Host、Origin 和会话 Header。当前页面明确报告文档控制台不可用，不包含任务、
数据库或完整前端功能。用法和威胁边界见[本地 Web 服务](docs/ui.md)。

转换结果和批量报告使用 CLI 与未来 HTTP 服务共享的公共 DTO schema 1。例如
`--emit result-json` 返回 `markdown`、版本化
`document`、base64 `assets`、`diagnostics` 和 `provenance`。协议细节、兼容策略与
不可信 JSON 资源预算见[稳定数据传输契约](docs/dto.md)。

TXT 转换可用，支持 UTF-8、带 BOM 的 UTF-16 与受限的常见字符集自动检测；显式
`--charset` 支持 `windows-1252`、`gb18030`、`big5` 和 `shift_jis`。无效序列默认
严格失败，`--encoding-errors replace` 会替换并输出带原始字节范围和替换数量的诊断。
自动检测会为完整 JSON 及具备三行和类型证据的 CSV/TSV 候选让路；带 BOM 的输入也会
按实际字符集解码完整内容，除 TAB、LF、CR 外的 C0、DEL 或 C1 都会拒绝自动 TXT 候选。

CSV 与 TSV 转换支持 RFC 4180 引号、双引号转义、字段内换行、空单元格和 UTF-8/UTF-16
BOM，并复用 TXT 的安全字符集解码。`--table-header auto|always|never` 控制表头，
`--ragged-rows strict|pad` 控制不等宽记录；默认保守识别表头并严格拒绝不等宽记录。
所有值作为文本进入 IR，中央 GFM 渲染器负责 pipe 与换行转义。

JSON 与 XML 转换可用。JSON 严格校验 RFC 8259、拒绝重复键、保留对象源顺序与数字
lexeme；XML 支持 UTF-8 和 UTF-16LE/BE，保留 QName、namespace、属性源顺序、mixed text、
CDATA 与 PI，并把注释记录在文档 metadata。XML 的 DTD、自定义/外部实体稳定拒绝，
两种格式的 provenance 都使用原始输入字节范围。完整 JSON 顶层标量也会自动识别；XML 每个
属性的 QName/value 具有独立原始 byte span，UTF-16 映射复用 compact run decoder。

Markdown/GFM 转换支持标题、强调和删除线、链接与 autolink、嵌套列表和任务列表、
表格、代码块及脚注，并保存 UTF-8 原始字节范围。独立段落的安全 HTTP(S) 图片以
external-only Asset 保持结构化且不下载；inline、相对或危险目标明确诊断并安全降级。
raw HTML 与 blockquote 在现有 IR 中保存为不可执行代码容器并产生诊断。
详细检测、编码、资源与降级规则见[格式矩阵](docs/formats.md)。

模型查询、离线校验、路径和安全清理后端已实现；当前权威清单只有上游 source
archives，没有可安装的最终 ONNX/字符表产物，因此安装返回稳定
`componentUnavailable`；校验、路径和清理对该 source-only 条目返回同一错误，
不会读取伪造安装状态或伪装成功。其他尚未可用的格式转换、OCR 推理、Provider 请求和插件
执行后端尚未实现。Windows 模型安装在 reparse-safe 目录 handle 持久同步完成审计前
同样 fail closed；目录解析和离线元数据查询不受影响。

抽取资源按完整内容 SHA-256 去重并使用 MIME 权威扩展名；主 Markdown 与全部资源
通过带持久 journal 的同一输出事务提交，进程中断后会恢复为完整旧集合或完成完整
新集合。每个物理目标父目录使用身份绑定的固定 lease，下一次相关输出会在写出前恢复
并有界重做预检，不扫描祖先或无关目录。该安全写出事务当前在 Unix 平台可用；Windows 返回稳定
`componentUnavailable`，资源规划与 bundle 编码不受影响。portable bundle manifest 使用
`schemaVersion: 2`，以
`sourceAssetIds` 表达多个文档资源 ID 到一个物理条目的映射。

实现路线详见[架构设计](docs/architecture.md)、[接口契约](docs/interfaces.md)、
[格式矩阵](docs/formats.md)、[OCR 与 AI](docs/ocr-and-ai.md)、
[本地模型管理](docs/models.md)、
[安全模型](docs/security.md)和[测试策略](docs/testing.md)。
命令与配置契约详见[命令行设计](docs/cli.md)和[配置文件](docs/configuration.md)，
许可证、第三方来源和发布审计详见[许可证治理](docs/licensing.md)。
