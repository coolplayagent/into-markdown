# ADR 0005：Core 内置 Office 97–2003 解析

状态：已接受

## 背景

DOC、PPT/PPS/POT 与 XLS 过去由可选 LibreOffice 运行时规范化成 OOXML，再进入嵌套转换。
这条路径增加了独立 provider、worker、安装权限、平台打包、运行时下载与供应链投影，并使
Core 安装后仍不能直接转换旧 Office 文件。

合入门禁使用 `tools/legacy-office-performance.py` 在同一 Linux runner 构建 PR base 与候选 Core
CLI，记录可执行文件大小、冷启动、逐格式冷转换、同进程串行批量、并发批量与峰值 RSS。旧基线
缺少可选 runtime 时只记录其不可用状态，不把快速失败时间伪装为转换性能；候选结果使用绝对资源
上限，并对 Core 大小、冷启动和并发吞吐施加相对回退门禁，完整 JSON 作为 CI artifact 保存。

Core 内置实现必须复用统一的 Document IR、source locator、Asset、结构化 diagnostic 和
`ExecutionContext` 资源契约，并保持 PPT 的 slide/notes 结构。共享受限 CFB/OLE reader 还能
避免为旧 Office 引入无关格式依赖、独立运行时和重复的 fixture/许可边界。

## 决策

项目使用自有纯 Rust 解析器，并将 `builtin.converter.legacy-office` 保持为 Core 内置 converter：

- MSG 与旧 Office 共享一个受限 CFB/OLE reader，统一认证 DIFAT、FAT、miniFAT、目录、stream
  链、循环、重叠、截断与资源预算。
- DOC 解析 Word 97–2003 FIB 与 piece table；PPT 按 CurrentUser、UserEdit 与 persist authority
  恢复 slide/notes 顺序；XLS 在共享 CFB 预检后使用已审核的 Calamine BIFF8 reader 和统一
  Workbook IR 组装。
- 公式、宏、ActiveX、外部工作簿和嵌入式可执行对象永不执行，也不触发网络、OCR、AI 或外部
  进程。安全图片载荷在资源限额内作为 Asset 返回；恢复与跳过必须产生结构化 diagnostic。
- 兼容边界固定为 Office 97–2003。更早二进制版本返回稳定 `unsupported`，加密、损坏和超限
  分别返回 `encrypted`、`malformed` 与具体 `resourceLimit`。

## 结果

Core 在 macOS ARM64、Linux x86_64、Linux ARM64 与 Windows x86_64 安装后直接提供旧 Office
能力。发布图由 Core 内置 converter 提供该能力；格式目录、doctor 和 Web 管理页均按 Core
能力报告其状态。

代价是格式前端与安全语义由本仓库维护；因此真实 fixture、损坏语料、确定性、fuzz、资源边界、
临时安装目录 smoke 与四平台回归属于该 converter 的持续合入门禁。
