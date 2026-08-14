# OpenDocument 1.3 安全边界与威胁模型

本实现把 ODT、ODS、ODP 当作不可信 ZIP/XML 容器，目标是确定性地产生统一 IR，而不是复刻办公
套件的完整排版、公式计算或扩展执行环境。解析过程无网络、无外部进程、无系统字体或模板搜索。

## 信任边界

- 可信：调用方给出的资源限制、Engine 创建的执行上下文、仓库固定版本的 Rust 依赖。
- 不可信：ZIP 中央目录和局部项、文件名、CRC/声明大小、manifest、全部 XML、样式继承、重复计数、
  URL、图片编码、元数据和坐标。
- 输出：只有通过统一 IR 验证的文本、结构、来源定位、诊断和完整验证的图片 bytes；链接无访问权限。

## 接受的 profile

接受 ODF 1.2/1.3 package 形式的 `office:document-content`、可选 `document-styles`、
`document-meta`、`document-settings`。这是封闭 profile：每种 part 都有独立的 element/attribute
allowlist，并校验精确根、根的直接子项以及 body/list/table/image 等关键父子关系；仅声明一个已知
namespace 并不足以获得接受。正文分别必须唯一地是 `office:text`、`office:spreadsheet` 或
`office:presentation`。未知或未声明命名空间、错误根/层级/属性、XML 1.0 非法字符、处理指令、DTD
和自定义/外部实体均拒绝。空的活动 element 与带空值的禁用 attribute 也同样拒绝。
manifest 根确定 package 版本；content/styles/meta/settings 显式给出的 `office:version` 必须相同，
省略时继承 package 版本，不能借不同 part 的版本差异切换解析语义。

package 可达图只包含 `content.xml`、可选的 `styles.xml`/`meta.xml`/`settings.xml`、manifest 和
PNG/JPEG/GIF/WebP 图片。core part 必须是精确 `text/xml`；目录必须在 ZIP/manifest 双向存在、以
`/` 结尾、零长度且 media type 为空。其他任意空 media type、泛 XML、OLE/嵌入对象、符号链接、
签名和二进制 part 都拒绝。宏、脚本、事件监听器、远程图片和相对外部引用不在 profile 内。
绝对 HTTP(S)/mailto 超链接及同文档片段仅作为文本目标保存；不产生网络能力。样式只解释可安全映射
到 IR 的粗体、斜体、下划线、删除线、上下标，其他排版属性不会伪造成语义。

## 主要攻击与控制

| 攻击 | 控制 |
| --- | --- |
| 路径穿越、反斜线、绝对路径、重复名 | 读取内容前规范化全部 ZIP 名并拒绝；拒绝 preamble、尾随 EOCD/comment、重复 EOCD、ZIP64 和 split archive |
| raw/semantic ZIP 名混淆 | local/central 原始名称必须逐字节相同且为严格 UTF-8；非 ASCII 名必须设置 bit 11；再与 `zip` 暴露的 raw/semantic 名双向绑定。拒绝 Unicode Path (`0x7075`) 及全部 extra/comment |
| mimetype/local-central 混淆 | 任何 XML 前原始解析 offset 0 local header；要求 exact name、bit 3=0、Stored、CRC/sizes、extraLen=0，再与唯一 central directory 双向绑定 |
| ZIP bomb、伪造大小、CRC 损坏 | 目录阶段累计展开资源上限；所有接受实文件用固定 16 KiB buffer 流到 EOF并校验 CRC/size，即使该图片未被引用 |
| manifest/ZIP 混淆 | 封闭 core/image/empty-directory 图双向绑定；缺失、孤儿、泛 media type、对象、签名和 symlink 均拒绝 |
| 加密包伪装 | ZIP encrypted flag 或 manifest `encryption-data` 均返回稳定 `encrypted` |
| XML bomb/深宽耗尽 | 禁止 DTD/entity，限制深度、事件、字段、节点并定期 checkpoint |
| ODS repeat 放大/稀疏坐标溢出 | 即使尾部空 repeat 不物化也用 checked `offset + repeat` 推进逻辑坐标，并验证后续 origin/span/covered grid |
| 样式/列表图混淆 | 样式键至少绑定 family+name，区分 common/styles automatic/content automatic 优先级并检测同 family 父图环；列表正式解析 level/start/continuation，嵌套省略 style 时只继承外层 identity+level，list-header 保留为无 marker 前置块 |
| 批注范围混淆 | `office:name` ranged annotation 必须唯一、正确嵌套且 start/end 完整配对；重复、悬空、crossing 拒绝。仅保留安全的 `dc:creator`/`dc:date` 与正文 |
| 单元格公式伪装 | `office:value-type` 与唯一缓存 attribute 严格对应；OpenFormula 只接受 `of:=`，缓存值与 `openformula` Code block 隔离 |
| 图片尺寸/codec bomb | 先扫描安全 anchor、只物化可达图片；读取前按 declared bytes/capacity 认证，构造 decoder 前按 header/16 MP working set/decoded output 上界认证；manifest MIME+规范扩展+sniff 三向一致并完整 decode |
| 活动内容或 SSRF | 拒绝脚本、宏、事件、外部图片；超链接只保留惰性安全 scheme |
| 内存预算 gap/双扣 | SPI 预检 permit 派生同上下文 credit；单一 child lease 在分配前持有，临时析构后中央估算缩账 |
| 取消/超时拖延 | ZIP 每 16 KiB、XML event、repeat、worksheet/slide checkpoint；codec 前后 checkpoint 且单次 codec 由 16 MP 硬上限约束 |
| 坐标变换溢出 | 每次 affine 父子 compose、每个变换点和最终 bounds/rotation 都必须保持 finite；slide/shape/group 循环逐项 checkpoint |

残余边界：逻辑内存 accounting 约束由本实现拥有或显式规划的分配，不声称等于 allocator metadata 或
进程 RSS；`zip`、`quick-xml` 和 `image` 的内部状态由保守全请求 preflight credit 及其自身结构限制
覆盖。工作计划按可达 core XML、实际名称 capacity/entry metadata、固定流缓冲、可达图片/完整解码和
阶段峰值计算；未引用图片的 expanded bytes 不进入内存 permit。ODF 的完整视觉布局、公式重算、动画、图表渲染和扩展命名空间不在支持范围内，遇到会明确拒绝
或只保留可证明安全的文本语义。
