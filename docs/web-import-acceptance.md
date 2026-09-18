# 串行导入与本地来源验收

日期：2026-09-18。环境：macOS ARM64，本机 Edge 无头浏览器，Cargo debug 产物；构建并发为 1，Node 堆上限为 512 MiB。当前工作区基于 `011fcf7b994948adf0e8e8f30e90e4bdc557e25f`，版本号保持 0.0.8。本次没有发布或替换用户安装目录中的程序。

## 已定位的批量失败

批量复现中，服务停止接收后返回 `queueUnavailable`。已确认两处存储记账问题：

1. 数据库更新原先把整个任务目录的增长与单次数据库预留比较，同时发生的输入写入、结果发布会被计入数据库增长。
2. 将测量范围收窄到数据库目录后，SQLite WAL 检查点仍可能使主数据库一次增长超过单次预留。诊断用例记录主数据库相关增长 1,503,232 字节、原预留 1,048,576 字节。

现在分别核算数据库目录与其他任务文件；数据库更新前保留 WAL 待检查点页和新页所需的物理空间，更新后检查日志增长、主库增长及全局硬限额。存储损坏和真实超额继续停止队列，并向操作区域返回明确错误。确定性回归覆盖其他任务写入与数据库更新重叠，以及重复发布预览跨越检查点。

## 行为与安全边界

- 服务端文档和转录共用一个转换槽位，覆盖结果发布；浏览器上传和本地复制共用一个接收槽位。接收可以与转换重叠。
- 本地选择、浏览器选择、拖放、粘贴先进入待提交列表。系统选择采用 `rfd::AsyncFileDialog::pick_files`；macOS 通过私有辅助进程在主线程调用原生窗口。
- 本地选择仅向页面提供不透明标识、名称和大小，路径留在服务端；提交通过标识流式复制受控副本，请求无文件正文。原生剪贴板优先文件列表，其次图片；图片保存为受限 PNG 临时快照。
- 未提交授权闲置 30 分钟失效，移除和取消释放授权。复制校验大小及源文件修改，失败要求重新选择。接收记录中的 `localCopy` 标志使刷新后的本地复制继续查询接收状态。
- 原生接口复用会话认证及来源检查，网页无法提交任意本地路径。浏览器粘贴优先事件中的文件，文本编辑控件保留默认粘贴。

## 本机验证结果

| 验证 | 结果 | 证据 |
| --- | --- | --- |
| 连续 20 批，每批 3 CSV + 3 XLSX | 120/120 成功；前置逐个转换对照 6/6 成功 | `target/batch-investigation/run-FflATx/results.json` |
| 上述服务进程内存采样 | 240 个样本，峰值 RSS 71,888 KiB，约 70.2 MiB | `target/batch-investigation/final-memory.json` |
| 8 MiB 填充 PPTX + 小 XLSX、切页、刷新 | 通过；首个接收暂停时只有一个接收记录 | `target/batch-investigation/final-browser.log` |
| 102 个文本文件，任务分页和完整批次导航 | 通过；加上混合文件共 104 个任务 | 同上 |
| 20 次打开结果、30 次批量查询 | 预览 P95 78 ms；查询 P95 5 ms；无浏览器脚本错误 | 同上 |
| 最终内嵌产物：混合文件、刷新、浏览器粘贴两项并移除 | 通过；粘贴后没有自动提交；预览 P95 74 ms，查询 P95 1.9 ms | `target/batch-investigation/final-paste-browser.log` |
| 扫描 PDF、图片、XLSX，逐个及混合提交 | 6/6 成功终态；PDF/图片保留原始附件并报告降级 | `target/batch-investigation/run-Iz2K4G/` |
| CLI 测试 | 356 通过，1 个既有忽略 | `target/batch-investigation/cli-final-tests.log` |
| 管理前端测试 | 28 通过 | `target/batch-investigation/web-admin.log` |
| TaskStore 测试 | 38 通过 | `target/batch-investigation/store-tests.log` |
| 前端六组测试 | 61 通过，组间跳过用于分组 | `target/batch-investigation/web-*.log` |
| CI 策略与结构门禁 | 14 项策略测试通过，结构违规 0 | `target/batch-investigation/structure-final.log` |

CSV 每个 4,000 行、409,803 字节；XLSX 使用仓库 `fixtures/small/xlsx/normal.xlsx`。20 批服务阶段采样：上传 P95 34 ms、接收准备 P95 65 ms、转换 P95 1,186 ms、排队 P95 8,277 ms。发布日志同时包含发布函数和状态阶段的观察值，保留原始日志，不将两者相加。每批端到端约 15 秒。填充 PPTX 主要验证传输与生命周期，不代表复杂原始幻灯片的解析耗时。

20 批候选 SHA-256：`df95395ae144eae7c4f3870bf5503ade952577818017ce428f0b6f13e569ab06`。之后补充取消授权释放、移除重复释放、局部函数提取和本地复制进度文案，重新执行源码测试及最终浏览器验证。最终 `target/debug/into-md` SHA-256：`f9e39283b189d0f363df576a7b296d537b86d046c95cc2370fdeed7269bb9bbf`。内嵌资源摘要由 `web/console/dist/asset-manifest.json` 和 `apps/cli/src/ui_assets.rs` 对应记录。

## 验证边界

- debug 产物未装载 PDFium 和 OCR 能力：PDF 输出 `pdf.recovery.originalPdf`，图片输出 `image.ocrUnavailable`，两者诊断为 `outcome=degraded`。本次混合测试证明降级后队列继续、附件保留；扫描文字识别质量与 OCR 资源取消需要带运行时产物另行验收。
- macOS 原生窗口辅助进程已启动，自动化工具无法连接其窗口（连接超时），原生多选和系统剪贴板的完整交互尚未实测。Windows、Linux 原生交互也未在本机验证。本地授权、源变化、失效和复制管线已有 Rust 测试；前端协议测试确认本地导入请求没有文件正文。
- 浏览器粘贴验收使用事件携带的文件数据，不改写用户系统剪贴板。原生截图解码的初始内存分配受剪贴板库实现约束，应用在取得尺寸后实施上限，详见安全模型。
- 内存数字仅为服务进程 RSS 采样，未包括浏览器、运行时子进程和操作系统缓存，也不覆盖用户原始大文件峰值。用户所述“多次重试后成功”未逐一复刻；上述两个存储错误有独立复现和确定性回归。
- 录音草稿、授权、说话人编辑保留既有回归测试；真实音频转录运行时矩阵本次未重新执行。跨平台及原生交互验证仍是验收剩余项。

## 重复验证

使用 `INTO_MD_CLI` 指向候选产物，运行 `node web/console/tests/batch-stability-local.mjs` 执行逐个对照与 20 批测试，输出保存在 `target/batch-investigation/run-*`。运行 `node web/console/tests/task-flow-local.mjs` 验证混合文件、切页、恢复、粘贴与预览；设置 `INTO_MD_BATCH_COUNT=102` 加入分页验证，`INTO_MD_LARGE_PPTX` 可指定 PPTX 输入。

保持现有四个 fast job：TaskStore 新回归和前端用例自动进入既有任务，CLI、真实浏览器和批量专项在本地运行。工作流及 CI 策略检查未放宽。
