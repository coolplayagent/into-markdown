# #339：统一格式支持检查与 HTML IR 证据

## 范围与来源

自动格式准入复用格式目录和精确 MIME 映射。文件签名、OLE 流名和 ZIP 包身份优先于
后缀、MIME 及文本启发式；显式格式继续调用指定解析器并执行完整安全校验。缺少可靠
二进制身份的未知后缀/具体 MIME 返回 `unsupported`。JS 按拒绝输入验收，保持现有格式目录。
参考 AnyDoc 的
[`detect.rs`，261fc257d17c3eab0f673be31c408fd9fdc2171a](https://github.com/firecrawl/anydoc/blob/261fc257d17c3eab0f673be31c408fd9fdc2171a/src/formats/detect.rs)
中的签名、OLE 流和 ZIP 包识别顺序；实现复用本仓库的受界解析器，无新增运行时依赖。

原报告的 1 个 TXT、1 个 HTML、3 个 JS 均未取得。以下公开语料和自建样本分别统计；
这些结果仅证明列出的输入、配置与构建，原报告五个文件的实际编码、配置和触发条件仍待确认。

固定对照基线：`26aaf8325ed31ce72921c84f0239574718d72257`。实现分支随后集成
`172277d9e0f72226f7a22a9fd79652890f9541f5` 的 Draw.io 支持，复用其 MIME 模块。
#338 的 RAR 识别与归档解析属于独立 PR；本项通过共同检测入口执行支持检查。

## 公开语料：36 个原始文件

[来源清单](issue-339-corpus-manifest.json)逐项固定仓库、提交、路径、原始 URL、许可证、
字节数、SHA-256、预期结果和正文片段。原始文件仅下载到忽略的测试缓存，证据不提交上游正文。

| 类别 | 数量 | 固定来源 | 预期 |
|---|---:|---|---|
| TXT | 12 | CPython `bcee1c322115c581da27600f2ae55e5439c027eb`，PSF-2.0 | 11 个转换；1 个含 C1 控制字符，保留现有安全拒绝 |
| HTML | 12 | jQuery `71c0dd14927c41d1aab5ce5ef2687d7808a4186b`，MIT | 10 个保留正文；2 个无可见正文，保留空内容错误 |
| JS | 12 | 同一 jQuery 提交，MIT | 全部 `unsupported`，包括含 HTML/JSON 字符串的源码 |

每个文件分别执行 `best-effort`、`strict`，共 72 次转换，并单独运行检测。
[基线 CLI 报告](issue-339-baseline.json)记录 48/72 满足新契约，24 个差异全部来自 JS
此前被接受为文本；TXT 与 HTML 原始语料没有新增行为差异。
[修复 CLI 报告](issue-339-fixed.json)记录候选、命令、错误、正文摘要和 IR provider。

[基线 probe 报告](issue-339-detection-baseline.json)与
[修复 probe 报告](issue-339-detection-fixed.json)另记录候选置信度、实际 converter probe、
最终选定格式、完整有效转换选项、诊断和 `Document::validate()` 结果。
拒绝发生在准入阶段的输入没有 probe；这与解析器失败分开记录。

基线和修复版本使用独立 Cargo target 目录，并清理 Core 包后重新构建所有依赖该包的项目。
早期共享 target 的观测曾发生旧库复用，已排除，归档报告使用隔离构建重新执行的结果。

## 自建与仓库 fixture 回归

以下测试独立于上述 36 个公开样本，不能计入真实语料数量。

[最小输入清单](issue-339-minimal-manifest.json)固定 10 个自建输入的 Base64/SHA-256，
并引用同一仓库 PPT 的 3 个改名变体。[基线](issue-339-minimal-baseline.json)与
[修复](issue-339-minimal-fixed.json)按同一 API helper 记录实际路由：PPT 的 Markdown/CSV
误路由及 HTML marks/空链接错误均有前后对照。该 helper 仅枚举 7 个文本转换器的 probe；
二进制格式的最终选择来自真实转换的 ExecutionContext。

- `format_admission_tests`：10 种未知后缀 × 5 类内容 × 2 种策略，包括空文件、完整
  JSON/XML/HTML、误导 MIME、显式字符集；另覆盖显式格式和旧式自定义 detector 注册。
- MIME 忽略大小写与参数，支持后缀与未知 MIME 保留冲突诊断；无后缀、普通 stdin、
  `application/octet-stream` 保留通用检测，未知 image/audio MIME 拒绝。
- 仓库真实 DOC/PPT/XLS、DOCX/PPTX/XLSX、PNG fixture：原文件与 `.js/.md/.csv/.bin`、
  无后缀对照实际选定转换器、完整 Markdown、资产数量和合法 IR；误导 JS MIME 同时存在。
  PDF `structures.pdf` 用固定目标 PDFium 单独执行原生页结构和正文等价测试。
- TXT 不完整 UTF-8 JSON/XML、BOM、UTF-16 JSON、全长二进制扫描及检测窗口边界保护在
  基线已存在。本次补强：UTF-16 LE/BE 的不完整 XML 声明从高分 XML 降为弱证据。
  对照中基线已由 XML probe 拒绝该候选并路由 Text，因此这是候选优先级修正；
  当前样本没有复现原报告 TXT 转换失败。合法 JSON/XML/CSV 保留内容识别。
- HTML 在追加 mark 时按首次出现顺序去重；覆盖粗体、斜体、删除线、下划线和上下标。
  同时存在上标与下标仍执行既有 IR 互斥校验。空链接按安全 base/source URI 解析；
  无有效目标时保留样式和文字，诊断定位到可靠的原始输入字节范围。
- DOCX altChunk、Feed、MSG 复用 HTML 的回归及独立 EPUB XHTML 回归验证正文与 IR；
  ZIP 同时包含被拒绝的源码、正常 TXT 与改名 Office，保留既有成员错误策略。
- CLI 文件/stdin/显式格式/批量，Web 真实上传，API/嵌套 ZIP 共用准入；低预算、取消、
  超时继续返回各自错误。准备阶段选定格式后组装服务；改名图片和含图 Office 的绑定 OCR
  provider 实际被调用，媒体能力选择与扩展名解耦。

主线 Draw.io 曾在文本叶节点排序去重；本项在传播样式时去重，保留首次出现顺序，
并避免嵌套重复样式消耗额外 mark 预算。新增 Draw.io 的 MIME、根节点识别和 CLI 回归保留。

## 执行与复现

在基线和候选 worktree 中分别使用独立 `CARGO_TARGET_DIR=target/cargo`：

```sh
export CARGO_TARGET_DIR=target/cargo
cargo build --locked -p into-markdown-cli
python3 tools/text-html-corpus-evidence.py \
  --manifest docs/evidence/issue-339-corpus-manifest.json \
  --cache target/issue339/corpus --fetch \
  --binary target/cargo/debug/into-md --source-revision "$(git rev-parse HEAD)" \
  --report target/issue339-corpus-report.json --enforce
cargo run --locked -p into-markdown --example text_html_detection_evidence -- \
  docs/evidence/issue-339-corpus-manifest.json target/issue339/corpus \
  target/issue339-probe-report.json
```

基线运行同一脚本时省略 `--enforce`，因为 JS 的旧行为预期与新契约不同。
CLI 报告记录 `--no-config`、OCR off、独立资产目录、绝对命令、二进制版本/SHA-256和源提交；
probe 报告记录 API 默认选项，OCR off。运行时及可选服务另由相关原生/mock 测试验证。

## 验证状态

本地、CI、安装产物分别记录；[本地命令、日志摘要与运行时哈希](issue-339-validation.json)可供复核。最终结果见本节更新及 [PR #346](https://github.com/coolplayagent/into-markdown/pull/346)。

- 本地当前主线隔离构建：Core/Engine/Converter/API 915 项通过、19 项按既有条件忽略；
  CLI exit contract 18 项通过（含 Draw.io）。公开语料 72/72 满足预期。
- 本地 Bazel：Core、Engine、Converter、API、Web typecheck/workbench、文档契约 7 个目标通过。
- 本地原生 PDF：固定 PDFium 下准入测试 9 项通过，包含单独启用的 PDF 回归。
- 本地 Clippy：受影响四个库执行 `--no-deps --lib -- -D warnings` 通过；
  非枚举顺序的嵌套样式专项通过，防止排序去重改变用户要求的首次出现顺序。
- 本地 Web 浏览器：真实 PPT 改名 `.bin` 正常转换，页面、讲者备注、图片引用保留；
  `.py` 被接受上传后在任务行就近显示格式错误；HTML 预览实际产生粗体、斜体，空链接保留标签而无空 anchor。
- 平台发布契约：72 项执行，71 通过、1 项按既有条件跳过。生产 Web 的 SPDX 与实际 JavaScript
  资产摘要绑定，CSS、bootstrap、Rust include 与 manifest 同步。
- 当前主线快速 CI：[33396806735](https://github.com/coolplayagent/into-markdown/actions/runs/33396806735)
  四平台通过；专项 CI [33396806728](https://github.com/coolplayagent/into-markdown/actions/runs/33396806728)
  四平台通过，逐平台执行72个语料策略用例。最后文档/测试提交的精确 head 状态另见 PR。
- 当前构建验证：[33396591314](https://github.com/coolplayagent/into-markdown/actions/runs/33396591314)，
  `build_only=true`、版本 0.0.4、unsigned；等待构建与安装产物验收。没有发布或替换用户安装。

全量 CLI 测试在固定基线也复现以下三项既有失败，不能计为通过：
`empty_source_and_empty_content_share_the_web_terminal_contract`，
`metadata_headroom_serializes_multiple_admission_failure_transitions_at_data_boundary`，
`permanent_store_headroom_allows_terminal_mutation_at_real_data_boundary`。
前者涉及已有 altChunk 占位内容语义；后两者为本机文件系统空间边界差 4096 字节。
全量 CLI 串行重跑为 326 通过、3 个上述基线失败、1 个既有忽略；slow-upload 退出测试重跑通过。
上述状态不改变本项准入和合法 IR 断言。

## English evidence summary

Admission is shared by CLI, API, Web and nested ZIP conversion. Reliable binary signatures and
container identities override conflicting names/MIME; unknown suffixes or concrete MIME without
such identity return `unsupported`. Explicit formats and custom detector registration remain
supported. JavaScript is accepted only as a rejection test category; no source-code converter is added.

The pinned corpus contains 12 original CPython TXT files, 12 jQuery HTML files and 12 jQuery JS
files, with immutable paths, licenses and SHA-256 values. Each runs under both error policies.
The baseline meets 48/72 updated expectations; its 24 differences are JS conversions. One TXT
with C1 controls and two HTML documents without visible content retain existing error boundaries.
Constructed regressions and repository binary fixtures are counted separately. The five files from
the original report remain unavailable, so their exact trigger conditions are unverified.

Ordered mark deduplication and safe blank-link handling preserve valid HTML IR across shared
DOCX/Feed/MSG paths; EPUB is tested separately. UTF-16 incomplete XML declarations gain consistent weak candidate priority; baseline probes
already routed these samples to Text. The reported TXT conversion failure remains unreproduced. Full binary safety,
explicit charset authority, resource limits, cancellation and timeouts remain enforced.

Machine-readable CLI and probe reports above identify actual routes, confidence, options,
diagnostics and output hashes. Local tests, four-platform CI, native-runtime tests and installed
artifact acceptance have separate statuses; known baseline CLI failures are retained explicitly.
