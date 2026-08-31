# Project agent instructions

- 设计时覆盖完整使用场景，不使用“第一版”“首版”“V1”等临时化表述。
- 完成代码或文档后，从整体产品与仓库边界复核改动；如果补丁只修局部而破坏整体一致性，继续简化或加固后再交付。
- UI 反馈遵循提示就近原则：校验、错误、状态与帮助信息应优先显示在触发它的控件或内容区域附近。只有真正跨区域、全局性的事件才使用全局提示；不要把局部提示放到页面角落或无关操作区。

## CI 变更硬性约束

- CI 只允许 `.github/workflows/pr-fast-gate.yml` 现有四个 job：Linux x86_64（共享测试与 Web）、Linux ARM64 Core、Windows x86_64 Core、macOS ARM64 Core；保持现有名称、runner、五分钟超时和 `pull_request` 触发方式。
- UT 可以加入这四个 job；共享 UT 优先加入 Linux x86_64，平台相关 UT 放入对应 job。耗时专项、完整构建、真实运行时矩阵通过本地命令按需验证，保持 fast gate 的时间边界。
- 禁止新增 CI workflow、job/task、matrix 平台或组合，以及手动、定时或其他自动 CI 触发入口；禁止通过脚本或复用工作流间接派发额外 CI。禁止删除、跳过或放宽 `tools/platform-release/ci_workflow_policy.py` 及其现有调用来容纳新任务。
- `.github/workflows/` 只保留 `pr-fast-gate.yml` 和仅手动触发的正式发布流程 `platform-modular-release.yml`。发布流程由用户明确要求发版或安装产物验收时调用，常规 PR 验证使用四个 fast job。
- 修改此白名单或发布触发边界必须先获得用户明确批准。交付前运行 `python3 -m unittest tools.platform-release.test_pr_fast_gate` 并整体检查工作流及调用脚本。
