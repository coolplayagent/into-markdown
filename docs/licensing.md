# 许可证治理

项目代码采用 Apache-2.0，根目录 `LICENSE` 是完整许可证文本，`NOTICE` 只陈述
当前可验证的第一方归属和分发边界，不把尚未引入的组件描述为已经合规。

## 权威清单与状态

- `Cargo.lock` 固定 Rust 依赖；`third_party/licenses/rust-lock.tsv` 必须与所有
  非 workspace 包的名称和版本精确一致，并为每个包记录经过选择的 SPDX 义务
  结论。`AND` 的每一项都必须保留并分别通过 allow/deny 策略；例如
  `unicode-ident` 的结论是 `MIT AND Unicode-3.0`，不能简化为 `MIT`。
- `third_party/licenses/inventory.json` 记录原生运行时、模型和未来组件。
- `reviewed` 表示清单字段已经核对，不表示组件一定进入发布物；
  `planned` 表示版本、来源、构建选项或义务仍待确定，不能用于发布。
- `included_in_release` 是发布边界。改为 `true` 前必须补齐版本、来源、SPDX
  许可证、义务和上游声明文件，并把状态改为 `reviewed`。

普通检查完全离线，只读取仓库文件：

```shell
cargo run --locked --offline -p license-check --bin license-check
bazel test //tools/license-check:license_check
```

发布审计使用更严格的模式：

```shell
bazel run //tools/license-check:release_audit
```

任何 Cargo 锁文件漂移、无法与 workspace 清单精确匹配的无来源包、未知许可、
deny 列表命中、清单重复、缺字段，或被纳入发布但仍为 `planned` 的组件都会失败。
严格发布规则不能通过策略配置降级。依赖升级必须同时核对上游许可表达式、选择
完整的兼容义务结论并更新精确版本清单。

## 当前覆盖与后续义务

当前门禁覆盖 Cargo 锁文件中的 Rust 包，并将已固定但只用于手动目标的 ONNX
Runtime 1.29.0、PP-OCRv6 detector/recognizer 源归档，与结构化权威清单
`third_party/licenses/downloads.json` 中的平台键、下载 URL 和哈希逐项绑定。
本地 Bzlmod extension 从同一 JSON 生成下载仓库，不解析或重复维护 MODULE 语法。
后两者没有进入普通构建产物。

TXT 字符集实现使用 `chardetng 0.1.17` 与 `encoding_rs 0.8.35`。前者上游表达式为
`Apache-2.0 OR MIT`，本仓库选择 MIT；后者为 `(Apache-2.0 OR MIT) AND BSD-3-Clause`，
本仓库选择并保留 `MIT AND BSD-3-Clause` 的完整义务结论，因此策略明确允许
`BSD-3-Clause`，且精确版本记录在 `rust-lock.tsv`。

`release-audit` 是不可降级的独立入口，不接受 `check` 或其他模式参数：

```shell
cargo run --locked --offline -p license-check --bin release-audit
```

ONNX Runtime 的唯一版本、API level、四平台压缩包 SHA-256 和解包动态库 SHA-256
及固定二进制的 load identity/系统动态依赖审计结果记录在
`third_party/onnxruntime/manifest.json`；`downloads.json` 只把同一组压缩包映射
为显式 Bazel repository。`ort`/`ort-sys` 固定为 `2.0.0-rc.13`，选择 MIT 许可，关闭
默认 feature、二进制下载和 build.rs 链接，仅启用 `std`、`alternative-backend` 与
API 28 兼容绑定；worker 直接使用该预生成 C API table，运行时仍以 authority 的 API 29
和精确 `GetVersionString` 做探针，父进程不加载 native library。
`object` 固定为 `0.40.0`，从上游 Apache-2.0 OR MIT 中选择 Apache-2.0，关闭默认压缩 feature 且该 crate
没有 build.rs；它只在加载前离线解析固定 ELF、Mach-O 与 PE 头，不下载或链接原生代码。
`prost`/`prost-derive` 固定为 `0.14.4`，选择 Apache-2.0；仓库不运行 `protoc` 或 build.rs，
只编译 checked-in 的安全边界消息类型。ONNX `onnx.proto3` 的来源、v1.20.0 tag、
SHA-256、Apache-2.0 许可、生成器版本、未知字段策略和递归上限记录在
`third_party/onnx/proto-authority.json`。

OCR 检测边界精确固定 `imageproc 0.25.0`，并与图片转换边界共享主分支固定的
`image 0.25.8`。`imageproc` 关闭 default features；workspace 的 `image` 仅启用图片转换
所需的 `gif`/`jpeg`/`png`/`webp` 纯 Rust codec，检测模块本身不调用 decoder/encoder。
`imageproc` 及其 Cargo.lock 传递依赖均为 Rust 数学、字体或容器实现，其精确 SPDX
结论记录在 `rust-lock.tsv`，没有 native library 或下载步骤。closed polygon round
offset 使用 `clipper2-rust 1.1.0`，选择精确 SPDX `BSL-1.0`；完整许可文本在
`third_party/licenses/BSL-1.0.txt`。该 crate 的 crates.io checksum 是
`0fd663fe209e7030c956e3be4c051dcc20cdb73da794f31466762cff12ca11bf`，上游 VCS
revision 是 `09e9505f99a18136505a64485011a292d4375a3a`。已审源码是 18,590 行、
22 个 Rust 文件的纯 Rust port，crate 级 `forbid(unsafe_code)`、`build = false`，唯一
runtime dependency 是 `num-traits`。任何版本升级都必须重新审计源码、unsafe、build、
依赖与许可；许可策略只接受精确 `BSL-1.0`，相邻或宽松拼写由负例测试拒绝。
当前默认
发布边界不携带 native archive，因此 inventory 的 `included_in_release` 保持 `false`；
发布目标若开始随包分发，必须先把上游 MIT 文本加入 release license set 与 SBOM。

模型清单的每个 bundle 都是 OCR bundle，bundle ID 必须唯一，并且必须各自包含唯一
`detector` 与 `recognizer-and-dictionary` 角色。`default_bundle` 必须非空、存在，且
默认 bundle 自身必须包含受管的 detector 与 recognizer/dictionary 源产物；其他
bundle 不能替它补齐。

PDFium `153.0.7999.0` 已按四个平台固定并审查，但仍是 `manual` 输入且不进入
普通构建或当前发布物。分发时必须保留归档内的 `LICENSE` 和完整 `licenses/`
第三方声明目录；显式联网制品审计见 `tools/pdfium-audit.sh`。

FFmpeg、LibreOffice、Wasmtime、字体和生成模型只是占位项。未来纳入时
必须记录具体版本、源码 URL、哈希、补丁、构建开关、许可证文本与 NOTICE 要求。
FFmpeg 还必须由可复现配置检查证明只启用 LGPL-compatible 组件；未知配置直接
阻止发布。

所有源码和二进制归档都必须包含项目的 `LICENSE`、`NOTICE`、
`THIRD_PARTY_NOTICES.md`，以及实际包含组件要求保留的上游许可证和声明。
当前审计核对仓库声明与受管下载输入，不检查已生成归档，也不证明归档中每个文件
都已建档；发布流水线实现归档后仍须增加逐文件/声明完整性检查。
