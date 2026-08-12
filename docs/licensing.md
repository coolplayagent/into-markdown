# 许可证治理

项目代码采用 Apache-2.0，根目录 `LICENSE` 是完整许可证文本，`NOTICE` 只陈述
当前可验证的第一方归属和分发边界，不把尚未引入的组件描述为已经合规。

## 权威清单与状态

- `Cargo.lock` 固定 Rust 依赖；`third_party/licenses/rust-lock.tsv` 必须与所有
  非 workspace 包的名称和版本精确一致，并为每个包记录经过选择的 SPDX 许可。
- `third_party/licenses/inventory.json` 记录原生运行时、模型和未来组件。
- `reviewed` 表示清单字段已经核对，不表示组件一定进入发布物；
  `planned` 表示版本、来源、构建选项或义务仍待确定，不能用于发布。
- `included_in_release` 是发布边界。改为 `true` 前必须补齐版本、来源、SPDX
  许可证、义务和上游声明文件，并把状态改为 `reviewed`。

普通检查完全离线，只读取仓库文件：

```shell
cargo run --locked --offline -p license-check -- check
bazel test //tools/license-check:license_check
```

发布审计使用更严格的模式：

```shell
bazel run //tools/license-check:release_audit
```

任何 Cargo 锁文件漂移、未知许可、deny 列表命中、清单重复、缺字段，或被纳入
发布但仍为 `planned` 的组件都会失败。依赖升级必须同时核对上游许可表达式、选择
兼容许可并更新精确版本清单。

## 当前覆盖与后续义务

当前覆盖 Cargo 锁文件中的 Rust 包、已固定但只用于手动目标的 ONNX Runtime
1.29.0，以及 PP-OCRv6 detector/recognizer 源归档。后两者没有进入普通构建产物。

PDFium、FFmpeg、LibreOffice、Wasmtime、字体和生成模型只是占位项。未来纳入时
必须记录具体版本、源码 URL、哈希、补丁、构建开关、许可证文本与 NOTICE 要求。
FFmpeg 还必须由可复现配置检查证明只启用 LGPL-compatible 组件；未知配置直接
阻止发布。

所有源码和二进制归档都必须包含项目的 `LICENSE`、`NOTICE`、
`THIRD_PARTY_NOTICES.md`，以及实际包含组件要求保留的上游许可证和声明。
