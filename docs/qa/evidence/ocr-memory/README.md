# #340 OCR 预算与可选识别验收

实施基线为 `d276d8a`，独立分支 `codex/issue-340-ocr-memory`。原报告的 15 个 PPTX
尚未取得，公开网络样本的结果单独记录。当前文件是验收工作记录；勾选项与最终证据
必须以所列构建和命令为准。

集成已同步到 `d04d41e`，包含 #334、#330、#338 与 #349。#340 保留主线的 PDF、
PPTX、归档和 RAR 契约；CI 仅使用现有四个 fast job。本地更新资源时由 Bazel 重算
扩展锁文件，并保持 Cargo 依赖清单不变。安装产物验收按用户明确要求进行，不发布版本。

## 样本和复现

`corpus.json` 冻结 17 类各 11 个、共 187 个 SHA-256 不同的网络文件。来源分类明确
区分上游回归文件与公开出版物。源文件和转换输出保存在本地测试目录，仓库保留
URL、版本、哈希和测量记录。仓库级许可及文件级声明仍需逐项核对，不能由扩展名
或仓库托管状态推定权利。

```sh
python tools/ocr_memory/corpus.py fetch --manifest docs/qa/evidence/ocr-memory/corpus.json --root target/ocr340
python tools/ocr_memory/measure.py --binary target/ocr340/baseline-embedded-into-md --manifest docs/qa/evidence/ocr-memory/corpus.json --root target/ocr340 --output target/ocr340/baseline-corpus
```

测量器默认执行 off、auto、auto + 16GiB，覆盖单文件、jobs 1、jobs 4。测试目录和
binary SHA 必须独立，恢复执行时校验 binary 与样本集合。每次保留完整命令、退出码、
stdout、stderr、批量报告以及进程树 RSS 采样。正文与资产指纹由 `observations.py`
独立从结果 DTO 提取，避免将源文档正文复制进验收汇总。

首轮安装包矩阵已生成 1,683 个文件结果（187 × 3 模式 × 3 入口）。当时批量命令
使用整批 120 秒截止时间，很多文件在等待准入时超时。因此保留该组结果，并在
`baseline-full-batches` 重跑按文件数扩展截止时间的批量实验。此修正仅影响测量命令。
恢复执行同时校验完整命令，防止不同配置复用旧记录。

`summarize.py` 从原始结果重新分析 `localOcr` 来源；早期测量器误用 `ocr` 标签而
漏计 OCR 块，原收据保持原样。修正后的分析单独记录投影脚本和收据哈希。
`recognition-supplement.json` 和 `recognition-followup.json` 另计 25 和 22 个网络文件。
原清单中的 `EDB-14503-1.html` 经签名检查为二进制回归样本，不计作有效 HTML；
补充清单的 `testHTML.html` 提供第 11 个实际 HTML 文件。

`sharedLeasePeakBytes` 是应用租约高水位。`processTreeRssSamplePeakBytes` 是按
50 ms 目标间隔观测的同一时刻进程树 RSS 总量；受调度影响不保证捕获瞬时峰值。
`operatingSystemPeakBytes` 标明 wait4.ru_maxrss 或根进程 peak_wset 来源，与进程树
RSS 总量分开解释。测量器完善前已完成的旧版记录在此字段使用 null，表示未采集。
CLI 的 sysinfo 可用量与测量器的 psutil 可用量采用各自平台定义，两者均保留。
较新的测量记录还采集模型 worker 命令行中的物理与地址空间限额；不会采集其他进程
或完整命令行。旧记录缺少该字段时应注明未采集。

## 当前本地观察

- 旧版公开 PPTX 的 11 个样本已运行对照，尚未复现原报告 15 个 PPTX 的现象。
  单文件 auto/16GiB 的正文、资产、识别投影与状态一致，不能据此声称这些文件都有
  有效 OCR 贡献。另以 Tika `testOCR.pptx` 和 `testOCR.docx` 验证到各 20 字识别贡献。
- 16 GiB 机器在并行开发负载下，sysinfo 可用量曾低于 2 GiB 系统余量；新 auto
  拒绝准入。显式 16GiB 保持原值，provider 请求额度保持认证上限 768 MiB。
- 本地开发构建的真实 provider 已成功识别清晰图片，贡献 1 个区域、67 个字符；
  同时观测到 1 个 provider 和 2 个模型 worker。启用进程组限额后仍成功。
  此项为本地开发构建证据，安装压缩包与四平台证据仍待补齐。
- 一次 150 秒开发构建实验超时；栈采样落在运行包 SHA-256 验证路径。加长观测窗口
  后同一清晰图片在约 133 秒完成，未增加产品重试策略。实验遗留进程组已按身份核对清理。
- 真实沙箱测试中，provider 与其子进程分别持有 288 MiB，在合计 512 MiB 的限额下
  触发全局资源失败；请求内存与临时租约归零，随后同一上下文的请求成功。
- Web 的六个分组共 39 项单元测试及类型检查通过，内嵌资源由
  `bazel run //web/console:update_assets` 生成，并同步 Rust 索引和 npm SPDX 哈希。
- 同步 `d04d41e` 后，转换器 650、Core 120、Engine 80、process-plugin 9、
  provider-plugin 12 项库测试通过（转换器 16 项忽略），Web 类型检查和六组测试通过。
  API collecting parity 的 7 项测试在此前的 `0ae0ddf` 集成上通过；最终提交需重跑。
- 真实 RTF 的旧版 always 返回 `componentUnavailable`，追踪到 CLI 未为 RTF、IPYNB、
  ZIP 装配 OCR。已补齐入口，17 类路由测试通过，旧 Office auto 路由保持原样。
- 旧 DOC 中只有控制字符的原生文本不构成可替代正文；新定向测试验证这类内容仍要求 OCR。
- 完整 CLI 单元测试为 324 成功、5 失败、1 忽略。事务取消清理及慢读写关闭两项
  单独重跑通过；以下三项在未修改的 `d276d8a` 测试程序中也稳定失败，单独记录基线：
  `empty_source_and_empty_content_share_the_web_terminal_contract`（空内容状态），
  `metadata_headroom_serializes_multiple_admission_failure_transitions_at_data_boundary`
  与 `permanent_store_headroom_allows_terminal_mutation_at_real_data_boundary`
  （macOS 磁盘计费相差 4096 字节）。最终集成需重新检查主线状态。

安装产物基线另用正式发布 `0.0.4` 的 macOS ARM64 压缩包，来源提交为
`a66287de6978ff3e1a94e1b45f2b0809051eea41`。`apps`、`crates`、模型、third_party
和 Cargo 清单与 `d276d8a` 的生产源码比较无差异。解包后逐项验证 archive manifest
中的文件大小与 SHA-256；CLI SHA-256 为
`4a435e8694a84d82b13d554e1c1f92232005901f1f2a3054212f2a94620eafde`。
完整安装产物矩阵保存在独立目录 `target/ocr340/baseline-installed`，避免混用调试构建。

## 验收状态

- [x] 独立 worktree，原 #334 工作区保留。
- [x] 确定性预算快照、配置覆盖及 jobs 共享额度测试。
- [x] 混合识别/拒绝、重复引用、跨转换缓存隔离与资源释放测试。
- [x] strict/always/必要正文、全局资源、协议、取消、超时的基础定向测试。
- [x] #333 的 PDF 页面预检与坐标回归保留。
- [x] 本地真实 provider 和模型子进程初步运行。
- [ ] 文件级许可、内容类型、正文/资产预期完整核验。
- [ ] 187 文件修复前后的全部模式及批量矩阵。
- [ ] 17 类各有有效识别内容的真实样本 strict/always 验收。
- [ ] 最终源码提交上的 Rust、DTO、CLI/API/Web 和发布契约门禁。
- [ ] 四个发布平台的真实 provider 与安装压缩包证据。
- [ ] PR 整体审核、合并、#340/#331 回填与 worktree 清理。

保持 #333 的安全预检拒绝路径：在运行 OCR 前可保留原生 PDF 页面。#340 的逐图片
运行期恢复只接受结构化识别内存拒绝，固定结构上限、帧、协议、进程异常和共享工作
额度拒绝保持终止。全局安全阈值不因 best-effort 放宽。
