# 稳定数据传输契约

`into_markdown` 公共外观统一导出转换结果、诊断、溯源、资源、Bundle 清单和批量报告
DTO。CLI、未来的本机 HTTP/SSE 服务及其他应用都直接使用这些 DTO；应用层不得把
Engine 的内部结构或 CLI 私有结构当作线协议，也不得让 Web crate 依赖 CLI。

## 版本与兼容规则

所有顶层 DTO 都包含必填的 `schemaVersion`，当前值为数字 `1`。字段名使用
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

## 顶层 DTO

- `ResultDto`：`markdown`、版本化 `document`、`assets`、`diagnostics` 和
  `provenance`。`ResultDto::from_result` 和 `TryFrom<ResultDto>` 是内部
  `ConversionResult` 的显式双向边界。
- `DiagnosticsDto`：Bundle `diagnostics.json` 和独立 HTTP 诊断响应使用的版本化包裹。
- `ProvenanceListDto`：Bundle `provenance.json` 和独立 HTTP 溯源响应使用的版本化包裹。
- `BundleManifestDto`：固定产物路径及资源索引。资源路径必须是使用 `/` 的规范相对
  路径，不允许绝对路径、反斜杠、空片段、`.`、`..`、NUL 或 Windows drive 前缀。
- `BatchReportDto`：包含派生的 `succeeded`、`failed` 和输入稳定顺序的 `items`；状态
  只有 `success`、`failed`。失败项必须有 `errorCode`，成功项不得有 `errorCode`。

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
  "items": [
    {
      "input": "report.pdf",
      "output": "report.md",
      "format": "pdf",
      "status": "success",
      "diagnostics": [],
      "errorCode": null,
      "message": null,
      "warnings": []
    }
  ]
}
```

## 不可信输入边界

`from_json_with_limits` 在类型化使用前限制 JSON 字节数、结构深度、资源数、base64
解码后总字节数、诊断数、溯源数和批量项数。标准解码还执行以下检查：

- 原始 JSON 在创建值树前按 JSON 字符串/转义规则保守扫描深度、分隔符代表的结构项、
  单字符串和总字符串编码字节预算；结构项计数用于提前拦截超宽对象与数组，不作为精确
  语义节点统计；
  `serde_json` 自身的递归限制继续作为第二道保护，JSON 总字节上限约束值树的整体分配；
- base64 必须使用 RFC 4648 标准字母表和规范 padding；先按编码长度检查剩余预算，
  再分配解码缓冲区。空 base64 必须同时提供安全的 `externalUri`；两者都有时 base64 是
  内容，URI 只是审计提示，任何消费者都不得据此自动取回资源；
- `externalUri` 只允许规范的绝对 HTTP(S) URL，且不得包含 userinfo、query 或
  fragment。内部模型和 JSON 输入只要带有未净化 URI 就拒绝转换，不会静默删除或改写
  provenance；`file:` 等本地语义 URI不进入协议，以防密钥、签名参数和本地路径泄露；
- 同一 result 或 manifest 中的资源 ID 不能重复，manifest 路径也不能重复；
- provenance confidence 必须是有限的 `0..=1`，locator 矩形坐标必须全部有限；
- 内嵌 Document IR 在反序列化为 `ResultDto` 前先调用 IR 的有界解码，必须通过它自己
  的版本、宽表预检、节点、重复 ID、路径及结构预算校验；
- Bundle 路径只描述归档内成员，不能直接作为宿主文件系统写入目标。

默认上限由 `DtoLimits` 和 `MAX_DTO_*` 常量公开，覆盖 JSON 总字节、深度、结构项、
单字符串、总字符串、各类记录数和 base64 解码后总量。HTTP 层可按请求策略进一步收紧，
但不能绕过这些验证后直接反序列化为内部模型。SSE 的事件包络由相应 Web 任务定义，
其结果、诊断和报告载荷复用这里的 DTO。
