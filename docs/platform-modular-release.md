# Linux 与 Windows 模块化发布

最终用户应按[安装与部署指南](user-guide.md)校验 signing policy 与摘要、安装 Core、离线导入能力插件并
排障；本文描述跨平台发布装配与验收权威。

Linux x86_64、Linux ARM64、Windows x86_64、Windows ARM64 与 macOS ARM64 使用相同产品边界：一个包含
Office 97–2003 原生解析的 Core 归档，以及 `official.ocr.ppocrv6`、`official.media.whisper`
两个自包含 `.imp`。Core 归档承载转换与管理功能；每个能力插件包含离线运行所需的完整
FFmpeg/ONNX Runtime、模型、字典、许可、SBOM、签名清单和目标平台声明。

每个最终 Core/插件构件同时发布以完整文件名为前缀的 `.spdx.json`、`.sources.json` 和
`.THIRD_PARTY_NOTICES.md` sidecar，并保留 SHA-256。每个目标另有 `*-signing-policy.json`，明确
记录外部签名模式、发布者身份是否被操作系统验证及对应安装警告。每个目标另有
`into-markdown-<target>-release-set.json` 和聚合 SPDX；其中 `core` 只引用 Core，
`complete-offline` 引用 Core 与两个插件，不产生另一份重复归档。

每个产品版本还发布一份平台无关的 `into-markdown-skill.zip` 与 SHA-256。skill 的 canonical
内容同时进入所有 Core 的 `share/into-markdown/skills/into-markdown/`，并由 Core 归档清单绑定；
用户通过独立 ZIP 或 Core 内置副本将其显式安装到 agent skill 目录。

原生发布入口是 `tools/platform-release/release.py`。它拒绝在非目标架构组装发布件，并固定
Rust 版本、PDFium、ONNX Runtime、模型、FFmpeg 审计产物、下载大小和 SHA-256。
Linux 归档为确定性 `tar.gz`，Windows 归档为确定性 ZIP。发行工作流在对应原生 runner 上
连续组装两次并逐字节比较 Core 和两个插件，然后从归档安装 Core、安装和验证两个插件、运行
DOC/PPT/XLS 真实文件转换并卸载。

Linux 两个架构都在 `authority.json` 固定摘要的 Rocky Linux 8.10 原生容器中构建。发布契约是
glibc 不高于 2.28、运行内核 5.15 以上；x86_64 使用通用 `x86-64`，ARM64 使用通用 ARMv8-A，
容器内固定使用 Rocky AppStream Python 3.11 执行发布工具，不能回退到 EL8 默认 Python 3.6。
组装器显式传递 `target-cpu`，不接受宿主 `native` 特性。Windows 固定 MSVC 14.44.35207 与
Windows SDK 10.0.26100.0。`audit.py` 在最终真实成员上检查 ELF/PE 架构、GLIBC symbol
ceiling、解释器、DT_NEEDED、RPATH/RUNPATH、文件模式、PE import 与项目二进制 Authenticode；
在 unsigned 模式下还要求项目二进制没有意外混入外部 Authenticode 身份。报告作为
`platform-audit.json` 上传。

最终 sidecar 从最终归档生成。Core 成员来自最终
归档的干净解包目录，并先由归档内 `archive-check` 与 `archive-manifest.json` 双向核对；插件
直接遍历最终 ZIP，并要求每个成员的大小和 SHA-256 同时匹配已签名 `plugin.json` 与
`provider.json` runtime inventory。SPDX 2.3 JSON 还会由固定版本、固定 wheel SHA-256 的官方
`spdx-tools` 重新解析和完整验证。

```sh
python tools/platform-release/release.py \
  --target x86_64-unknown-linux-gnu \
  --version 0.0.0 \
  --build-root /absolute/build \
  --build-only

python tools/platform-release/release.py \
  --target x86_64-unknown-linux-gnu \
  --version 0.0.0 \
  --build-root /absolute/build --skip-build \
  --cache /absolute/cache \
  --ffmpeg-artifacts /absolute/ffmpeg-audit \
  --plugin-signing-key /absolute/official-key.pk8 \
  --plugin-base-url https://github.com/coolplayagent/into-markdown/releases/download/RELEASE_TAG \
  --output /absolute/core-stage \
  --plugins-output /absolute/plugins \
  --archive /absolute/into-md-linux-x86_64-core.tar.gz
```

手动候选运行的 `signing_mode` 明确选择 `unsigned` 或 `signed`，默认是可安装的
`unsigned`。unsigned 不读取 Windows PFX 或 Linux GPG secret；Windows 可能显示 Unknown
publisher/SmartScreen，Linux 不生成 `.asc`。signed 模式保留原有路径：Windows 只把 PFX 导入
runner 的临时 CurrentUser 证书存储，并在插件 manifest 哈希前完成项目二进制和 FFmpeg 的
Authenticode 与可信时间戳；Linux 为 Core 和每个 `.imp` 生成 GPG detached signature。选择
signed 却缺少对应凭据时必须失败。两种模式都保留内部 `.imp` Ed25519 签名，该密钥用于包清单
完整性，不是操作系统发布者证书。Core 的 PDFium 保留固定上游字节，ONNX Runtime 保留供应商签名。

Windows 的 OCR 与语音进程使用稳定的零 capability AppContainer 身份。插件管理器只给当前已
验证、不可变的 runtime 快照授予读取和执行 ACL，并由 kill-on-close Job Object 持有整个进程树。

跨平台归档工作流使用原生 `ubuntu-24.04-arm` 与 `windows-11-arm` runner，而不是把交叉编译
误报为 ARM64 运行验收。所有五个目标的发布结论都以原生安装后的公开 CLI/Web E2E 报告为准。
平台无关的 skill 与五个目标制品由统一发布入口共同收敛。

Linux 与 Windows 的安装脚本只是 `bin/into-md-installer` 原生事务工具的兼容包装。Core 进入
以 `archive-manifest.json` SHA-256 命名的不可变 `versions/` 目录；事务日志在发布前写穿，
重复安装验证现有版本，升级只原子切换 `current` authority。Linux 的公开命令是稳定的相对
symlink；Windows 的公开命令是稳定 launcher，它只读取受保护 prefix authority 和
`current.txt`，不把版本路径拼进 `.cmd`。链接/reparse、外部可写目录、外来命令和损坏归档在
改变 current 前拒绝；卸载先隔离版本目录，命令占用时回滚完整旧安装。

`platform_acceptance.py` 与既有 `installed-smoke` 分开，既有参数和报告不变。它只接收已安装
根、两个本地 `.imp`、发布者 authority、安装内 fixture 和外部授权语音 fixture，清空开发环境
并隔离状态，覆盖两插件全部四种组合、两种顺序、幂等重装、校验、启停、移除、重装、错误
摘要、损坏包和运行中语音快照语义；还会启动已安装二进制内的生产 Web bundle，通过带会话
authority 的 loopback API 对两个本地 `.imp` 执行上传、授权安装、状态刷新、停启、损坏后重装
修复和卸载。子进程由 Unix 进程组或 Windows kill-on-close Job Object
持有；统一报告固定记录 target、三类构件 SHA-256、Core 安装树状态、插件矩阵、平台审计、
用例、残留资源和总体结论。

仓库转换为公开前必须手动运行 `Public repository release readiness`。该工作流用固定版本与
SHA-256 的 Gitleaks 扫描完整 Git 历史，并下载扫描 issue/PR metadata、用户附件、仍可读取的
Actions 日志和历史 artifacts。任何发现先吊销/轮换并清理精确历史数据；仓库 owner/admin
转换可见性后立即恢复 main 保护、push ruleset、只读默认 token、fork 审批和受保护 `release`
environment。候选与 tag 发布 job 都只通过该 environment 获取凭据，只有 tag 事件拥有公开
Release 所需的 `contents: write` 权限。

统一入口 `.github/workflows/release.yml` 自动响应与 Cargo `workspace.package.version` 完全一致的
`v<SemVer>` tag。正式 tag 发布强制 `signed`：两个 Linux 目标生成 GPG detached signature，
两个 Windows 目标使用 Authenticode，macOS 使用 Developer ID、公证与 stapling；任一凭据或任一
原生验收失败都不会产生公开 Release。手动 dispatch 只生成候选 Actions artifact，允许 unsigned，
不会创建或修改公开 Release。

公开资产使用 `插件ID-目标.imp`，五个目标的同 ID 插件可安全共存于 GitHub Release 的平面命名空间；
包内插件 ID 和本地 `.imp` 文件名保持不变。统一收敛 job 拒绝重复文件名、缺少的 Core/插件/摘要、
签名策略漂移、未通过的平台审计、installed-smoke、acceptance 或 release-set，并生成
`release-manifest.json` 与 `SHA256SUMS`。正式路径把精确资产集上传到 draft，核对远端文件名后才
切换为公开 Release，避免组件工作流各自发布不完整集合。
