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
`172277d9e0f72226f7a22a9fd79652890f9541f5` 的 Draw.io 支持，复用其 MIME 模块，
并同步 `0ae0ddfe3ecefb92a1e959f0055187b49b6b57e0` 的统一 CI 策略。
#338 的 RAR 识别与归档解析已随主线 `d04d41e6ce5c1359e32eb5c3c81c9debf3d6567c` 集成。
随后集成 #341 主线 `dd1120b38e3b0dfabcf745c5d059633ce6d2d2cf` 的渲染与预览修复。
本项复用归档错误边界，不新增 RAR 解析器；目录中标记 `Unsupported` 的格式参与统一拒绝判断。
已完整检查且含成员的普通 ZIP 按 ZIP 路由，包括改名为 Office/EPUB/ODF 的输入；
空容器、存在文档包标记或检查不完整时保留兼容候选，继续由解析器报告损坏、加密或资源错误。

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
- 仓库自建 DOC/PPT/XLS、DOCX/PPTX/XLSX、PNG fixture：原文件与 `.js/.md/.csv/.bin`、
  无后缀对照实际选定转换器、完整 Markdown、资产数量和合法 IR；误导 JS MIME 同时存在。
  PDF `structures.pdf` 用固定目标 PDFium 单独执行原生页结构和正文等价测试。[独立原始二进制清单](issue-339-binary-manifest.json)固定 Microsoft MarkItDown、Apache POI
  的 DOC/PPT/XLS、DOCX/PPTX/XLSX、PDF/JPEG 共 9 个上游文件；安装包矩阵另行验证。
  其中 `test.pdf` 的目标页索引、`simple.doc` 的 CFB 目录被现有解析器拒绝，
  [固定基线也复现相同错误](issue-339-public-binary-baseline.json)。这两项验证路由和既有错误边界，
  不计作转换成功；其余上游文件验证正文/资产等价。
  两个固定 libarchive ZIP 原始样本另执行
  [48 个改名/策略用例](issue-339-real-zip.json)，正文和 ZIP 路由全部保持一致。
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

- 本地整合构建：生产代码 `89ffce9642c8eb82f2f133f2ebc847734c77f87b`，测试修正提交
  `973f6891982776d891971da6d54dc70f3c60a78e`。Core/Engine/Converter/API/Renderer
  **968 项通过、19 项既有忽略**；CLI **334 通过、1 项既有忽略**；CLI exit contract **18/18**。
  合入 #341 前的三项 CLI 失败已消失，历史基线和日志摘要保留在验证 JSON。
- 本地 Bazel **10 个目标通过**：Core、Engine、Converter、API、Renderer、Web
  typecheck/workbench/preview、文档契约、发布契约。受影响五个库 Clippy、Cargo fmt、许可证
  检查通过。结构门禁 **590 文件、0 违规**；发布契约 **81 通过、1 跳过**；CI 白名单 **14/14**。
- 本地原生 PDF：固定 PDFium 下准入测试 **10/10**，含启用的 PDF 回归。
  合入 #341 后重建 CLI 的[公开语料](issue-339-local-main341-corpus.json) **72/72**，
  [二进制矩阵](issue-339-local-main341-matrix.json) **120/120**，使用固定外部 PDFium、OCR off。
- [Web 实际操作记录](issue-339-web.json)分别验证安装包和合入 #341 后的本地构建：四种后缀均可上传，
  改名 PPT 保留页面、正文、备注和图片引用；`.py` 与 RAR 错误就近显示；HTML 保留语义粗体、斜体，
  空链接标签没有空 anchor。观察到的 PPT 图片引用在预览中显示为 Markdown 文字；本项记录引用与资产保留，
  不将这次检查写成图片显示成功。安装包使用 OCR auto，本地源码构建使用 OCR off。
- 当前已验证的[四项 fast checks](https://github.com/coolplayagent/into-markdown/actions/runs/33405357492)
  对应 `973f689`，全部通过。最终证据提交仍执行同一四项门禁，结果由 PR checks 保留。
  `.github/workflows/` 相对最新主线没有改动；#349 删除的专项入口保持删除。
- [正式安装验收构建](https://github.com/coolplayagent/into-markdown/actions/runs/33401363606)
  固定源码 `d1f91b59d3b25ce27eafc793086f39ef94350e4f`，版本 **0.0.4**、unsigned、build-only；
  四平台构建、Windows 黑盒与汇总共 **6 项全部通过**。
  [macOS 材料、架构与摘要检查](issue-339-installed-integrated.json)通过，
  [安装包语料](issue-339-installed-integrated-corpus.json) **72/72**，
  [安装包矩阵](issue-339-installed-integrated-matrix.json) **122/122**。
  矩阵含 120 个改名用例和 2 个能力用例，实际调用内置 PDFium/OCR，缺失 ASR 保留
  `componentUnavailable`。其中两份公开原件共 12 个变体保留基线解析错误；预期错误按断言验收，
  与成功转换分别记录。其他公开二进制原件验证路由、正文和资产等价。
- 安装包已包含 #338 和本项格式/HTML 修复，**早于 #341**；含 #341 的最终组合版本仅完成上述
  本地源码构建验收，**未重新构建并验证安装包**。材料校验先在较新源码上准确报告静态许可证摘要不匹配，
  随后在安装包对应的固定提交校验通过。没有发布 release，也没有替换用户已有安装。

历史证据保留固定来源：[早期 fast CI](https://github.com/coolplayagent/into-markdown/actions/runs/33396806735)
和[已完成的旧专项 CI](https://github.com/coolplayagent/into-markdown/actions/runs/33396806728)均四平台通过，
后者逐平台执行 72 个语料策略用例，其工作流已删除。
较早的[安装验收构建](https://github.com/coolplayagent/into-markdown/actions/runs/33396591314)
来源 `c521b861fb96939474dc6fac89d0f61782283985`，保留对应的
[72/72 语料](issue-339-installed-corpus.json)和[56/56 矩阵](issue-339-installed-matrix.json)，
不替代上述较新结果。

固定基线曾复现 `empty_source_and_empty_content_share_the_web_terminal_contract`、
`metadata_headroom_serializes_multiple_admission_failure_transitions_at_data_boundary` 和
`permanent_store_headroom_allows_terminal_mutation_at_real_data_boundary` 三项 CLI 失败。
`d1f91b5` 当时为 331 通过、3 失败、1 忽略；合入 #341 后串行重跑为 334 通过、0 失败、1 忽略。

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
already routed these samples to Text. The reported TXT conversion failure remains unreproduced. Automatic text detection retains full-input binary safety. Explicit format/charset authority and existing
parser checks remain in place, along with resource limits, cancellation and timeouts.

Machine-readable CLI and probe reports above identify actual routes, confidence, options,
diagnostics and output hashes. Local tests, historical four-platform CI, native-runtime tests and installed
artifact acceptance have separate statuses. The three historical CLI failures are resolved after integrating #341: 968 library tests and 334 CLI tests pass, with existing conditional skips preserved. Ten Bazel targets, 18 CLI exit contracts, 72 corpus cases and 120 binary cases pass on the combined local source build.

Following #349, the issue-specific workflow was removed and the existing four fast jobs remain
the only PR CI entrypoints. Completed historical runs remain evidence; the explicit installed-artifact
request authorizes the build-only official release workflow linked above.

The #338 RAR/ZIP changes are integrated from `d04d41e6ce5c1359e32eb5c3c81c9debf3d6567c`. Catalog status participates in admission; valid RAR keeps extraction guidance, while plain text with an unsupported `.rar` suffix requires an explicit supported format. Fully inspected nonempty generic ZIP archives override misleading document-package suffixes.

The unsigned macOS 0.0.4 package from `d1f91b5` passes source-bound material verification,
72 corpus cases and 122 binary/runtime cases. It includes #338 and the #339 runtime changes but
predates #341; a package of the final combined source has not been rebuilt and validated.
Two of nine public binary originals retain baseline parser errors across twelve renamed variants;
these are expected-error checks, not successful document conversions. Browser observations are
recorded separately for the package and the combined local source. No release was published and
no existing installation was replaced.
