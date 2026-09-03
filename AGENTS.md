# AsterFiles 开发说明

这是 Windows 优先的 Rust + Slint 文件管理器。

## 启动与验证

```powershell
cargo run
python tools/verify.py
```

日常启动使用 `cargo run`，统一验证覆盖格式、静态检查、测试、无界面 Agent 场景和 Debug 构建。机器可读汇总位于 `artifacts/verify/summary.json`；详细规则和确定性 UI 场景见 `docs/agent/debug-validation.md`。Debug 构建产物位于 `target/debug/`；只有用户明确要求正式构建时才运行 `python tools/verify.py --release` 或 `cargo build --release`。UI 截图写入 `artifacts/ui/`，日志写入 `artifacts/logs/`，状态导出写入 `artifacts/state/`，性能 artifacts 写入 `artifacts/perf/`。

本地正式发布使用 `./tools/publish.ps1 major|feature|bugfix`，它会递增 `Cargo.toml` 版本、验证、提交、打标签并原子推送；`-DryRun` 仅预演。发布包可使用 `./tools/release.ps1 -Tag v<版本>` 在本地生成，输出位于 `artifacts/release/`；GitHub Release 由 `.github/workflows/release.yml` 在推送版本标签或手动触发时创建。版本唯一来源是 `Cargo.toml`。

## UI 操作与验证

- 禁止 Codex 操作、自动化或尝试控制 AsterFiles 的 UI，包括通过内置浏览器、Chrome、Computer Use、Playwright、agent-browser、截图点击或键鼠模拟等方式。
- 不得为排查或验证而启动交互式 UI 操作流程；允许编译、自动化测试、无界面场景、日志和状态导出等非 UI 验证。
- 需要确认视觉效果、交互行为或真实桌面窗口能力时，必须停止 UI 验证，向用户说明需要验证的内容，并提供简短、明确的手动验证步骤，由用户执行并反馈结果。
- 不得因 UI 验证失败或工具不稳定而反复尝试其他 UI 工具。

## 项目约束

- 不在 UI 线程读取目录、提取缩略图或执行文件操作。
- UI 只依赖应用协调层给出的模型，不直接调用 Windows API。
- 窗口级快捷键统一在 winit 窗口事件入口处理；Slint 焦点域只处理地址栏等控件内输入，不能单独承担全局快捷键。
- Windows Shell/COM 代码放在独立的 `platform/windows` 模块，后续不得散落到 UI。
- 大目录必须增量加载、可取消、严格虚拟化；不要一次创建十万个 UI 节点。
- 不增加网络协议、插件或索引服务，除非当前里程碑明确需要。
- 注释说明目的、性能约束或 Windows 平台决策，不复述代码。
- 修改完成后至少运行与变更范围相符的最小验证，并保留可读取的错误输出。
- 每个开发任务完成后必须运行 `cargo build`，确保 `target/debug/asterfiles.exe` 已更新，供用户直接测试；交付时报告 Debug 文件时间与 SHA-256。只有用户明确要求正式构建、发布验证，或用户确认某个 issue 已完成时，才运行 Release 构建并报告其时间与 SHA-256。
- 每个 issue 经用户明确确认完成后，必须构建一版 Release；用户确认前不得因该规则提前构建 Release。

## Issue 与任务状态

- GitHub Issues 是任务范围、验收条件与完成状态的唯一来源；GitHub Project `AsterFiles Development` 管理实施状态和顺序。不得新增本地任务清单或在设计文档中复制任务状态。
- 开始开发前确认 Issue 恰好有一个 `type: *` 和一个 `priority: P0–P3` 标签，并包含明确范围、非目标、验收条件和验证方式。信息不足时先完善 Issue。
- 实施状态使用 Project：`Backlog`、`Ready`、`In progress`、`In review`、`Blocked`、`Done`。需要用户真实 UI 验收时进入 `In review`，用户确认前不得关闭 Issue。
- 提交、方案文档和验证证据使用 `#编号` 关联 Issue；验证结果、artifact 路径和剩余风险回写 Issue。范围外问题另建 Issue，不扩大当前任务。
- 设计文档只维护仍有效的架构与产品边界；任务完成后更新受影响的设计文档，不维护第二份勾选状态。
- 涉及路径、后台加载、多标签页、本地化或网络边界时，同时遵守并更新 `docs/foundation-plan.md`。
- 除非用户显式要求，否则不新建 worktree/分支。在主线完成工作。

## 架构红线

- 文件身份始终保留为 Rust 的原始路径或稳定 ID；展示字符串不得反向承担打开、重命名等操作身份。
- UI 线程禁止执行 `exists`、`is_dir`、目录枚举、元数据读取、Shell/COM 或网络探测。
- 所有目录加载携带 `TabId + RequestId`；只有对应标签的最新请求可以更新页面。
- 新导航、关闭标签和退出必须取消旧任务；慢任务不得占住全局唯一工作线程。
- 目录和网络结果采用分批提交；不得等待完整列表后才显示首批内容。
- 用户可见文案进入语言资源；新增硬编码文案需在当前切片内迁移。

## 参考项目

UI/交互参考 Files， 源码： F:\CodeProjects\Files
网络部分参考WinSCP https://github.com/LiuYangArt/winscp
