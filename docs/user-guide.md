# 用户安装、离线部署与故障排查

[English](user-guide.en.md) · [CLI 示例](cli-examples.md)

正式发布提供四个平台的 Core ZIP、对应的可选语音插件、Agent Skill 和审计 ZIP。Core 已包含 OCR 模型与运行时。选择相同版本、相同目标平台的构件；发布说明中的 `UNSIGNED` 表示没有操作系统发布者签名，语音 `.imp` 仍使用内部 Ed25519 清单签名。

| 平台 | Core |
| --- | --- |
| macOS Apple Silicon | `into-md-macos-arm64.zip` |
| Linux x86_64 | `into-md-linux-x86_64.zip` |
| Linux ARM64 | `into-md-linux-arm64.zip` |
| Windows x86_64 | `into-md-windows-x86_64.zip` |

普通文档、Office 97–2003、PDF、OCR 和 Web 工作台均由 Core 提供。语音转写与说话人分离使用 `official.media.whisper-<target>.imp`；Agent 指令使用 `into-markdown-skill.zip`。macOS x86_64 不在支持范围内。

## 安装 Core

从 [正式发布页面](https://github.com/coolplayagent/into-markdown/releases) 下载对应 ZIP，核对发布资产的 SHA-256。审计 ZIP 包含发布文件清单、来源、许可证与构建证据；Core 内同时保留许可证材料。

macOS 示例：

```sh
shasum -a 256 into-md-macos-arm64.zip
unzip into-md-macos-arm64.zip -d into-md-core
./into-md-core/into-md version --json
```

Linux 选择与 `uname -m` 对应的 ZIP，使用 `sha256sum` 校验，解压后运行 `./into-md-core/into-md`。可将解压目录加入 PATH，后续使用 `into-md` 调用。

Windows PowerShell 示例：

```powershell
(Get-FileHash -Algorithm SHA256 .\into-md-windows-x86_64.zip).Hash
Expand-Archive .\into-md-windows-x86_64.zip .\into-md-core
& .\into-md-core\into-md.exe version --json
```

Windows 保持解压目录完整，包含 PDFium 等随包文件。unsigned 产物可能触发系统信任提示；先核对下载来源与摘要，再按系统提示允许运行。macOS unsigned 产物具有执行所需的 ad-hoc 签名，没有 Developer ID 公证。

升级时解压到新目录，结束旧版本转换任务后切换命令路径。转换配置和已安装语音插件保存在用户数据目录中；保留旧目录可供回退。

## 验证与能力安装

```sh
into-md version --json
into-md formats --json
into-md capabilities list --json
into-md doctor --json
into-md setup media
```

OCR 随 Core 提供，默认 `best-effort` 与 `auto`。`setup media` 是显式联网安装语音插件的操作；普通转换使用已安装能力。

## 离线部署

在联网机器下载并校验 Core ZIP；需要语音时同时下载相同版本、目标平台的 `.imp`，再复制到离线机器。解压 Core 即可进行文档转换与 OCR。

运行 `into-md ui`，在工作台插件管理中导入本地语音 `.imp`。官方包的摘要和发布者身份由 Core 内置目录核对；安装后通过 `into-md capabilities list --json` 检查。CLI 离线导入的发布者指纹参数见[插件管理](plugin-management.md)。

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

卸载便携 Core 时删除自己解压的安装目录，并移除自己添加的 PATH 项。用户配置、已安装插件、转换结果和单独复制的 Agent Skill 按需分别保留或删除。
