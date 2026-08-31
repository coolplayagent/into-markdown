# 参与 Into Markdown 开发

[English](CONTRIBUTING.en.md)

感谢改进 Into Markdown。仓库维护本地优先的转换 Core、两个完整能力插件、Agent Skill、
安装器和四个平台的发布权威。变更必须保持统一 IR、默认离线、显式授权和可复现发布边界，
而不能只让某一条入口看起来可用。

## 开始之前

- 先搜索现有 issue；一个 issue 对应一个聚焦的 PR，并在 PR 正文中关联它。
- 安全漏洞不要公开提交利用细节；按仓库 Security 页面提供的私密渠道报告。
- 不提交客户文档、凭据、私钥、下载缓存、生成模型、能力运行时或本机测试产物。
- 新依赖、来源、模型或 runtime 必须同时更新 lockfile、来源权威、许可清单、SBOM 和发布
  inventory；不能以 README 声明代替机器校验。

## 产品与接口约束

- 转换先生成受验证的 Document IR，再由公共 renderer 生成 Markdown。CLI、Web、插件和
  Provider 不能各自维护另一套语义结果。
- 本地文件默认离线；远程来源和 Provider 只在当前调用显式授权后访问网络，私网需要独立授权。
- Office 97–2003 解析内置于 Core。OCR 与语音以包含运行时、模型、许可和 SBOM 的完整
  能力插件发布，并以插件为单位安装、更新和验证。
- 公共命令、DTO、错误码、格式 catalog、插件/能力 ID 和发布路径的改动需要兼容性说明、测试和
  中英文文档同步更新。
- 安全失败应返回稳定、可解析的错误，不得 panic、静默降级或把不完整结果伪装为成功。

## 构建和测试

Bazel 是发布构建权威；Cargo 用于快速反馈和 crate 定向测试。先运行与改动最接近的门禁，
再运行受影响的上层契约：

```sh
bazel build //...
bazel test //...
cargo fmt --all -- --check
cargo check --workspace --locked
```

格式或转换改动必须使用真实结构的文档、图片或媒体，并断言 IR、Markdown、资源、诊断与
provenance；不能使用改扩展名、随机字节、静音音频或只检查编译的替代品。原生 runtime 和
能力插件结论必须来自全新安装发布件的黑盒测试。详细门禁见
[`docs/testing.md`](docs/testing.md) 与 [`docs/installed-smoke.md`](docs/installed-smoke.md)。

文档改动运行可执行文档契约：

```sh
bazel test //tools/docs-check:docs_check_test
```

该测试从真实 CLI 发现公共命令和当前格式 catalog，检查中英文示例覆盖、命令语法、本地链接，
并执行真实 TXT、stdin 和每种可用格式的 dry-run。新增或修改公共命令、格式时，必须同步更新
[`docs/cli-examples.md`](docs/cli-examples.md) 和对应英文文档。

## CI 变更约束

- CI 只允许 `.github/workflows/pr-fast-gate.yml` 现有四个 job：Linux x86_64
  （共享测试与 Web）、Linux ARM64 Core、Windows x86_64 Core、macOS ARM64 Core。
- 保持四个 job 的名称、runner、五分钟超时和 `pull_request` 触发方式。UT 可以加入现有
  job；共享 UT 优先放入 Linux x86_64，平台相关 UT 放入对应 job。耗时专项、完整构建和
  真实运行时矩阵通过本地命令按需验证，保持 fast gate 的时间边界。
- 禁止新增 workflow、job/task、matrix 平台或组合，以及手动、定时或其他自动 CI 触发入口；
  禁止通过脚本或复用工作流间接派发额外 CI。禁止删除、跳过或放宽白名单校验来容纳新任务。
- 工作流目录只保留 `pr-fast-gate.yml` 和仅手动触发的正式发布流程
  `platform-modular-release.yml`。发布流程由用户明确要求发版或安装产物验收时调用。
- 四个 job 已有的 `pr_fast_gate.py` 调用会执行 `ci_workflow_policy.py`，拒绝额外工作流、
  job、矩阵、runner/name 漂移及发布自动触发。校验使用固定的 YAML 块布局，调整 UT 时
  保持工作流控制结构。修改白名单或发布触发边界须先获得用户明确批准；交付前运行
  `python3 -m unittest tools.platform-release.test_pr_fast_gate`，并整体检查工作流及调用脚本。

## 插件与发布改动

进程插件和 WASI 插件分别遵守 `process-v1` 与 `wasi-v1` 的隔离契约；开发、签名和生命周期
门禁见 [`docs/plugin-development.md`](docs/plugin-development.md)。发布脚本必须从单一 canonical
来源装配各平台产物，固定清单、摘要、权限和签名，并在解包后重新验证。不要在平台脚本中复制
一份实现，也不要修改用户的 agent skill 或配置目录。

## PR 交付标准

PR 应说明用户可见结果、安全与兼容性影响、执行过的测试及不能在本地执行的精确门禁。提交前
从全局复核 README、权威文档、CLI/Web 行为、安装与卸载、四平台发布和供应链证据是否一致；
删除临时文件和调试开关，并确认 `git diff --check` 通过。
