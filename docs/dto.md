# 稳定数据传输契约

`into_markdown` 公共外观统一导出转换结果、诊断、溯源、资源、Bundle 清单和批量报告
DTO。CLI、未来的本机 HTTP/SSE 服务及其他应用都直接使用这些 DTO；应用层不得把
Engine 的内部结构或 CLI 私有结构当作线协议，也不得让 Web crate 依赖 CLI。

## 版本与兼容规则

所有顶层 DTO 都包含必填的 `schemaVersion`。结果、诊断、溯源和批量报告当前为数字
`1`；Bundle manifest 当前为数字 `2`，reader 仍接受 manifest schema 1。字段名使用
lower camel case，枚举值、状态和错误码使用稳定英文标识，不随界面语言变化。
Document IR 自己的 `schemaVersion` 与外围 DTO 独立演进；例如 result 同时包含外围
版本和 `document.schemaVersion`。

同一 schema 版本采用字段 additive 兼容：解码器忽略对象中的未知字段，因此生产方可
增加可选字段；生产方不能删除必填字段、改变已有字段类型或语义。枚举是封闭集合，
同一版本的未知枚举值返回 `invalidJson`；增加枚举值需要提升 schema 版本。错误码字段是
字符串，消费者应保留未知值分支。未知 `schemaVersion` 返回
`unsupportedSchemaVersion`，缺少必填字段或字段类型错误返回 `invalidJson`。其他稳定
DTO 解码错误为 `invalidField`、`invalidBase64`、`duplicateId` 和 `resourceLimit`；显示
文本只用于人工排错，程序必须按错误码处理。

公开 DTO 既不实现 `Serialize`，也不实现 `Deserialize`。因此
`serde_json::from_str::<ResultDto>`、`serde_json::to_string(&result)` 和未来直接使用
`axum::Json<ResultDto>` 作为 extractor 或 response 均在类型层不可用；HTTP、SSE、CLI 和
库调用方入站必须使用 `from_json` 或 `from_json_with_limits`；已有 owned DTO 出站使用
`to_json` 或 `to_pretty_json`，内部 `ConversionResult` 出站使用
`write_json_from_result`；CLI 内部类型化批量报告使用 `BatchReportDto::write_json` 流式
写入暂存事务，保留语义、JSON 总字节及字符串字节限制，不克隆整个 wire 报告或生成
整串 JSON 后回读。该固定类型的出站路径不套用不可信输入的结构数量限制；入站
`from_json` 的默认限额、重复键和深度检查不变。大报告消费者仍须显式选择适合自身
信任边界的 `from_json_with_limits` 预算。这些入口通过私有 Raw/borrowed wire 类型编码或解码，并执行
版本、预算和不变量检查，避免框架绕过稳定边界。字段公开用于读取已验证结果及应用代码
显式构造，但不能直接进入通用 serde/Axum wire 边界。

## 顶层 DTO

- `ResultDto`：`markdown`、版本化 `document`、`assets`、`diagnostics` 和
  `provenance`。`write_json_from_result` 是内部 `ConversionResult` 的借用出站边界，
  `TryFrom<ResultDto>` 是经验证的反向边界。
- `DiagnosticsDto`：独立 HTTP/库诊断响应使用的版本化包裹。
- `ProvenanceListDto`：独立 HTTP/库溯源响应使用的版本化包裹。
- `BundleManifestDto`：固定产物路径及资源索引。schema 2 强制使用 `document.md`、
  `document.ir.json`、`diagnostics.json` 和 `provenance.json`，并以
  `diagnosticsSchemaVersion`、`provenanceSchemaVersion` 声明成员版本。资源路径只允许
  使用 `/`、ASCII 字母数字、`.`、`-`、`_` 的 portable 相对路径；拒绝非 ASCII、
  大小写折叠碰撞、ADS 冒号、Windows 保留设备名、尾随点/空格、超长片段和路径穿越。
  每个资源项代表一个物理 ZIP entry；`id` 是稳定 canonical ID，`sourceAssetIds` 是
  非空、唯一、按字节序排列的完整别名集合。schema 1 输入归一为 `[id]`，写出端生成
  schema 2。
- `BatchReportDto`：包含派生的 `succeeded`、`failed` 和输入稳定顺序的 `items`；状态
  只有 `success`、`failed`。`outcome` 区分 `complete`、`degraded`、`failed`；
  `reasonCode` 细分 `emptySource`、`assetOnly`、`emptyContent` 或首个有损诊断。失败项
  必须有 `errorCode`，成功项不得有 `errorCode`。旧报告缺少 additive 的 `outcome` 或
  `reasonCode` 时仍按原有 status 解码。可选 `durationMs` 表示 worker 获得批量准入后到
  输出提交或失败清理完成的逐项端到端耗时；可选 `processingDurationMs` 表示检测、解析、
  IR 和渲染的 Engine 处理耗时，排除输出 sink 与持久化。顶层可选 `wallDurationMs` 从
  命令开始独立计量到最后一次提交或回滚结束，并发执行时不能以逐项耗时之和替代。

`ResultDto` 保持 schema 1，不新增重复 outcome 字段。显式 `emptySource` / `assetOnly`
证据作为稳定 diagnostics code 随 Result JSON、Web diagnostics artifact 和 bundle
传播；Engine 返回前已经执行共享终态校验，因此结构化消费者不会收到未证明可用的空
成功结果。Web schema 1 历史记录仍只公开任务的 succeeded/failed 状态，不声称保存
`ConversionOutcome`；完整诊断保存在成功任务的 diagnostics artifact 中。

结果示例：

```json
{
  "schemaVersion": 1,
  "markdown": "# Example\n",
  "document": {
    "schemaVersion": 1,
    "metadata": { "title": null, "authors": [], "properties": {} },
    "blocks": []
  },
  "assets": [
    {
      "id": "image-1",
      "filename": "image.png",
      "mediaType": "image/png",
      "dataBase64": "AQID",
      "externalUri": null
    }
  ],
  "diagnostics": [],
  "provenance": []
}
```

批量报告示例：

```json
{
  "schemaVersion": 1,
  "succeeded": 1,
  "failed": 0,
  "wallDurationMs": 15.72,
  "resourceUsage": {
    "sharedLeaseBudgetBytes": 2147483648,
    "sharedLeasePeakBytes": 123456789,
    "ocr": {
      "recognizedRegions": 2,
      "recognizedChars": 21
    }
  },
  "items": [
    {
      "input": "report.pdf",
      "output": "report.md",
      "format": "pdf",
      "status": "success",
      "outcome": "complete",
      "durationMs": 12.34,
      "processingDurationMs": 9.81,
      "diagnostics": [],
      "errorCode": null,
      "message": null,
      "warnings": []
    }
  ]
}
```

`resourceUsage` 是一次 CLI invocation 的共享资源快照，不按并发 `jobs` 倍增。
`sharedLeasePeakBytes` 是根 `ExecutionContext` 与全部 fork 的真实历史高水位，拒绝的
reserve 不抬高它，且任务释放 lease 后仍保留。OCR 计数只累计经过非空/置信度过滤、
原生文本去重并完成结构化合并的 region 与 Unicode scalar；OCR 已启用但无命中时两个
字段都为 `0`，关闭时省略 `ocr`。旧 schema 1 报告可缺少 `resourceUsage`；新生产者
必须提供大于零的 budget，并保证 `0 <= peak <= budget`。

## 不可信输入边界

`from_json_with_limits` 在类型化使用前限制 JSON 字节数、结构深度、资源数、base64
解码后总字节数、诊断数、溯源数和批量项数。标准解码还执行以下检查：

- 原始 JSON 在创建值树前按 JSON 字符串/转义规则保守扫描深度、分隔符代表的结构项、
  单字符串和总字符串编码字节预算；结构项计数用于提前拦截超宽对象与数组，不作为精确
  语义节点统计；
  `serde_json` 自身的递归限制继续作为第二道保护，JSON 总字节上限约束值树的整体分配；
- 预算扫描后、值树分配前遍历 JSON token，并按转义解码后的完整 member 名在每个对象
  内判重；顶层、内嵌对象及未知 additive 字段中的重复 member 均返回 `invalidJson`，不
  接受解析器的 last-wins 行为。判重遍历仍受前述字节、深度、结构项和字符串预算约束；
- base64 必须使用 RFC 4648 标准字母表和规范 padding；先按编码长度检查剩余预算，
  再分配解码缓冲区。空 base64 必须同时提供安全的 `externalUri`；两者都有时 base64 是
  内容，URI 只是审计提示，任何消费者都不得据此自动取回资源；
- `externalUri` 只允许规范的绝对 HTTP(S) URL，且不得包含 userinfo、query 或
  fragment。内部模型和 JSON 输入只要带有未净化 URI 就拒绝转换，不会静默删除或改写
  provenance；`file:` 等本地语义 URI不进入协议，以防密钥、签名参数和本地路径泄露；
- 同一 result 或 manifest 中的资源 ID 不能重复，manifest 路径也不能重复；
- provenance confidence 必须是有限的 `0..=1`，locator 矩形坐标必须全部有限；可选
  `byteStart`/`byteEnd` 必须同时出现并构成有序的原始输入半开字节范围；
- 内嵌 Document IR 在反序列化为 `ResultDto` 前先调用 IR 的有界解码，必须通过它自己
  的版本、宽表预检、节点、重复 ID、路径及结构预算校验；
- Bundle 路径只描述归档内成员，不能直接作为宿主文件系统写入目标。

出站的 `to_json`、`to_pretty_json` 和 Bundle 成员 serializer 使用与默认解码相同的
JSON 总字节、深度、结构项、单字符串和总字符串预算。因此任何成功序列化的默认 DTO
都能由对应默认入口读回；超大 Markdown 或 base64 不会出现“能写不能读”。base64 解码
总预算为 32 MiB，同时受 8 MiB 单 JSON 字符串、48 MiB 总字符串和 64 MiB JSON 总量
约束。内部 result 出站时先按原始 bytes 用 checked arithmetic 计算逐项及聚合后的
padded base64 长度，再把完整 result 按所选 `Compact` 或 `Pretty` 布局以及 private wire
的相同字段和顺序流式写入无缓冲计数器：
Markdown、Document、诊断、溯源、资源元数据、JSON 固定开销及转义后的实际长度都会与
base64 精确预计长度合并。在任何 base64 缓冲区分配前拒绝资源数、原始总量、单字符串、
总字符串或预计 JSON wire 预算必然超限的结果；Document 只经过计数 writer，不创建巨大
JSON 副本。完整预检通过后，`write_json_from_result` 把非资源字段直接从内部模型借用
序列化，并通过 base64 的固定小缓冲逐项编码到调用方 writer，不构造 owned DTO、Raw
副本或 base64 String。`json_from_result` 只分配最终 JSON 缓冲。CLI `result-json` 选择
`Pretty` 并直接使用该借用写接口，因此缩进导致的额外字节也在任何 base64 编码前计入；
成功的 compact 或 pretty 输出都能由默认入口读回。

Bundle manifest schema 2 保留既有 `diagnostics.json` 与 `provenance.json` 裸数组形状，成员版本
由 manifest 统辖；`to_bundle_pretty_json` / `from_bundle_json` 是这两个成员的专用边界。
带 `schemaVersion` 的 envelope 只用于独立 HTTP/库响应。这样不会把曾经改变过的成员
形状伪称为兼容。`assets/` 目录成员始终存在，即使没有资源。

默认上限由 `DtoLimits` 和 `MAX_DTO_*` 常量公开，覆盖 JSON 总字节、深度、结构项、
单字符串、总字符串、各类记录数和 base64 解码后总量。HTTP 层可按请求策略进一步收紧，
但不能绕过这些验证后直接反序列化为内部模型。SSE 的事件包络由相应 Web 任务定义，
其结果、诊断和报告载荷复用这里的 DTO。
