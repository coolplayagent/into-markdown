# 用户安装、离线部署与故障排查

[English](user-guide.en.md) · [CLI 示例](cli-examples.md)

正式发布由一个平台 Core、两个自包含能力插件和 Agent Skill 组成。每个 Core/插件同时发布
SHA-256、SPDX、来源与第三方声明 sidecar；每个目标的 `*-signing-policy.json` 明确说明是否具有
外部发布者签名。只组合相同版本和目标平台的构件。当前默认发布模式是 `unsigned`：可以安装，
但操作系统不能验证发布者身份。两个 `.imp` 始终保留内部 Ed25519 清单签名和 SHA-256 固定。

| 能力 | 构件 |
| --- | --- |
| 普通文档、Office 97–2003、PDF、Web 工作台 | 对应平台 Core |
| OCR | `official.ocr.ppocrv6-<target>.imp` |
| 转写与说话人分离 | `official.media.whisper-<target>.imp` |
| Agent 指令 | `into-markdown-skill.zip` |

Core 原生解析 Office 97–2003 的 `.doc/.ppt/.xls` 文件。

## 安装 Core

macOS ARM64 先校验摘要，再根据 signing policy 挂载 DMG 并运行根目录安装器：

```sh
shasum -a 256 -c into-md-macos-arm64-core.dmg.sha256
# unsigned 发布在摘要匹配后可移除下载隔离；signed 发布改用 spctl 验证
xattr -d com.apple.quarantine into-md-macos-arm64-core.dmg 2>/dev/null || true
hdiutil attach into-md-macos-arm64-core.dmg
# 使用 hdiutil 输出的实际卷路径
cd "/Volumes/into-markdown"
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

unsigned DMG 使用 ad-hoc Mach-O 签名保证 Apple silicon 可执行性，但没有 Developer ID 或 Apple
公证；也可以保留 quarantine，并在“系统设置 → 隐私与安全”中选择“仍要打开”。只有 policy 为
`signed` 时才应执行 `spctl --assess --type open --verbose=2` 并要求通过。不支持 macOS x86_64。

Linux 选择与 `uname -m` 匹配的 x86_64 或 ARM64 归档：

```sh
sha256sum -c into-md-linux-x86_64-core.tar.gz.sha256
mkdir into-md-core
tar -xzf into-md-linux-x86_64-core.tar.gz -C into-md-core
cd into-md-core
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

ARM64 使用 `into-md-linux-arm64-core.tar.gz`。只有 signing policy 为 `signed` 时才会发布 `.asc`，
此时额外运行 `gpg --verify`；unsigned 模式以 GitHub Release 旁的 SHA-256 sidecar 为安装前校验
权威。安装器不修改 shell profile。

Windows x86_64 与 Windows ARM64 在 PowerShell 先校验 ZIP 摘要；ARM64 使用
`into-md-windows-arm64-core.zip`，以下 x86_64 示例中的文件名按架构替换。unsigned ZIP 只有在
摘要匹配后才解除下载标记：

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64-core.zip).Hash
Unblock-File .\into-md-windows-x86_64-core.zip
Expand-Archive .\into-md-windows-x86_64-core.zip .\into-md-core
& .\into-md-core\bin\archive-check.exe .\into-md-core
powershell -NoProfile -ExecutionPolicy Bypass -File .\into-md-core\Install.ps1
```

摘要必须匹配发布 sidecar。unsigned 发布会显示 `Unknown publisher` 或 SmartScreen 提示，这是预期
行为；不要在摘要不匹配时绕过提示。只有 signing policy 为 `signed` 时才运行
`Get-AuthenticodeSignature` 并要求 `Status` 为 `Valid`。

Linux 和 Windows 重复运行同一安装命令会验证并修复相同版本，而不是返回冲突。升级保留旧的
不可变版本，只有新归档完整校验后才切换。Windows PATH 中的 `into-md.exe` 是稳定 launcher，
因此不要把 `versions/<摘要>/bin` 手工加入 PATH，也不要修改同目录的 `into-md.prefix`。
文件占用导致升级或卸载失败时先结束提示中对应的本地任务并重试；失败不会删除原安装。

## 验证与能力安装

```sh
into-md version --json
into-md formats --json
into-md capabilities list --json
into-md doctor --json
into-md setup ocr
into-md setup media
```

`setup` 是联网安装完整能力插件的显式命令，包内包含对应模型与运行时。转换和状态查询使用
当前已安装的能力状态。

## 完整离线部署

在联网机器验证 Core、两个 `.imp` 和 sidecar，通过受控介质传入隔离环境。安装 Core 后使用其
固定的官方发布者身份：

```sh
installed="$HOME/.local/share/into-markdown/current"
catalog="$installed/share/into-markdown/plugins/official-publisher.json"
signer_id=$(jq -r .signingKeyId "$catalog")
signer_sha=$(jq -r .signingKeySha256 "$catalog")
target=x86_64-unknown-linux-gnu # 改为当前平台 target
for package in official.ocr.ppocrv6 official.media.whisper; do
  file="/media/release/$package-$target.imp"
  sha=$(sha256sum "$file" | awk '{print $1}')
  into-md plugins install "$file" --sha256 "$sha" \
    --signing-key-id "$signer_id" --signing-key-sha256 "$signer_sha" --scope global
  into-md plugins verify "$package" --scope global
done
into-md capabilities list --json
```

macOS 用 `shasum -a 256`；Windows 用 `Get-FileHash` 和同一 catalog 字段。离线安装不增加
`--allow-network`。

## 转换与网络

```sh
into-md report.docx -o report.md --conflict error --log-format json
into-md documents --recursive --output-dir markdown --conflict error --dry-run
into-md documents --recursive --output-dir markdown --conflict error \
  --report conversion-report.json --log-format json
into-md meeting.webm --ai audio-transcription=only --diarize \
  -o meeting.md --conflict error --log-format json
```

远程来源由当前调用增加 `--allow-network`，尽量用 `--allow-host` 收窄；回环/私网还需
`--allow-private-network`。完整覆盖见 [CLI 示例](cli-examples.md)。

## 故障排查与卸载

保留退出码和 `--log-format json` 稳定事件，再运行 `into-md doctor --json`。

| 信号 | 处理 |
| --- | --- |
| `componentUnavailable` | 用 `capabilities show <ID> --json` 定位插件，执行 `setup` 或离线重装。 |
| `networkDenied` | 确认远端意图，只授权精确 host；私网另行授权。 |
| `outputConflict` | 保留原文件，明确授权后才 overwrite。 |
| `malformed` / `invalidMedia` | 输入损坏或不匹配，不改扩展名重试。 |
| `pluginSandboxUnavailable` | 核对 Core/插件目标和平台隔离能力。 |
| `hashMismatch` / `invalidManifest` | 停止使用并从正式发布源重新取得。 |

普通诊断不能定位损坏时才运行 `doctor --deep`。公开 issue 不粘贴 API Key、带 query 的 URL、
私有路径或敏感内容。

```sh
./uninstall "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

```powershell
& .\into-md-core\Uninstall.ps1
```

卸载器负责产品树和命令 shim；用户自行复制的 Agent Skill 或建立的链接由用户单独管理。
