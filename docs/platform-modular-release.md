# Linux 与 Windows 模块化发布

Linux x86_64、Linux ARM64 与 Windows x86_64 和 macOS ARM64 使用相同产品边界：一个只含
Core 能力的归档，以及 `official.ocr.ppocrv6`、`official.media.whisper`、
`official.legacy-office.libreoffice` 三个自包含 `.imp`。Core 不包含 FFmpeg、ONNX Runtime、
OCR/语音模型或 LibreOffice；每个插件包含离线运行所需的完整 runtime、模型、字典、许可、
SBOM、签名清单和目标平台声明。

每个产品版本还发布一份平台无关的 `into-markdown-skill.zip` 与 SHA-256。skill 的 canonical
内容同时进入所有 Core 的 `share/into-markdown/skills/into-markdown/`，并由 Core 归档清单绑定；
安装器和卸载器都不会修改用户的 agent skill 目录。

原生发布入口是 `tools/platform-release/release.py`。它拒绝在非目标架构组装发布件，并固定
Rust 版本、PDFium、ONNX Runtime、模型、LibreOffice、FFmpeg 审计产物、下载大小和 SHA-256。
Linux 归档为确定性 `tar.gz`，Windows 归档为确定性 ZIP。发行工作流在对应原生 runner 上
连续组装两次并逐字节比较 Core 和三个插件，然后从归档安装 Core、安装和验证三个插件、运行
旧 Office 真实文件转换并卸载。

```sh
python tools/platform-release/release.py \
  --target x86_64-unknown-linux-gnu \
  --build-root /absolute/build \
  --build-only

python tools/platform-release/release.py \
  --target x86_64-unknown-linux-gnu \
  --build-root /absolute/build --skip-build \
  --cache /absolute/cache \
  --ffmpeg-artifacts /absolute/ffmpeg-audit \
  --plugin-signing-key /absolute/official-key.pk8 \
  --plugin-base-url https://downloads.example/into-md/x86_64-unknown-linux-gnu \
  --output /absolute/core-stage \
  --plugins-output /absolute/plugins \
  --archive /absolute/into-md-linux-x86_64-core.tar.gz
```

Windows 工作流先对两次组装结果做逐字节比较，再从同一批已签名项目二进制生成最终构件。
受保护的 PFX 只导入当前 runner 的临时 CurrentUser 证书存储，签名命令仅传递非敏感
thumbprint；语音插件中的 FFmpeg 发布副本也在插件 manifest 哈希生成前执行 Authenticode
与可信时间戳，插件内 authority 随签名字节重新绑定。Core 的 PDFium 保留固定上游字节，
因此运行时仍可直接按仓库 authority 校验；ONNX Runtime 与 LibreOffice 保留供应商签名。
工作流结束后移除证书和 PFX。Linux 对 Core
和每个 `.imp` 生成受保护 GPG 密钥的 detached signature。缺少发布凭据时工作流必须失败，
不能用临时签名冒充公开发布签名。

Windows 旧 Office 插件使用稳定的零 capability AppContainer 身份，使已签名 runtime authority、
Provider 进程和单独的兼容 worker 绑定同一个 SID。插件管理器仍只给当前已验证、不可变的 runtime
快照授予读取和执行 ACL；其他插件继续使用按安装作用域派生的隔离身份。

跨平台归档工作流使用 GitHub 当前公开的原生 `ubuntu-24.04-arm` 标签，而不是在 x86_64
runner 上把交叉编译误报为 ARM64 运行验收。所有发布结论仍以原生安装后的公开 CLI/Web E2E
报告为准。平台无关的 skill 工作流先通过后，三平台发布 job 才开始组装 Core 与能力插件。
