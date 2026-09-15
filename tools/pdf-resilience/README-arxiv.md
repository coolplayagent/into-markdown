# arXiv PDF resilience acceptance

`arxiv-manifest.json` freezes 100 independently identified, versioned originals. The groups contain 25 surveys, 25 model reports, 20 figure/table papers, 20 mathematics/physics papers and 10 multilingual/document-image papers. A paper belongs to one primary group; multilingual papers can contain both native text and document images. IDs, actual titles, page counts, byte counts and SHA-256 bind every replay to the same input. Selection predates product edits. Conversion failures remain in the results.

Acquire these explicitly authorized public sources into a project-local diagnostic directory:

```sh
python3 tools/pdf-resilience/acquire_arxiv.py --allow-network \
  --manifest tools/pdf-resilience/arxiv-manifest.json \
  --destination target/arxiv-100/sources
```

Replay an immutable CLI against those bytes. Run the official 0.0.6 binary first and the candidate binary separately. Each replay executes default OCR and `--ocr off`, with `--no-config --error-policy best-effort`; no memory, layout-comparison, page or asset limits are increased. The external watchdog is recorded as a failure. The CLI's automatic memory ceiling uses machine capacity. Reports retain the observed available-memory snapshot. Delivery replays may run in independent local groups with the same options and executable; record concurrency and retain each raw report. For isolated performance comparisons, run sequentially after builds finish. Concurrent replay durations describe those runs and do not establish a controlled speedup.

```sh
python3 tools/pdf-resilience/arxiv.py \
  --manifest tools/pdf-resilience/arxiv-manifest.json \
  --sources target/arxiv-100/sources --into-md /absolute/path/to/into-md \
  --output target/arxiv-100/candidate-results
```

`--resume` requires the same manifest, executable, OCR modes, explicit configuration hashes and watchdog settings. Reports retain exact arguments, duration, CLI outcome/diagnostics, effective resource budgets, page evidence, broken resource references and output hashes. Missing/empty/repeated page containers fail coverage. Whole-document recovery covers missing page containers only when a document-level recovery diagnostic accompanies an attachment whose SHA-256 matches the original. Download errors are reported separately from conversion errors. Originals and full outputs stay outside Git in `target/arxiv-100`; commit the manifest, replay tools and compact acceptance evidence.

Page anchors and successful exit codes establish delivery coverage. They do not certify textual accuracy. Inspect source/output examples covering reading order, tables, formulas, footnotes and OCR anomalies. Release acceptance uses zero best-effort delivery failures, at least 95% OCR success on independently classified text-bearing images, and quality no weaker than official 0.0.6. Exhaustive recovery-page review is supplementary. Record structured output, native-text recovery, image recovery and original attachment separately. A recovery note alone does not establish source-content coverage; an original attachment must match the input SHA-256.

Recovery uses `Degraded` plus `pdf.recovery.<representation>` warnings with page and stage locators. Default best-effort keeps available page text, uses a published PNG for uncertain visual layout, and retains one hash-addressed original when a page cannot render. Internal OCR working images remain separate from published recovery images. Strict errors, cancellation, deadlines and output-write failures retain their own semantics. Explicit resource ceilings continue to bound work and payloads.

These expensive, opt-in network and native-runtime runs remain local. The four existing PR fast jobs are unchanged.

Build the page audit using independently extracted source text, then summarize both complete replays:

```sh
python3 tools/pdf-resilience/audit_arxiv.py \
  --manifest tools/pdf-resilience/arxiv-manifest.json \
  --report target/arxiv-100/candidate-results/report.json \
  --inventory target/arxiv-100/source-review \
  --output target/arxiv-100/candidate-audit.json
python3 tools/pdf-resilience/review_arxiv.py \
  --manifest tools/pdf-resilience/arxiv-manifest.json \
  --audit target/arxiv-100/candidate-audit.json \
  --results target/arxiv-100/candidate-results \
  --sources target/arxiv-100/sources \
  --inventory target/arxiv-100/source-review \
  --output target/arxiv-100/review
```

Retain available hash-bound source/output verdicts in `reviews.json`, then run source-content acceptance before producing the release summary:

```sh
python3 tools/pdf-resilience/quality_gate.py \
  --expectations tools/pdf-resilience/arxiv-content-expectations.json \
  --report target/arxiv-100/candidate-results/report.json \
  --reviews target/arxiv-100/reviews.json --review-policy source-content \
  --output target/arxiv-100/content-quality-gate.json
python3 tools/pdf-resilience/summarize_arxiv.py \
  --manifest tools/pdf-resilience/arxiv-manifest.json \
  --baseline target/arxiv-100/baseline-quiet/report.json \
  --candidate target/arxiv-100/candidate-results/report.json \
  --audit target/arxiv-100/candidate-audit.json \
  --quality-gate target/arxiv-100/content-quality-gate.json \
  --output target/arxiv-100/acceptance-summary.json
```

The text audit reports normalized 12-character ngram recall per page. It keeps the manual-review status separate from conversion acceptance; automated checks leave `manualReviewComplete` false. Maximum process RSS comes from the operating system, while retained-memory and worker budgets come from the conversion report. These measurements describe different resource scopes.

The offline review packet places each source page beside its delivered images and Markdown, with OCR mode, page number and source/executable hashes. Opening a page leaves its review status pending. Review records must distinguish overview inspection from full-resolution comparison and text verification. Identical image hashes may share an image inspection across OCR modes; text and diagnostic differences still require their own checks.

The existing layout-quality authority now binds the current fixture manifest and OCR-merge authority. All 12 pinned PDF payload hashes and semantic goldens remain unchanged; the native layout-quality test verifies these bindings before checking precision, recall and golden output.

Release content acceptance follows [the shared conversion-quality contract](../../docs/qa/conversion-quality.md). Execute `quality_gate.py` with separately reviewed `--expectations`, the immutable replay `--report`, a hash-bound `--reviews` ledger and an `--output` result. Content expectations and OCR minimums apply to each source; successful conversion alone cannot pass this gate. Recovery-page images enter OCR at the page boundary once. A source-page rendering remains a deliverable asset when OCR fails, with the failure recorded separately from completed recognition.


The local cross-format OCR contract embeds a repository-owned image whose expected sentence appears only in pixels. `ocr_contract_corpus.py` generates DOC/DOCX, PPT/PPTX, XLS/XLSX, ODT/ODS/ODP, EPUB, HTML, RTF, Notebook, MSG, ZIP and standalone image sources. Supply a local LibreOffice executable through `--office` to generate legacy Office fixtures. LibreOffice is a fixture-generation dependency. Replay the generated manifest with `arxiv.py` and its generated source directory, then pass the independent `expectations.json` to `quality_gate.py`. Auto mode requires the complete image sentence exactly once; off mode requires the image and balanced skip accounting.

User-provided real-world corpora stay in the project-local diagnostic directory. Copy external-drive archives before inventory, extraction or conversion, verify the local copy hash, and freeze every source identity. Keep source documents and their converted contents out of public PR evidence unless publication is authorized; aggregate metrics and hashes can document acceptance without publishing document contents.


`web_cli.py` launches the real local service and compares its artifacts with the CLI for the same source and both OCR modes. Generate `--request` from the frontend's exported `taskRequest(defaultWorkbenchOptions)` so this check covers the actual browser wire contract. The request must explicitly use best-effort and auto OCR with omitted limits. The report binds source and executable hashes and compares Markdown content, every delivered asset hash, and source/attempt/completed/failed/skipped OCR counters. Keep its private session log local. Optional `arxiv.py --config` files are hash-bound; this allows an isolated installed speech runtime to participate in user-corpus tests without inheriting unrelated settings.

The corpus report separates unresolved source hyperlinks from missing delivered images and generated asset references. Original-file recovery is recorded as degraded delivery and retains the source bytes; it does not imply successful text recognition. Legacy encodings use the supported encoding registry, and JPEG primary-image validation permits camera metadata after EOI while retaining the complete original asset.

`compare_ocr.py` compares the same frozen image inputs against official 0.0.6 and the candidate. Source inspection supplies text eligibility independently of OCR output; `--frames` binds an independently decoded frame inventory, including multi-page TIFF. Blank and no-text images remain in delivery results while leaving the text-recognition denominator. A text-bearing input must deliver recognized text for every expected frame. The report includes per-input word changes for quality review. `compact_report.py` exports hashes, status, timings, asset counts and OCR counters while omitting source filenames, paths, text and diagnostic messages. Its `ocrTotalsByMode` separates default recognition from intentionally disabled OCR; use the `auto` totals when assessing default skip coverage.

Unreadable encrypted sources retain their real CLI status. `classify_inputs.py` requires an independent source-encryption ledger with matching SHA-256, retains the original failed-delivery count, and reports unreadable inputs separately from conversion failures. A negative-fixture label or filename alone cannot establish this exception. The user corpus includes password-protected fixtures; keep their unavailable content visible in the acceptance report.

### 修复后的专项复测

`reconcile_reports.py --base BASE --replay REPLAY --manifest ORIGINAL_MANIFEST --output RESULT`
将分阶段运行关联到原始清单。每条记录携带实际可执行文件哈希及原始报告哈希，多个可执行文件参与时组合报告的顶层可执行文件哈希为空，同一可执行文件的分组运行保留共享哈希；所有原报告继续保留。部分基线必须提供原始清单，只有每篇、每种模式均有结果时组合报告才完整。专项复测产生的新失败会保留为失败。

邮件的 CID 图片在正文引用或附件展示两条路径都纳入 OCR。全透明图像通过独立像素检查归入无可见文字项，原始 OCR 失败计数仍保留；整页已执行 OCR 的 PDF 原始图像不重复识别。不同口径分别报告。

`masked_pdf_corpus.py --output <directory>` builds a PDF-specific OCR contract from the frozen English image golden (Pillow is required locally). The sentence lives in a soft mask over black pixels, alongside a native text paragraph. Run its `manifest.json` in auto/off modes and validate `expectations.json` with the shared quality gate. This checks both visible image composition and image OCR on a page with native text. The native PDFium regression also checks partial alpha, original pixel dimensions, repeated extraction and unchanged page rendering. PDF image extraction uses `FPDFImageObj_GetRenderedBitmap` at source resolution and restores the object matrix while holding the runtime gate.

Disjoint parallel runs use `reconcile_reports.py --partition FIRST --partition SECOND --manifest FULL_MANIFEST --output RESULT`. Their executable, OCR modes, watchdog and configuration must agree. The combined report binds the full source inventory and retains each partition's report hash; duplicate sources, changed source hashes and changed page counts are rejected, while missing cases remain incomplete.
