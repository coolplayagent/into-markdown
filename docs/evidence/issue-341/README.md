# #341 验收证据

本目录保存可复跑的来源清单与验收证据。公开原文及完整转换产物保存在本地验收目录，
不加入仓库；来源网站的公开可访问性不视为再分发授权。

## 验收范围

- 样式：固定 pulldown-cmark 0.13.4，455 组空白、标点、Unicode、相邻和组合标记检查，
  加上链接、代码、表格、硬换行及自定义列表重启的语义回归。
- 来源标记：正文移除 source-marker，IR 保留 marker_label、任务状态与来源；空结构分隔符
  在 Markdown 再次导入时保留独立列表结构，不产生代码块。
- 资产：CLI 84 个路径/路由/资产模式组合；按消费端解析的链接读取文件或 bundle 成员，
  比较原始图片字节摘要。Windows 盘符/UNC/设备路径的规划及拒绝边界由 CLI 单元测试覆盖。
- 备注：PPTX/ODP 空占位符、空白、文字、图片、alt、混合与表格；PPT 文字与表格；
  三级生成标题的身份传递；真实 API 中有文字/无文字 OCR、extract/embed/omit 六组合。
  传统 PPT 的图片关联仍遵循既有解析器边界，完整 notes shape 几何恢复不在本项范围内。

## 浏览器证据

[Web 样式截图](web-preview.png)、[DOM](web-preview-dom.txt) 与
[网络和版本记录](web-browser.json) 验证了受限组件的样式、转义、嵌套、换行、表格，
以及图片/外链/脚本字符串不产生资源 DOM。记录到的消费端 UA 为 Chromium 151。

[实际工作台截图](web-product.png)、[结果 DOM](web-product-dom.txt) 和 [构建记录](web-product-version.json) 来自本地候选 CLI：
上传仓库自有 PPTX，关闭 OCR，完成转换后打开阅读预览，检查多语言斜体与三级备注。
[图片加载记录](browser-images.json) 验证生成的特殊字符目录图片链接可在浏览器中实际加载。
[真实 PPTX 图片记录](public-browser-images.json) 另核对公开课件中的六张图片完整加载。

## 公开语料与复跑

[公开语料清单](public-corpus.json) 固定 DOCX、XLSX、PPTX、PPT、ODP 各 11 份，
按 SHA-256 去重。清单同时记录列表页访问、下载失败与重复内容；自产 fixture 单独统计。
每份成功获取的文件绑定 URL、最终 URL、UTC 获取时间、大小、摘要和授权说明。

```sh
python3 tools/markdown-quality/replay.py --manifest docs/evidence/issue-341/public-corpus.json --output "$QA_ROOT/corpus"
cargo build --locked --release -p into-markdown-cli
cargo build --locked -p into-markdown-render-markdown --example inspect
python3 tools/markdown-quality/pair.py --baseline-cli "$BASELINE_CLI" --candidate-cli "$CANDIDATE_CLI" --probe target/debug/examples/inspect --corpus "$QA_ROOT/corpus" --output "$QA_ROOT/paired" --baseline-revision "$BASELINE_SHA" --candidate-revision "$CANDIDATE_SHA"
python3 tools/markdown-quality/compare.py --baseline "$QA_ROOT/paired/baseline.json" --candidate "$QA_ROOT/paired/candidate.json" --output "$QA_ROOT/comparison.json"
python3 tools/markdown-quality/path_matrix.py --cli "$CANDIDATE_CLI" --probe target/debug/examples/inspect --fixture fixtures/small/odt/image-exact.odt --output "$QA_ROOT/paths"
```

基线与候选使用相同转换参数：OCR off、best-effort、2 GiB 内存、result-json、extract，
资源目录包含中文、空格、括号、`# % &`。正文、源编号、表格等实质 IR 与资产清单对照；
生成备注标签、节点 ID、来源元数据及空占位段落作为声明的规范化项，另由专项回归核对。
可见正文额外经过独立 GFM 消费端比较，忽略空白和生成备注标签。

初次转换脚本使用 120 秒外层超时。大型 XLSX 在本机并行构建负载下出现超时；
最终 `pair.py` 对全部 55 份文档按基线/候选配对复跑，保持 CLI 参数相同、禁用用户配置，
将外层等待扩为 600 秒。初次尝试与失败单独保留。运行时长受本机并行任务影响，不作为性能基准。

比较器默认拒绝所有可见正文变化。已逐项审核的预期变化通过 `--reviewed-deltas` 传入，
每条记录同时绑定源文件、基线可见正文和候选可见正文的 SHA-256；过期记录直接失败。
该审核只解释旧输出中误显示的转义符、HTML 标签与实体，实质 IR、资产和链接检查仍独立执行。

## Golden 差异审核

- DOCX/Markdown/ODT/RTF/CSV/TSV/MSG 的预期输出改用原生标记，空白移到分隔符外；
  fixture 原文件未变，按逐项实际转换更新语义摘要及生成器中的对应字符串。
- PPTX 各扩展名：斜体原生化，备注先转换有效正文再生成标题，节点 ID 因序号与身份后缀变化；
  来源 locator 和正文内容保持完整，z-order/language 元数据同步改绑新节点 ID。
- ODP：原粗体备注段落升级为三级生成标题，其正文身份单独标记。
- 质量权威同时同步既有 workbook table 节点编号及单元格起点 locator；相关生产转换器未修改，
  行列、正文、资产和 GFM 保持一致。PDF/OCR 清单只更新 fixture manifest 绑定摘要。
- 安装 smoke 的 Markdown 摘要与假执行器输出共同更新。Web 包含字节、MIME、路径与清单摘要
  由新增测试一起核对。Bazel 锁记录按当前主线的依赖输入重新生成，本项未新增 Cargo 依赖，保留主线 #338 的依赖变更。
- Web 全量回归中的既有 altChunk 测试与当前主线的可见省略提示同步；基线和候选 CLI 均验证
  相同提示及诊断。配额测试在创建填充文件后测量目录占用，覆盖文件系统目录增长。

## 固定版本与结果

转换基线为 `d276d8a`，其 CLI SHA-256 为
`6fe592bb56312d7e82773afd362f02e3f94e805e2ce424d8be761342f4f2760f`。
最终候选为合入 #334/#330/#338 与 CI 政策 #349 后的 `f63520b`，其 CLI SHA-256 为
`c3585249086e674731e976d526e52a8ecf9744cae0f496e2426aabb1daaaa98e`。
后续提交只补充测试和证据。编译器 Rust 1.97.1；本机 macOS ARM64，16 GiB 内存。

[全部转换尝试](corpus-attempts.json) 包含初次 120 秒限时、全部配对复跑和同步 #338 后
全部 55 份候选复跑；[最终比较](corpus-comparison.json) 保留每份结果，
[逐项可见文本审核](reviewed-visible-deltas.json) 对 14 份预期变化绑定精确摘要。
基线和最终候选均为 42 份成功、13 份转换失败，没有 QA 超时；成功样本实质 IR、243 份资产负载
和 275 个渲染后图片引用的字节摘要一致，图片定位失败为零。

| 格式 | 已测试 | 成功 | 既有失败边界 |
| --- | ---: | ---: | --- |
| DOCX | 11 | 8 | 三份公开源包引用缺失的 `word/media/image1.png` |
| XLSX | 11 | 10 | 一份表格超出本次固定 2 GiB 内存预算 |
| PPTX | 11 | 11 | 无 |
| PPT | 11 | 11 | 无 |
| ODP | 11 | 2 | 九份涉及未支持 WMF 或当前 ODF XML/投影约束 |

公开样本中的 source-marker 注释从 1006 到 0；Speaker notes 字符串出现次数从 293 到 5。
原始 Markdown 中 `<strong>` 从 2596 到 39，`<em>` 从 1036 到 3；剩余 HTML 用于实际需要的
表格或边界。最终 [84 项路径矩阵](path-matrix.json) 全部通过，包括实际解码读取和字节核验。
六张公开 PPTX 图片在最终候选生成结果中均完整加载；受限 Web 预览维持惰性资源边界。

CI 按 #349 仅保留四项 PR fast job；`f63520b` 的
[四项 fast gate](https://github.com/coolplayagent/into-markdown/actions/runs/33400209276) 全部通过。
此前启动的语义专项和 build-only 发布验收按新要求停止，保留其失败/取消记录；没有新增 CI 入口。
四平台 fast checks 与本地完整转换、运行时和安装产物验收分别记录。

[分层验证记录](validation-summary.json) 固定执行范围、日志摘要、首次失败及最终复跑结果。
本地 Cargo 覆盖 API、CLI、转换器、核心、渲染器、布局、PDF、license authority 和安装 smoke。
渲染器最终 39 项、API 66 项与采集一致性 7 项、CLI 335 项及空输出/退出码/插件集成 26 项、
转换器 646 项、核心 118 项、布局 13 项、PDF 43 项、license 95 项及安装 smoke 26 项通过。
严格 Clippy 使用仓库原有 `--all-targets --no-deps -- -D warnings` 范围，核心、渲染器、转换器和
布局均通过；额外包含依赖的尝试发现主线 OCR 中两项既有 lint，CLI 也保留主线已有 dead-code 警告。
额外对 installed-smoke 全 crate 的严格 lint 遇到五项 macOS 条件编译/借用告警，位于未改动的
process.rs、rust_consumer.rs；该范围保留未通过，安装实际运行和 26 项测试单独通过。
Web 六个测试分组及 28 项管理界面测试、类型检查和四个内嵌资源摘要验证通过。
91 项发布/CI/portable Python 契约通过；完整 fixture 字节复现和 legacy fixture 检查通过。

Bazel `--config=macos_arm64 --lockfile_mode=error --distdir=third_party/runtime-assets/models`
下的转换器、语义布局、PDF 布局及 PDF 质量五个目标通过。完整 OCR API 质量目标两次失败：
繁体字错误数为 7，固定上限为 6。该检查读取 OCR 的 IR 文本；本项没有改动图像 OCR、模型、
识别运行时或该阈值。此项保留为未通过，缺少同环境主线基线运行证据。

完整 Cargo 执行保留首次失败：并发时安装 smoke 两项进程时序测试、毫秒级采集/聚合耗时比较，
以及旧 CSV/TSV HTML 预期和缺少 process-plugin fixture 的集成调用。时序测试单线程复跑、CSV/TSV
原生粗体预期更新与 fixture 配置分别验证，原断言和阈值保持原有强度。

Golden 复核同时纳入 Drawio 表头的原生粗体。完整 fixture 生成器补齐已有音视频样本，
恢复与清单一致的格式集合；原始 fixture 字节均未改变。[发布声明审核](release-material-review.json)
对当前主线的 12 个产品/平台配置逐一核对：仅 fixture 清单摘要发生变化，组件与许可内容保持一致。

## 本地安装产物

[产物汇总](installation-summary.json)、[ZIP 审核](installation-native-audit.json)、
[原生端到端](installation-e2e.json)、[PDF 验收](installation-pdf-regression.json)、
[13 份跨格式 golden](installation-cross-format.json) 和 [84 项产物路径矩阵](installation-path-matrix.json)
均通过。使用现有 portable-release 脚本构建 macOS ARM64 0.0.4，完整校验 Core 与语音包、
许可声明及 ad-hoc 签名；压缩包内程序通过 9 项原生用例与 6 项 PDF 检查。

Core ZIP SHA-256：`c1a39b228f8f252121a7baaaf27a5f76f64c538ad630e873bf9abc20daffb9d0`；
包内 CLI：`9585babf8745080eecf498df8d056f11494fb23a871195e95970b4b984a174f9`。
二进制仍来自 `f63520b` 的生产代码，之后同步安装 smoke 的 Drawio 原生表头摘要、测试及证据。
验收隔离用户数据，路径矩阵使用既有 `INTO_MARKDOWN_USER_DATA_HOME`，没有替换本机安装。
其他三平台安装产物未完成；按更新后的 CI 要求停止专项工作流并记录此边界。

首次产物验收发现 Drawio 安装 smoke 仍绑定旧 HTML 表头，按已审核的 fixture 摘要同步 Python
与 Rust 安装检查及测试执行器后通过。路径补充调用曾误用仅 UI 支持的 `--data-dir`，84 项均
被参数解析拒绝，改用已有隔离环境变量后全部通过；两次记录都保留。

## 原报告证据边界

#331 的 1994 个图片引用、235 个空备注标题及原消费端环境尚未取得原文复现。
本次公共样本、固定边界 fixture 和浏览器结果分别记录，不据此声称原批次已被复现。
不发布 Release，不替换本机安装；交付后保留证据并删除本任务的独立 worktree。
