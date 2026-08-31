# #334 PDF 链接与大型文档回归

## 样本与复现

2026-08-31 在 macOS ARM64，以干净基线 `3b328ad6407c8e12217346eb60c5d07217aa0566`
构建 `into-md 0.0.4`，使用仓库锁定的 PDFium 153.0.7999.0。

| 公开出版方样本 | 页数 / 字节 | SHA-256 | 基线结果 |
| --- | --- | --- | --- |
| [Accenture Humans, AI and Robots](https://www.accenture.com/content/dam/accenture/final/capabilities/strategy-and-consulting/strategy/document/Accenture-Humans-AI-Robots.pdf) | 29 / 1,188,879 | `4c8b1e634ccc08987b32027539db1772a17cc19f585c78c6d59ed7a0395ef423` | exit 3，`malformed input (link_rect): invalid or non-finite rectangle` |
| [OpenStax Calculus Volume 1](https://assets.openstax.org/oscms-prodcms/media/documents/CalculusVolume1-OP.pdf) | 873 / 41,375,116 | `202c86537285adf7e5abeb64057c39ee7333ad8c8473b6dd6a9ddf3e72443286` | exit 5，第 472 页 `pdfPageObjects: 100135 > 100000` |

Accenture 官网文件与报告中的 `Accenture_Humans_AI_Robots.pdf` 题名、大小和报错吻合。
官网使用连字符文件名；尚无报告者原文件哈希，不能认定字节完全相同。
该文件包含 1,872 个原始页面对象、45 个注释链接。第 9 页起出现有限倒序矩形，
例如 bottom=455.829、top=437.167；这能直接解释旧边界检查的拒绝。

OpenStax 全文有 188,095 个原始对象，单页最多 748 个，原生字符约 133 万。
`tools/pdf-resilience/samples.py` 使用 pypdf 6.10.0 生成前 600 页摘录，重映射保留页的
内部跳转，仅移除 21 个指向摘录之外的注释，保留 578 个注释。摘录含 129,507 个原始对象，单页最多 694 个。该摘录 SHA-256 为
`d87333f1640923b5c6430cede56909e091d2d3e97c48ae6cddc066fce5d41ec9`。
公开大文件与输出均保留在本地/CI 测试缓存，不进入 Git、安装包或证据 artifact。

## 验证范围

修复前先加入两项原生回归：有限倒序链接和 501 页 × 200 个原始对象的合成文档。
两项均在旧实现失败，分别得到 `link_rect` 和 `pdfPageObjects` 错误。

修复后的源码回归包括：

- 注释和网页链接的混合好坏矩形、倒序、非有限、零面积、越界、读取失败与非零页面原点。
- 规划/物化结果变化、URI 长度变化、诊断与结果容量、过量扫描、无进度、失效句柄。
- 请求取消、超时、低内存、单页/累计对象精确边界与累计溢出；失败释放请求内存租约。
- OCR off/auto/always 的转换器路径，以及现有逐页 OCR 消费、取消及资源释放回归。
  这些 OCR 测试验证路径和生命周期，公开大文件采用 OCR off。
- 配置读取、默认值、CLI 覆盖、请求序列化与 Web 上限；中文路径、批量输出、严格策略
  失败和低预算失败不覆盖已有输出，内部跳转与有效链接保留。
- 锁定 PDFium 的版面质量 golden：规范化 IR、Markdown 与语义精确率/召回率断言。

Accenture 的 best-effort 完整保留正文和有效链接；strict 在倒序矩形规范化后，
继续于第 28 页因 URI 含控制字符明确失败。该独立安全检查保留为回归的预期失败。

600 页真实摘录在对象默认预算、8 GiB 内存、`--asset-mode omit` 和显式
`--max-pdf-layout-comparisons 120000000` 下完整转换。版面比较默认仍为 12,000,000；
扩大对象预算后，该摘录实际触发了独立比较上限，因此增加显式比较预算接口。
全文继续以 `documentInlines: 1000329 > 1000000` 失败，保留最终 IR 限制。

## 复跑与证据分层

```sh
cargo build --locked -p into-markdown-cli
python3 -m pip install pypdf==6.10.0
python3 tools/pdf-resilience/run.py --into-md target/debug/into-md \
  --pdfium-library /absolute/path/to/pinned/pdfium \
  --work-root target/issue-334-repro/blackbox --public-samples
```

`report.json` 记录执行平台、版本、二进制哈希、样本 URL/哈希、命令、退出码、页数和
Markdown 内容哈希；每项失败日志单独保存。安装产物省略 `--pdfium-library`，使用自身
打包的运行时。测试不会自动安装软件或变更用户配置。

- 本地源码构建：Accenture 29 页成功，71,237 字节 Markdown，并输出图片资产；
  600 页摘录成功，976,524 字节 Markdown。每页 anchor、正文、链接与资产有内容断言。
- PDFium 原生边界、转换器及 API、配置/Web 和发布工具测试有独立日志。
- `.github/workflows/pdf-resilience.yml` 在四个平台运行原生与公开样本回归，包含
  Windows 中文路径，上传 JSON/日志；执行结果需以具体 CI run 为准。
- 发布流程的 `native_acceptance.py` 对四平台实际压缩包解出的可执行文件调用同一离线
  合成回归，另写 `pdf-regression.json`；`build_only=true` 验证构建不发布版本。
  安装产物通过状态需以具体构建 run 的证据为准。

本地 Bazel quality 命令遇到基线已有的 rules_nodejs 扩展锁文件不一致；未改写锁文件。
相同的显式质量测试通过 Cargo 编译，使用相同 fixture、manifest、PDFium 和 runfiles
定位执行，通过全部 golden 断言。另有一项既有 pdf-layout 单测
`outside_page_geometry_fails_without_a_publishable_document_or_lease` 在干净基线及本分支均失败；
该测试预期拒绝部分越界原生文字，而现有实现保留该块。此补丁不改变那条文字几何路径。
