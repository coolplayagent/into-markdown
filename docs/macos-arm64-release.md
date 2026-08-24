# macOS ARM64 模块化发布

Apple silicon 每个版本固定发布五个独立构件：

- `into-md-macos-arm64-core.dmg`：Developer ID 签名、Apple 公证并 stapling 的 `into-md`、Document IR、管理界面、PDFium、安装器、Core SBOM 与许可材料；
- `official.ocr.ppocrv6.imp`：OCR provider、ONNX Runtime、worker、PP-OCRv6 检测/识别模型与字典；
- `official.media.whisper.imp`：语音 provider、FFmpeg、ONNX Runtime、worker、Whisper、VAD 与说话人模型；
- `official.legacy-office.libreoffice.imp`：旧 Office provider、worker 与固定 LibreOffice runtime；
- `into-markdown-skill.zip`：平台无关的 Agent Skill，另附 SHA-256，并以相同字节内置于 Core。

Core 不携带 FFmpeg、ONNX Runtime、本地 OCR/语音模型或 LibreOffice。每个插件都是通用插件管理器可安装的有界签名 ZIP，`plugin.json` 绑定目标平台、入口、执行权限和全部文件哈希；`provider.json`、SBOM、许可、模型及 runtime 均在同一签名清单内。本地模型是插件私有实现资源，不存在独立安装或切换入口。

## 构建

在原生 ARM64 Mac 上使用 Rust 1.97.1。发布凭据只作为进程文件注入，不能进入仓库或产物：

```sh
brew install gnupg
FFMPEG_AUDIT_NETWORK=1 \
FFMPEG_AUDIT_OUTPUT_DIR="$PWD/target/ffmpeg-audit-aarch64" \
./tools/ffmpeg-build-audit.sh

PYTHONPATH=tools/macos-release python3 tools/macos-release/release.py \
  --output /private/tmp/into-md-core-stage \
  --cache /private/tmp/into-md-release-cache \
  --build-root /private/tmp/into-md-build \
  --ffmpeg-artifacts "$PWD/target/ffmpeg-audit-aarch64" \
  --plugin-signing-key /secure/official-plugin-signing-key.pk8 \
  --plugin-base-url https://github.com/coolplayagent/into-markdown/releases/download/RELEASE_TAG \
  --plugins-output /private/tmp/into-md-plugins \
  --archive /private/tmp/into-md-macos-arm64-core.tar.gz
```

本地命令输出用于确定性门禁的 Core tar 与三个 `.imp`。平台无关的 skill 工作流独立生成两次 ZIP、
逐字节比较并重新验证内容。正式发布工作流先对两次未注入发布凭据的干净构建逐字节比较，再从已验证的同一构建产物生成 Developer ID 签名版本；Core 装入 DMG 后提交 Apple 公证并 stapling，三个插件分别以 ZIP 载体提交公证。最终发布 DMG、三个 `.imp`、skill ZIP 及各自 SHA-256。

发布凭据由 GitHub 受保护的 `release` environment 注入：Developer ID Application 证书、临时 keychain 密码、App Store Connect API key 与插件 Ed25519 私钥都必须非空。工作流结束时删除 keychain 和凭据文件；缺少任一凭据会失败关闭。项目 Mach-O、FFmpeg 与 PDFium 的发布副本在 manifest/SBOM 哈希生成前签名；已具备有效上游 Developer ID 签名的 ONNX Runtime 与 LibreOffice 仍保留其供应商签名。

## 安装与能力管理

Core 使用用户级原子安装器。官方插件目录和摘要由 Core 内的 `official-publisher.json` 固定，管理界面或 `setup` 下载后仍会重新校验包摘要、签名、目标与完整 inventory：

```sh
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
into-md setup ocr
into-md setup media
into-md setup legacy-office
into-md capabilities list
```

安装、更新、验证、禁用、修复、回滚和卸载均复用通用事务式插件管理器。任一插件缺失或损坏只影响对应能力；Core 和其它插件继续工作。

## 验证

发布验证必须从只读挂载的已公证 Core DMG 和空用户目录开始，随后通过公开 CLI/Web 流程安装三个插件。门禁检查：

- Core 与三个插件分别具有签名/哈希/SBOM/许可证据；
- Core DMG 的公证票据和 stapling 验证通过，三个插件的独立公证提交均为 Accepted；
- Core inventory 不出现 `ffmpeg`、`onnxruntime`、`models` 或 `legacy-office-runtime`；
- Core 清单包含完整 Agent Skill，且内置副本与独立 skill ZIP 逐文件一致；
- 插件安装、更新、校验、禁用、修复、卸载和离线重启；
- 真实 DOC/XLS/PPT、扫描 PDF/PNG/JPEG、WAV/MP3/M4A/WebM 和双人语音；
- 本地/远端来源切换、`prefer`/`fallback`/`only`、超时/限流/坏响应/断网、provenance；
- 浏览器刷新、重启、任务恢复、产物下载及控件附近的错误修复。

单元测试、契约测试和故障注入是基础门禁，不能替代真实发布包 E2E。
