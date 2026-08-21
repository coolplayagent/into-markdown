# 跨格式语义布局质量门禁

`into-markdown-layout-quality` 是转换器和渲染器之外的独立、离线质量边界。各格式转换器
仍负责解析源文件并给出权威 Document IR；质量层只读取公共 IR、资源清单和最终 GFM，
不会修正文档、重新排序节点或调用模型/网络。这样失败属于转换质量回归，而不是渲染器
在输出阶段掩盖的修复结果。

## 门禁范围

质量投影按深度优先顺序保留稳定 node ID、全局顺序、同级顺序和父子关系，并统一记录：

- page、slide、sheet、cell、part 和源 byte range 边界；
- 量化到千分之一源单位的有限 bounds，以及 authority 明示的绝对容差；
- 标题、段落、列表、代码、公式、脚注、页面、幻灯片、工作表和时间片段层级；
- 表格逻辑网格中每个 origin cell 的行列位置、row/column span、header 和直接嵌套 block；
- 图片 asset、脚注和链接的正文引用，以及 node/inline/OCR detector→recognizer→merge 来源链；
- 不含资源 bytes 的稳定 asset inventory，包含媒体类型、文件名、外部 URI、长度、完整内容
  SHA-256 和引用状态。

附件没有被伪装成图片。若转换器以附件清单段落表达附件，质量层只在正文标签与一个且仅一个
asset filename 完全相等时建立 `attachment` 关联；同名歧义保持未绑定并失败，避免按前缀、
大小写或猜测路径误绑资源。

比较器按 node ID 定位并稳定报告 `missing`、`duplicate`、`unexpected`、`order`、
`content`、`hierarchy`、`boundary`、`geometry`、`tableTopology`、
`resourceAssociation`、`sourceChain`、`irGolden`、`gfmGolden` 和 `threshold`。
每条报告包含 fixture、page/slide/sheet、node 和顺序位置。正文 asset 引用缺失、脚注目标
缺失、重复 asset ID 和孤立 asset 都会失败；GFM 只比较渲染后的语义结果，不承载坐标或
来源信息。

现代文本和现代 Office/ODF/EPUB/MSG cohort 的 precision、recall 均不得低于 95%；PDF、
旧 Office 和图像 OCR 这类几何派生 cohort 均不得低于 90%。结构差异、IR/GFM hash 漂移
或低于阈值中的任一项都会使门禁失败。geometry tolerance 只能由 checked-in authority
按 fixture 声明；默认值为 0，反例测试固定证明 tolerance 内允许、超出一个千分之一即失败。

## Authority 与真实样本

`fixtures/semantic-layout-quality-authority.json` 绑定完整 `fixtures/manifest.json` SHA-256，
并为真实 DOCX/DOCM family、PresentationML、SpreadsheetML/XLSB、ODF、RTF、EPUB 和 MSG
转换结果保存语义投影、规范 IR SHA-256 与精确 GFM SHA-256。每次执行同一 fixture 两次，
先比较 IR、资源和 GFM 的确定性，再进入质量审计。

coverage 表同时列出每个核心格式 family 的 normal、complex、misordered、corrupt 和
resource-boundary 证据及其真实 Bazel gate。PDF 使用带固定 PDFium runtime 的
`//crates/converters:pdf_layout_quality`；图像 OCR 使用完整 detector→recognizer→公共 API 的
`//crates/api:ppocrv6_image_quality`；旧 Office 使用隔离 worker 的
`//crates/legacy-office:legacy_office_test`。这些目标与共享门禁由四平台矩阵一起执行，不把清单
中的 target 名称当作通过证据。矩阵同时运行 converter 与 PDF layout 单元门禁，固定覆盖
单栏/多栏、横纵排、浮动图片、页眉页脚、脚注及 Presentation shape 顺序。
`counterexample:reading-order` 不是格式转换样本，而是共享比较器的故意乱序
反例；它保证任何格式产生相同退化时都会被统一拒绝。

authority 只能通过显式 ignored generator 形成候选文件，生成不会自动改写仓库：

```shell
SEMANTIC_AUTHORITY_OUTPUT=/tmp/semantic-layout-quality-authority.json \
  cargo test -p into-markdown-layout-quality --test semantic_layout_quality \
  generate_review_candidate -- --ignored --nocapture
```

候选必须逐项审阅语义节点、来源、边界、表格和资源变化后，才能替换 checked-in authority；
不得仅因为测试失败而接受新 hash。

## 有界执行与平台一致性

审计在任何分配前估算 report working set，并通过同一个 `ExecutionContext` 预留内存。
遍历对 node、inline、OCR evidence、asset 及资源哈希分块执行 checkpoint，使用
`max_nesting_depth`、table row/column/cell 限制和 `semantic_layout_work` 工作量上限。取消、
deadline、算术溢出或任何资源限制都会返回 typed
`ConversionError`，不会返回部分报告；错误路径释放 reservation，成功报告持有 reservation
直至 Drop。IR hash 通过 streaming serializer 计算，不为完整 JSON 创建第二份缓冲。

本地完整命令为：

```shell
cargo test --locked -p into-markdown-layout-quality
bazel test --config=macos_arm64 \
  //crates/layout-quality:layout_quality_test \
  //crates/layout-quality:semantic_layout_quality \
  //crates/converters:converters_test \
  //crates/pdf-layout:pdf_layout_test \
  //crates/converters:pdf_layout_quality \
  //crates/api:ppocrv6_image_quality \
  //crates/legacy-office:legacy_office_test
```

`.github/workflows/semantic-layout-quality.yml` 在 macOS ARM64、Linux x86-64、Linux ARM64 和
Windows x86-64 的真实 runner 上分别通过 Cargo 和 Bazel 执行同一 authority；因此跨平台
规范 IR/GFM 任一 byte 漂移都会由 hash 门禁直接失败，而不是由单平台生成结果推断一致。
