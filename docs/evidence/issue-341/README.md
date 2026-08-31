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

[实际工作台截图](web-product.png) 和 [结果 DOM](web-product-dom.txt) 来自本地候选 CLI：
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
  由新增测试一起核对。Bazel 锁记录按当前主线的依赖输入重新生成，Cargo 依赖未修改。
- Web 全量回归中的既有 altChunk 测试与当前主线的可见省略提示同步；基线和候选 CLI 均验证
  相同提示及诊断。配额测试在创建填充文件后测量目录占用，覆盖文件系统目录增长。

## 原报告证据边界

#331 的 1994 个图片引用、235 个空备注标题及原消费端环境尚未取得原文复现。
本次公共样本、固定边界 fixture 和浏览器结果分别记录，不据此声称原批次已被复现。
不发布 Release，不替换本机安装；交付后保留证据并删除本任务的独立 worktree。
