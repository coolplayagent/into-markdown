# Agent Skill 发布与安装

Into Markdown 以开放 Agent Skills 目录格式发布 `into-markdown` skill。它指导 agent 调用已安装的
`into-md` 完成文档、图片、完整音视频、stdin、目录和明确授权远程来源的转换，并校验真实产物。
它不安装产品、不管理能力插件或 Provider，也不打开本地 Web 管理界面。

目录结构遵循 [OpenAI Agent Skills 文档](https://learn.chatgpt.com/docs/build-skills)。
`agents/openai.yaml` 只增强 Codex 中的显示、默认提示与隐式匹配；不声明 MCP 依赖，也不改变
其他兼容 agent 读取 `SKILL.md` 的方式。

canonical 源位于 `.agents/skills/into-markdown/`。每次产品发布从该目录生成同一份内容的两个交付面：

- 平台无关的 `into-markdown-skill.zip` 与 `into-markdown-skill.zip.sha256`；
- 每个平台 Core 内的 `share/into-markdown/skills/into-markdown/`。

ZIP 固定以 `into-markdown/` 为根目录，条目顺序、时间戳和权限保持确定性。Core 的
`archive-manifest.json` 绑定内置副本的每个文件；独立 ZIP 与 Core 副本必须逐文件相同。

## 显式安装

产品安装器不会修改任何 agent 配置或 skill 目录。使用独立 ZIP 安装到 Codex 用户目录：

```sh
shasum -a 256 -c into-markdown-skill.zip.sha256
mkdir -p "$HOME/.agents/skills"
unzip into-markdown-skill.zip -d "$HOME/.agents/skills"
```

也可以从 Unix Core 安装目录创建显式链接：

```sh
mkdir -p "$HOME/.agents/skills"
ln -s \
  "$HOME/.local/share/into-markdown/current/share/into-markdown/skills/into-markdown" \
  "$HOME/.agents/skills/into-markdown"
```

Windows PowerShell 可把独立 ZIP 解压到用户 skill 目录：

```powershell
New-Item -ItemType Directory -Force "$HOME\.agents\skills" | Out-Null
Expand-Archive -LiteralPath .\into-markdown-skill.zip -DestinationPath "$HOME\.agents\skills"
```

其他兼容 Agent Skills 的产品应使用各自声明的发现目录，不应改写 skill 内容。Codex 可以通过
`$into-markdown` 显式调用，也可以在匹配的文件转换任务中自动选择它。Codex 的用户级发现目录
与符号链接支持见同一份[官方本地 skill 文档](https://learn.chatgpt.com/docs/build-skills)。

删除 Core 不会删除用户自行复制的 skill，也不会删除指向 Core 的链接。用户应在不再需要时只移除
自己创建的精确 `into-markdown` 目录或链接。

## 构建与验证

仓库内发布工具不依赖平台运行时：

```sh
python3 tools/skill-release/skill_release_main.py validate
python3 tools/skill-release/skill_release_main.py build \
  --archive /absolute/into-markdown-skill.zip
python3 tools/skill-release/skill_release_main.py verify \
  --archive /absolute/into-markdown-skill.zip
```

发布 CI 连续生成两次并逐字节比较 ZIP，然后重新读取所有条目、权限和 canonical 字节。三平台
Core 装配器复用同一 materializer；安装后 smoke 还会检查 skill 的精确文件集合、入口、元数据和
许可证。skill 随产品发布同步演进，不维护独立版本号。

## English summary

The release publishes one portable `into-markdown-skill.zip` and embeds the identical canonical
directory in every Core package. Installation into an agent's discovery directory is always an
explicit user action. The product installer and uninstaller never modify `$HOME/.agents/skills`.
The skill runs the installed CLI for conversion and read-only diagnostics; it does not administer
plugins, providers, models, configuration, or the Web UI.
