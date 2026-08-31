# #340 OCR 预算与可选识别验收

独立 worktree：`into-markdown-issue-340`；分支：`codex/issue-340-ocr-memory`；
[PR #347](https://github.com/coolplayagent/into-markdown/pull/347) 交付本项独立修复。
原报告的 15 个 PPTX 尚未取得，以下公开样本证据单独表述。

生产实现基线为 `d276d8a`。当前集成主线为 `c2c7aec`，包含 #334、#330、#338、#341、
#339 和 #349；业务验证源码为 `a75678b86096f0d668ff10ec9baa40bbc9d98317`。
旧版到当前结果同时包含这些已合并功能的变化，差异按具体路径归因。
仅保留现有四个 PR fast job。用户已撤销四平台安装验收门槛；不再追加相关执行。此前授权的已完成流程使用
`build_only=true`、`signing_mode=unsigned`，无正式版本发布。

## 行为边界

- CLI auto 每次调用探测一次；整批共享 `min(总量/2, 可用量-max(1GiB, 总量/8))`。
  已知容量约束探测不完整时的保守默认；可用量不足明确拒绝准入。显式值原样保留、
  CLI 覆盖配置，公共 API 默认 2GiB 和 Web 安全上限保持不变。
- provider 请求额度受用户值、签名 capability 上限、已取得工作租约共同约束。
  宿主协议缓冲独立计费；provider 与模型子进程共同受进程组/Job 额度约束。
- 仅有效 auto + best-effort 且图片所在内容单元已有原生正文时，允许
  `ocrRecognitionMemory` 逐图片遗漏。内容身份缓存成功和遗漏，每处引用获得定位诊断。
  原生正文、原始资产和其他成功图片的 OCR 保留，缓存限当前转换。
- strict、always、必要正文、旧版通用 resourceLimit、结构/帧/协议/进程异常、取消、
  超时、全局预检及工作租约不足继续失败；没有增加自动重试。
- #333 原生 PDF 页的运行前预检恢复保持独立：必须有本页原生正文，扫描页不可借
  页码/页脚或其他页面文字放宽。PDFium 图片和 PDF 布局正文的来源身份已对齐。
- 共享 DTO 兼容地新增预算快照与 OCR 执行统计。Web 在对应文件区域显示遗漏和失败。
  RTF/IPYNB 服务装配已补齐，ZIP 保留主线装配，旧 Office auto 路由保持不变。

## 来源与文件级审阅

原 `corpus.json` 冻结 17 类各 11 个、共 187 个不同 SHA-256 的网络文件。
签名检查发现 `EDB-14503-1.html` 为二进制回归输入；保留原清单与所有结果，
`corpus-content-review.json` 明确剔除它，以补充清单的真实 `testHTML.html` 替换。
有效格式集合仍为 17 × 11。修正依据内容签名，替换文件已有独立前后测量。

补充清单另列 76 个不同文件，全部清单共 263 个不同 SHA-256。
来源区分公开出版物与上游回归样本，包括 Apache POI/Tika、LibreOffice、Jupyter、
W3C EPUB 和 Microsoft MarkItDown。清单固定 URL、提交或出版来源、SHA-256、尺寸；
`file-review.json` 逐文件关联内容类型、仓库声明哈希、内嵌声明和审阅缺口。

仓库根许可不能自动覆盖测试文档里的字体、图片及第三方材料。无法从逐文件证据确认
授权范围的字段明确保留 `NOASSERTION`；加密或不可读取成员单列缺口。
W3C 文档声明与内嵌字体分开记录；Gutenberg 保留各书内嵌许可及其地域条件。
当前没有把这组文件记为全部权利已核清。源文件、完整转换正文只用于本地验证，
仓库提交来源、哈希、结果和有限诊断，不重新分发测试文档。
自组装混合 HTML 和仓库自有 OCR fixture 均不计入网络文件数量。

```sh
python3 tools/ocr_memory/corpus.py fetch --manifest docs/qa/evidence/ocr-memory/corpus.json --root target/ocr340
# 每个补充清单用同一 fetch 命令恢复；按 corpus-content-review.json 生成 final-corpus.json。
python3 tools/ocr_memory/measure.py --binary target/ocr340/final-local-package/installed/into-md --manifest target/ocr340/final-corpus.json --root target/ocr340 --output target/ocr340/candidate-final-a756 --modes off,auto,16gib --groupings single,jobs1,jobs4
```

测量器需要 psutil，本地使用 `target/ocr340/venv/bin/python`。每次保存完整命令、binary
SHA、退出码、stdout/stderr、报告和结果。恢复执行校验命令、样本与二进制身份。
`observations.py` 从 NativeParser/LocalOcr 来源提取正文、资产及 OCR 指纹；兼容 Core
和公共 DTO 两种文本节点表示。正文视图与节点视图可能重叠，合并字符总量以报告为准。
`summarize.py` 拒绝重复或非标准实验目录，`index.py` 索引结果、失败文件及原收据哈希。

## 构建与测量分层

| 身份 | 用途 | 证据 |
| --- | --- | --- |
| 已发布 0.0.4 / `a66287d` | 修复前 macOS 安装包 | CLI SHA `4a435e8694a84d82b13d554e1c1f92232005901f1f2a3054212f2a94620eafde` |
| `de40de19` 本地优化构建 | 中间候选对照，保留内存紧张现象 | CLI SHA `4199c1fef0ff57477c2063d1a39bee437fa2dc80c439bc80a8e12c27ca7d0645` |
| `a75678b` 本地优化构建 | 当前安装包矩阵 | CLI SHA `81127f7ecf1718e7f2aed9bcf2604db0a1e49108b39183da9b0e1c0e70c00b14` |
| `a75678b` 本地安装压缩包 | 真实 OCR、归档材料审核 | ZIP SHA `d447ca13c7bd9c57c551c9226befcc55a2d4b28455679910e63d2c1d63404a8f` |

已发布版本的 apps/crates、模型、third_party 与 Cargo 清单和 `d276d8a` 比较无生产源码差异。
本地优化构建使用 Cargo release、已认证 provider 文件及 release archive 材料验证，
没有采用 CI 的完整编译/签名配方，因此单独标记。构建记录在 `final-local-package/build-inputs.json`。
本地首次打包因旧 release-projection 二进制携带过期许可 authority 而失败；重建该工具后通过，
没有放宽材料检查。旧失败日志和成功重试日志分别保留。

| 量 | 含义和限制 |
| --- | --- |
| sharedLeaseBudget/PeakBytes | 应用协作式共享租约预算和高水位 |
| processTreeRssSamplePeakBytes | 目标 50ms 间隔，同一采样中进程树 RSS 总量；可能漏过瞬时峰值 |
| operatingSystemPeakBytes | 标明 wait4.ru_maxrss 或根进程 peak_wset；独立记录，不能当作并发树 RSS |
| memory 快照 | CLI sysinfo 的总量、可用量、余量及实际选择 |
| hostTotal/AvailableBytes | 测量器 psutil 快照，平台可用量定义可与 sysinfo 不同 |
| worker/model 额度 | DTO 请求额度和观察到的模型 worker 物理/地址限额；旧收据缺少时不回填推测值 |

首次旧包矩阵的批量命令使用整批 120 秒截止时间，产生等待准入超时；该实验保留。
`baseline-full-batches` 按每个文件 120 秒扩展总截止时间，已完成全部模式 jobs 1/4。
补充矩阵的一次采样中断及冲突重跑移至 `interrupted-experiments`，避免重复计数。
旧收据保留原样，投影修复后重新生成分析并记录脚本哈希。

## 已验证结果

- 旧版公开 11 个 PPTX 的单文件 auto/16GiB 状态、正文、资产和识别投影一致；
  未复现原报告 15 个 PPTX 的现象。Tika testOCR.docx/pptx 另观测到各 20 字识别贡献。
- 16GiB 机器在开发负载下，sysinfo 可用量曾低于 2GiB 系统余量，新 auto 在准入前拒绝。
  显式 16GiB 保持原值，provider 不超过签名 768MiB；jobs 不复制批量额度。
- 当前本地安装包 always + strict + 2GiB 的真实 provider 测试通过：1 个区域、67 个字符、
  1 次请求、0 次识别拒绝，worker 768MiB。完整收据保留在 final-local-package。
- `real-provider-probe.json` 在真实 provider 568MiB 请求额度下验证混合文档：
  2 个独立图片身份、1 次识别拒绝、同图 2 处定位遗漏、另一图保留 67 字识别；正文与
  资产和 OCR off 完全相同。strict/always/必要正文均明确失败；全部工作与临时租约归零。
- 真沙箱测试覆盖 provider 和子进程各 288MiB、共同超过 512MiB 后失败，释放资源后
  后续请求成功。协议、取消、超时和事务测试保持终止及清理契约。
- 当前 `a75678b`：CLI 339 通过/1 忽略，API 库 79 通过/3 忽略、collecting parity 7 通过，
  Engine 82 通过，converters 662 通过/16 忽略。此前 CLI 三项基线失败已在最终集成消失。
- 同生产变更集的 Core 120、OCR 131/1 忽略、ONNX 15、process-plugin 11、provider-plugin 12
  库测试通过，真实 process runtime 10 项通过。Web 10 个 Bazel 测试目标通过，包含类型、
  单测、内嵌资源及确定性检查；Rust 索引与 npm SPDX 同步。
- release/license 三个 Bazel 目标通过，现有四 job 白名单 14 项测试通过。
  [a75678b 四个 fast checks 全部通过](https://github.com/coolplayagent/into-markdown/actions/runs/33410086118)。
- Web 浏览器验证使用真实前端和本地受控 API：遗漏在对应正文旁、页码/资产可定位；
  必要识别失败在文件区域和历史记录显示。DOM、截图及哈希保存在 ui-qa；该项不代表生产后端联调。

## 已完成安装 CI 与保留的证据边界

[7876a83 安装验收](https://github.com/coolplayagent/into-markdown/actions/runs/33402389312)：
四平台 build 和 Windows 黑盒通过，Linux 汇总的 OCR hot-cache-reuse 因通用 provider
resourceLimit 失败。旧记录缺少具体触发层级，不能将根因直接认定为真实进程内存超额。
Linux 当前改用实时 PSS/SwapPss 和 huge-page 计数，并将观测故障分类为进程异常。
[19073df 安装验收](https://github.com/coolplayagent/into-markdown/actions/runs/33408650194)
在用户撤销该门槛前已经完成并全部成功，包含 Linux 热缓存 OCR 和 Windows 黑盒。
它不含 a75678b 的 PDF/预检修复；不据此宣称最终提交四平台已验收。追加产物下载已停止。

当前有效 OCR 格式覆盖仍未全部达到计划。已采集 DOC/XLS/ODS/RTF/HTML/ZIP 的样本
尚缺成功输出中的有效识别贡献；某些样本没有可提取的栅格文字，某些遭遇已有解析限制。
例如 XLS 的常见 BSE 内嵌 BLIP/跨 BIFF 段图片在既有解析路径没有提取出来，0 次请求
不能计为 OCR 成功；droste.zip 虽识别到文字，最终触及递归深度限制，仍计文件失败。
个别 ODT/PPT/MSG 只有 1 字识别，需核对真实性；PPTX/XLSX/MSG 的 strict 解析失败保留。

- [x] 独立 worktree 与主线 CI 边界。
- [x] 预算、分类、逐图片恢复、缓存/释放和跨入口契约。
- [x] #333 PDF 预检、坐标、布局和其他嵌入视觉回归。
- [x] 当前本地安装包及真实混合 provider 验证。
- [x] 逐文件来源、内容类型、声明与输出对照记录；授权不明项显式保留 NOASSERTION。
- [x] 当前最终安装包的 187 文件全部模式/入口矩阵完成并对照旧包。
- [x] 17 类代表样本完成前后 always/strict/always-strict 对照；有效 OCR 覆盖缺口按上述记录保留。
- [x] 四平台安装验收门槛由用户撤销；保留已完成证据，不再补跑。
- [ ] 整体审核、合并、回填 #340/#331；合并后保存原始证据并删除 worktree。

17 类策略对照见 `policy-comparison.json`：旧版 always/strict/always-strict 分别 14/10/8 个成功，
当前为 16/10/10，严格解析失败数保持 7。成功总数只表示转换状态；有效 OCR 内容单独核对。
最终源码本地命令、日志摘要、二进制/包身份及 Web 观察见 `validation.json`，
完整原始证据在合入后保留于 `/Users/yx/projects/document-convert/into-markdown/target/issue340-evidence`。

最终主集合前后各 1,683 个文件结果见 `corpus-comparison.json`，所有失败和指纹差异保留。
当前显式 16GiB 单文件/jobs1/jobs4 均为 135 成功、52 失败；整批预算 16GiB，
请求 worker 上限始终 768MiB，jobs1/jobs4 的进程树 RSS 采样峰值分别约 1.070/1.067GiB。
新监控明确拒绝 ClippedImages.pdf 和 ConditionalFormattingSamples.xlsx 的运行期全局进程组
超额（约 810/811MB 对 805,306,368 字节额度），不作为可选识别内存拒绝吞掉。
auto 的快照随主机压力变化：本轮 jobs1 约 1.40GiB、jobs4 约 0.96GiB，成功数 133/114；
这两次是独立调用的不同快照，不能把差异解释为 jobs 倍增额度。
文件级正文/资产比较跨越多个已合入工作项；#340 的保留契约另以同源混合样本与 off 精确对照验证。
