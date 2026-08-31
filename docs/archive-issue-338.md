# #338 PPTX 与归档兼容性验收

## 交付边界

PPTX 以每部件非 EOF XML 事件预算替代同深度累计元素宽度；默认 2,000,000，
拒绝零值，覆盖 MCE 所有分支及辅助扫描。Core/API、配置、CLI 参数和 Web 请求上限一致。
资源、安全、取消及超时失败继续终止转换；现有深度、几何和最终 IR 预算独立生效。

ZIP/EPUB 保留解码后的逻辑路径，兼容归一化及完整大小写折叠用于冲突检查。
全部清单按组件检查重复、Unicode/大小写别名及双向文件/目录前缀冲突；原路径和
兼容别名都检查穿越、盘符/ADS、设备名、尾部危险字符及链接类型。
归档内存读取与内容寻址资产落盘沿用现有边界，索引和规范化展开计入内存预算。

RAR4/5 使用[官方完整签名](https://www.rarlab.com/technote.htm)，统一返回 `unsupported`
及 `archiveExtractionRequired` 原因码，提示先解压。截断签名为 `malformed`。
普通文本中的签名字样不触发识别，扩展名无法覆盖真实内容。RAR 目录状态为
`unsupported`；Web 允许上传以获取正确的内容检测结果，在任务行、失败详情及混合归档
结果内就近展示建议。此功能不提供 RAR 解压器。

## 公开样本与边界回归

[samples.json](../tools/archive-compat/samples.json) 固定 48 个不同公开文件的来源版本、URL、
源文件与解码后 SHA-256、分发许可、特征和预期。每类 12 个；RAR4 与 RAR5 各 6 个。
下载及转换正文只存测试缓存，Git 和发布包仅包含清单、脚本与统计证据。

macOS ARM64 上以主线 `26aaf83` 对比修复版、相同配置（OCR off、best-effort、extract）：

| 类型 | 数量 | 成功转换 | 正确拒绝 / 既有边界 |
| --- | ---: | ---: | --- |
| PPTX | 12 | 12 | 0 |
| ZIP | 12 | 5 | 7：头部不一致、链接、大小写冲突 |
| RAR | 12 | 0 | 12：明确 unsupported 与先解压建议 |
| EPUB | 12 | 10 | 2：处理指令安全策略、既有 EPUB XML 事件上限 |

主线成功的 27 个文件，正文 SHA-256 和全部提取资产引用/哈希在修复版保持一致。
俄文 ZIP 两例包含 `ПРИВЕТ`/`привет` 别名，修复版正确定位为冲突。
`haruko-html-jpeg.epub` 与 `linear-algebra.epub` 在相同主线分别复现 XML PI 拒绝和
`epub_xml_events` 超限；验收保留失败，未跳过样本或放宽安全边界。

公开 PPTX 的最大同深度累计元素数为 31,846，未复现报告中的 100,000 width 故障。
边界回归单独构造 100,001 个文本片段，在默认事件预算下完整保留全部正文；低事件
预算明确失败。该结果属于最小边界复现，与报告者原文件的直接复现分开记录。

源码回归覆盖事件/深度精确边界、未选中 MCE 分支、Unicode 原名与资产来源、别名、
双向前缀、路径穿越、特殊文件、CRC、压缩比/解压量、取消、超时与内存租约释放。
恢复路径重复转换 RAR 保持相同错误码及解压建议。安装后的离线转换回归同时验证中文目录实际
写入和 Markdown 图片链接，另复跑每类一个固定公开代表文件。

## 复跑

```sh
cargo build --locked -p into-markdown-cli
python3 tools/archive-compat/run.py --into-md target/debug/into-md \
  --work-root target/issue-338/candidate --cache target/issue-338/public --public-samples
python3 tools/archive-compat/run.py --into-md /absolute/path/to/baseline/into-md \
  --work-root target/issue-338/baseline --cache target/issue-338/public --public-samples --baseline
python3 tools/archive-compat/compare.py \
  target/issue-338/baseline/report.json target/issue-338/candidate/report.json
```

主线使用独立 Cargo target，防止同名 workspace crate 的编译缓存混用。
`report.json` 记录二进制版本/哈希、源码版本、完整命令、退出码、诊断、正文和资源哈希、
耗时与进程内存峰值。耗时和 RSS 是本机观测；预算租约精确值由源码测试断言。

## 证据状态

- 本地：Core、Engine、转换器、API 整组通过；CLI、Web、许可与发布契约在最终提交复核。
- 四平台：专用 PR 工作流覆盖 macOS ARM64、Linux x86_64/ARM64、Windows x86_64，
  每个平台执行真实样本、中文落盘、资产链接及精确 PR 基线对比。
- 安装产物：发布 build-only 工作流从实际归档提取可执行文件，执行合成安全回归和
  PPTX/ZIP/RAR/EPUB 代表文件；包的版本、哈希和原生验收报告由发布证据保存。
- 报告者原文件：未取得，保留明确限制。

CI、安装产物运行链接及最终逐项结论在 PR 和 #331 中回填；待运行项以实际结果为准。
