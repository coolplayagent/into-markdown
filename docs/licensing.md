# 许可证治理

项目代码采用 Apache-2.0，根目录 `LICENSE` 是完整许可证文本，`NOTICE` 只陈述
当前可验证的第一方归属和分发边界，不把尚未引入的组件描述为已经合规。

## 可选 FFmpeg 媒体运行时

FFmpeg 8.1.2 只在显式手动工作流中从固定官方源码构建。配置先关闭所有组件，并明确
关闭 GPL、version3、nonfree、网络、自动探测和外部库，再应用最小解码/解封装白名单。
项目不会发现或调用系统 FFmpeg。生产包必须绑定工作流产物与生成的 authority，并履行
LGPL 对应源码、告知、逆向工程及重新链接/替换权利等义务。

## 权威清单与状态

- `Cargo.lock` 固定 Rust 依赖；`third_party/licenses/rust-lock.tsv` 必须与所有
  非 workspace 包的名称和版本精确一致，并为每个包记录经过选择的 SPDX 义务
  结论。`AND` 的每一项都必须保留并分别通过 allow/deny 策略；例如
  `unicode-ident` 的结论是 `MIT AND Unicode-3.0`，不能简化为 `MIT`。
- `third_party/licenses/inventory.json` 记录原生运行时、模型和未来组件。
- `pnpm-lock.yaml` 固定前端依赖的完整 registry integrity；
  `third_party/licenses/npm-inventory.json` 必须与每个 lock package 精确双向覆盖，并记录
  runtime/build/test 范围、SPDX 结论、HTTPS 来源与是否进入发布物。
- `third_party/licenses/npm-release.spdx.json` 是嵌入式控制台生产 JavaScript 的确定性
  SPDX 2.3 SBOM。document namespace 绑定实际资产完整 SHA-256，creationInfo 固定且不含
  时间漂移、本机路径或随机值；包、文件与 relationship ID 必须唯一且不能悬空。
- 根 `//:release_license_files` 是发布归档必须携带的许可证与 SBOM 权威集合。它必须
  精确包含项目 LICENSE/NOTICE、第三方声明、所有实际 runtime npm 许可文本以及上述 SPDX
  SBOM；审计对 inventory 推导出的集合双向比较，缺项与未管理的额外项都失败。
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
bazel --batch run --jobs=2 --local_resources=memory=4096 //tools/license-check:release_audit
```

任何 Cargo 锁文件漂移、无法与 workspace 清单精确匹配的无来源包、未知许可、
deny 列表命中、清单重复、缺字段，或被纳入发布但仍为 `planned` 的组件都会失败。
严格发布规则不能通过策略配置降级。依赖升级必须同时核对上游许可表达式、选择
完整的兼容义务结论并更新精确版本清单。

npm 审计同样拒绝锁新增而未审核、清单孤儿、integrity 漂移、范围与发布标记冲突。
React、React DOM 与 Scheduler 的 MIT 代码进入嵌入式控制台生产资产；发布归档必须保留
其版权与 MIT 许可声明。三者 exact npm tarball 的 `LICENSE` 内容相同，仓库逐字节保存为
`third_party/licenses/npm/react-MIT.txt`；清单记录各自 tarball 来源、完整文件 SHA-256 与
版权文本。发布审计重新计算许可文件和生产 app 的 SHA-256，并要求 release inventory、
SPDX packages、asset-manifest 与 app bytes 双向完全一致；许可文件删除/漂移、SBOM 包删除/
新增、重复 SPDXID、悬空 relationship 与资产漂移均失败。TypeScript、esbuild-wasm 与类型包只用于构建，happy-dom 与
axe-core 只用于测试。axe-core 采用 MPL-2.0：npm 源包和可能分发的 CI/cache 测试产物
保留其文件级声明与对应源代码可获得性义务，但 axe-core 不进入 CLI 或控制台生产资产。

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
所需的 `bmp`/`gif`/`jpeg`/`png`/`tiff`/`webp` Rust codec，检测模块本身不调用
decoder/encoder。
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

模型清单 schema 2 区分完整 `ocr-pipeline` 与 `recognizer-component`，bundle ID 必须唯一。
默认 pipeline 自身必须包含唯一 detector 与 recognizer/dictionary 源角色；独立 recognizer
组件必须包含唯一 recognizer/dictionary source 角色及唯一 recognizer、character-table
runtime 角色。schema 1 只兼容 planned source-only bundle；available/runtime 语义会由产品
validator 与 release audit 双向拒绝。

PDFium `153.0.7999.0` 已按四个平台固定并审查，但仍是 `manual` 输入且不进入
普通构建或当前发布物。分发时必须保留归档内的 `LICENSE` 和完整 `licenses/`
第三方声明目录；显式联网制品审计见 `tools/pdfium-audit.sh`。

LibreOffice runtime 当前不进入仓库、普通测试或 release inventory；机器契约
`third_party/legacy-office/authority.schema.json` 与隔离 worker 已实现，但平台制品只能由包装任务
在核对该安装内 `LICENSE`、完整 third-party notice、文件清单、ABI、size 与 SHA-256 后发布。
ELF/Mach-O/PE 递归依赖也必须分别进入 package inventory 或平台精确 `systemLibraries`；
LibreOffice core 的 MPL-2.0 不能替代随具体版本变化的所有 bundled component 许可。

FFmpeg、Wasmtime、字体和生成模型只是占位项。未来纳入时
必须记录具体版本、源码 URL、哈希、补丁、构建开关、许可证文本与 NOTICE 要求。
FFmpeg 还必须由可复现配置检查证明只启用 LGPL-compatible 组件；未知配置直接
阻止发布。

所有源码和二进制归档都必须包含项目的 `LICENSE`、`NOTICE`、
`THIRD_PARTY_NOTICES.md`，以及实际包含组件要求保留的上游许可证和声明。
控制台进入归档时，还必须从 `//:release_license_files` 复制 React 系列完整 MIT 文本与
`npm-release.spdx.json`；SBOM 是发布物的一部分，不是仅供 CI 使用的中间文件。
当前审计核对仓库声明与受管下载输入，不检查已生成归档，也不证明归档中每个文件
都已建档；发布流水线实现归档后仍须增加逐文件/声明完整性检查。

## Bundled SQLite

任务库固定 `rusqlite 0.37.0`、`libsqlite3-sys 0.35.0` 及 `bundled` feature。该 crate 自带并由
`cc` build script 编译 SQLite 3.50.2 amalgamation，不选择系统库；`pkg-config`/`vcpkg` 仍是
上游 build dependency 的条件分支，版本同样由 lock 与许可证清单固定。两个 Rust 包选择 MIT，
amalgamation 上游声明为 public domain。新引入的 build/iterator/hash helper 已加入
`rust-lock.tsv`，其中 `foldhash 0.1.5` 为 Zlib。Cargo/Bazel 离线 license/release audit 共同
检查锁文件漂移。构建未启用 rusqlite load-extension API，运行时也不调用 extension loading。

图像转换固定 `tiff 0.10.3`，并复用其纯 Rust `fax`、`half`、`crunchy` 依赖；对应精确版本
与许可证已加入 `rust-lock.tsv`。转换器仅在完整 TIFF/BigTIFF envelope、IFD 链、字段范围、
strip/tile 范围与资源上限验证通过后调用 decoder，不加载系统图像库、外部色彩配置、网络
资源或可执行 metadata。`half` 在双许可中选择 Apache-2.0，其余三个包选择 MIT。

## Fixture 许可与来源

`fixtures/small/` 全部由仓库生成，文本由 into-markdown contributors 编写，不复制外部
文档或相邻项目语料，按 Apache-2.0 随仓库和测试发布。`fixtures/manifest.json` 对每个
文件记录作者、SPDX、再分发结论、生成器、获取日期、大小和不可变 SHA-256；license audit
同时拒绝 manifest 未声明文件和缺失文件。

`fuzz/seeds.json` 只引用上述 manifest 内 Apache-2.0 的仓库生成 fixture，以及两个仓库原创
plugin TOML seed。持续 fuzz 发现的最小化样本按内容 SHA-256 固定在
`fuzz/regressions/manifest.json`，标记为仓库 fuzz 过程生成并沿用 Apache-2.0；未经 review
的外部语料不得进入该目录。

OCR 图片使用固定 Noto Sans CJK SC Regular 字体生成。字体来源固定到 notofonts/noto-cjk
commit `f8d157532fbfaeda587e826d4cd5b21a49186f7c`，单文件 SHA-256 为
`2c76254f6fc379fddfce0a7e84fb5385bb135d3e399294f6eeb6680d0365b74b`，许可为
OFL-1.1，全文位于 `third_party/licenses/OFL-1.1.txt`。字体不提交且不进入发布物；生成
PNG 的仓库原创文字与图像按 Apache-2.0 管理。

PP-OCRv6 tiny recognizer ONNX 官方归档固定 SHA-256
`1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29`，许可为
Apache-2.0，既是可由显式 library transport 安装的 recognition component authority，也是
真实 OCR 质量目标的 manual 下载 authority；模型字节不进入语料、普通测试或当前发布物。
两个大输入都必须在 manifest、inventory 与 `fixtures/downloads.json` 三者双向一致；host、大小、hash、
redirect 上限（固定为零）、许可、manual/release 标志、repository 与 portable 下载文件名任一漂移都会失败。
显式 `//fixtures:download_fixture` 工具拒绝重定向，按流式大小上限读取并在原子落盘前校验精确
大小与 SHA-256；普通 Cargo/Bazel 图不会调用它。
