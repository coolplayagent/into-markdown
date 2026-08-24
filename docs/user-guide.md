# 用户安装、离线部署与故障排查

[English](user-guide.en.md) · [CLI 示例](cli-examples.md)

正式发布由一个平台 Core、三个自包含能力插件和 Agent Skill 组成。每个 Core/插件同时发布
SHA-256、签名、SPDX、来源与第三方声明 sidecar。只组合相同版本和目标平台的构件。

| 能力 | 构件 |
| --- | --- |
| 普通文档、PDF、Web 工作台 | 对应平台 Core |
| OCR | `official.ocr.ppocrv6.imp` |
| 转写与说话人分离 | `official.media.whisper.imp` |
| DOC/XLS/PPT | `official.legacy-office.libreoffice.imp` |
| Agent 指令 | `into-markdown-skill.zip` |

## 安装 Core

macOS ARM64 校验摘要、公证与挂载内容后运行 DMG 根目录安装器：

```sh
shasum -a 256 -c into-md-macos-arm64-core.dmg.sha256
spctl --assess --type open --verbose=2 into-md-macos-arm64-core.dmg
hdiutil attach into-md-macos-arm64-core.dmg
cd "/Volumes/Into Markdown"
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

卷名以 `hdiutil` 输出为准；不支持 macOS x86_64。

Linux 选择与 `uname -m` 匹配的 x86_64 或 ARM64 归档：

```sh
sha256sum -c into-md-linux-x86_64-core.tar.gz.sha256
gpg --verify into-md-linux-x86_64-core.tar.gz.sig into-md-linux-x86_64-core.tar.gz
mkdir into-md-core
tar -xzf into-md-linux-x86_64-core.tar.gz -C into-md-core
cd into-md-core
./bin/archive-check .
./install "$HOME/.local/share/into-markdown" "$HOME/.local/bin"
```

ARM64 使用 `into-md-linux-arm64-core.tar.gz`。安装器不修改 shell profile。

Windows x86_64 在 PowerShell 校验 ZIP 摘要及内部项目可执行文件的 Authenticode：

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64-core.zip).Hash
Expand-Archive .\into-md-windows-x86_64-core.zip .\into-md-core
Get-AuthenticodeSignature .\into-md-core\bin\into-md.exe | Format-List
& .\into-md-core\bin\archive-check.exe .\into-md-core
& .\into-md-core\Install.ps1
```

摘要必须匹配发布 sidecar，Authenticode `Status` 必须为 `Valid`。

## 验证与能力安装

```sh
into-md version --json
into-md formats --json
into-md capabilities list --json
into-md doctor --json
into-md setup ocr
into-md setup media
into-md setup legacy-office
```

`setup` 只在明确安装动作中联网。转换和状态查询不下载插件或模型；模型属于完整插件内部。

## 完整离线部署

在联网机器验证 Core、三个 `.imp` 和 sidecar，通过受控介质传入隔离环境。安装 Core 后使用其
固定的官方发布者身份：

```sh
installed="$HOME/.local/share/into-markdown/current"
catalog="$installed/share/into-markdown/plugins/official-publisher.json"
signer_id=$(jq -r .signingKeyId "$catalog")
signer_sha=$(jq -r .signingKeySha256 "$catalog")
for package in official.ocr.ppocrv6 official.media.whisper official.legacy-office.libreoffice; do
  file="/media/release/$package.imp"
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

卸载器只删除产品树和命令 shim，不删除用户自行复制或链接的 Agent Skill。
