# AsterFiles 开发说明

这是 Windows 优先的 Rust + Slint 文件管理器。

## 启动与验证

```powershell
python tools/build.py setup # 首次安装固定 Rust 与 kache
python tools/build.py run --locked # 日常开发，自动执行缓存维护
python tools/verify.py --quick # 小改动：格式、Clippy、测试、Debug 构建
python tools/verify.py         # Issue 完整 Debug 验证
python tools/verify.py --release # 用户确认 Issue 后的本地 Release 构建验证；不发布
```

验证默认首个失败即停止，并在开始前关闭由本仓库 Debug/Release 程序启动的 AsterFiles；诊断全部失败时显式加 `--keep-going`。验证开始和结束时执行 `tools/build.py` 的构建缓存维护。`target` 预算 8 GiB，kache 数据预算 4 GiB，两者连同工具和索引的收尾总预算 13 GiB。工具链、依赖或构建配置变化时退休旧 `target`，保留最终程序；超预算同样清理整个旧输出，依赖从 kache 恢复。查看占用使用 `python tools/build.py status`，主动退休编译输出使用 `python tools/build.py clean`。具体边界见 `docs/agent/build-cache.md`。完整验证只构建一次 Debug，随后直接复用程序运行全部无界面场景。`--release` 仅在本机生成并验证 `target/release/asterfiles.exe`，不会更新版本、提交、打标签、推送或创建 GitHub Release；工作树内容未变化时复用最近成功的完整验证，只补本地 Release 构建。Issue 收尾使用 `./tools/finish-issue.ps1 <编号> -Message '<提交说明>' -Paths <本 Issue 文件>`，依次验证、提交、回写 Issue、设为 Done 并关闭；任一步失败立即停止，且不会带入未明确列出的改动。机器可读汇总位于 `artifacts/verify/summary.json`；详细规则和确定性 UI 场景见 `docs/agent/debug-validation.md`。UI 截图写入 `artifacts/ui/`，日志写入 `artifacts/logs/`，状态导出写入 `artifacts/state/`，性能 artifacts 写入 `artifacts/perf/`。

本地正式发布默认使用 `./tools/publish.ps1`：它读取上一个 GitHub Release 之后关闭且恰好带一个 `type: *` 标签的 Issue；只要包含 `type: feature` 就升级 feature 版本，否则升级 bugfix 版本，并把这些 Issue 按类型写入中文 Release Note。没有已完成 Issue 或 Issue 类型标签不合规时停止发布。确需人工覆盖时才使用 `./tools/publish.ps1 major|feature|bugfix`；`-DryRun` 仅预演。脚本负责递增 `Cargo.toml` 版本、验证、提交、将 Release Note 写入标签并原子推送。用户只要求“更新版本并让 GitHub Action 打 Release 包”时，运行脚本并确认其成功触发 Action 后立即结束，不等待 Action 构建完成，也不重复执行脚本已覆盖的检查；只有用户明确要求确认云端发布结果时才等待。发布包可使用 `./tools/release.ps1 -Tag v<版本>` 在本地生成，输出位于 `artifacts/release/`；GitHub Release 由 `.github/workflows/release.yml` 读取标签说明并发布。版本唯一来源是 `Cargo.toml`。

## Windows 持续验证（#106）

主线 push、面向 main 的 PR 和手动运行触发 `.github/workflows/ci.yml`，在 Windows Server 2025 / PowerShell 7 上运行完整 `python tools/verify.py`。本地、CI 和标签打包共同读取 `rust-toolchain.toml`；Rust 版本只在该文件维护。首次本地验证需安装 Python 3.13+、Rustup、Visual Studio C++/Windows SDK，运行 `python tools/build.py setup`，以及 `Install-Module Pester -RequiredVersion 4.10.1 -Scope CurrentUser -Force -SkipPublisherCheck`。验证显式导入该 Pester 版本；Cargo 检查、测试和构建均使用 `--locked`。

CI 只有仓库读取权限，不发布、不创建标签；同一 PR/引用的新运行取消旧运行，单次超时 60 分钟。日志、状态和汇总上传到 `windows-verification-<run-id>-<attempt>`，保存 14 天。成功与失败路径的云端复验命令及下载入口见 `docs/agent/debug-validation.md`；受控失败只修改 runner 的临时 checkout。

## 网络复制专项验证（#136）

`target/debug/asterfiles.exe --agent-network-copy-probe '<至少 128 MiB 的源文件路径>'` 运行真实父进程与网络辅助进程，无应用界面。探针只读取来源，在 `artifacts/state/issue-136/parent-probe/` 创建独占目标，验证实际暂停、保持 12 秒不增长、继续及暂停中取消，随后回收进程并清理目标。结果写入该目录的 `result.json`。请使用足够大的文件，避免传输在请求暂停前完成；来源不会被删除或修改。单元回归使用 `cargo test --locked issue_136 -- --nocapture`，真实界面仍由用户验收。

## HybridMount 慢源专项验证（#137）

`target/debug/asterfiles.exe --agent-network-copy-probe --stall` 验证未暂停且连续 15 秒无进度后仍完成复制，以及等待中取消在 3 秒内回收辅助进程。结果位于 `artifacts/state/issue-137/stall-probe/result.json`。

`target/debug/asterfiles.exe --agent-network-copy-probe --complete '<源文件路径>'` 通过真实父进程和辅助进程完整复制，再逐字节比较内容并清理测试副本。比较会再次读取来源；来源始终只读。进度、结果及错误位于 `artifacts/state/issue-137/complete-probe/`。受控中断恢复测试使用 `cargo test --locked robocopy -- --nocapture`，快照并发测试使用 `cargo test --locked issue_137 -- --nocapture`。受控测试不等于真实断开 NAS 网络。 Windows 已请求终止但退出事件未触发的回归使用 `cargo test --locked issue_137_pending -- --nocapture`；退出等待上限为 250 ms，旧暂存文件退休隔离。`artifacts/state/issue-137/disconnect-live/` 保存真实挂起现场的线程栈和进程等待信号；`network-copy-retired` / `network-copy-cleanup-deferred` 审计保存尚未完成系统回收的精确路径。

`target/debug/asterfiles.exe --agent-network-copy-probe --resume-complete '<至少 256 MiB 的源文件路径>'` 在真实传输达到 128 MiB 后暂停，确认后保持 2 秒，再继续到完整复制并逐字节比较。结果仍写入 `artifacts/state/issue-137/complete-probe/` 的独占运行目录；`network-copy-block-start` 审计记录恢复读取的位置。NAS→本地块续传回归使用 `cargo test --locked issue_137_block_download -- --nocapture`，槽位上限回归使用 `cargo test --locked issue_137_read_limit -- --nocapture`。恢复仅覆盖当前任务，不覆盖应用退出或崩溃。读取与元数据查询共用 4 个槽位，旧 Windows I/O 必须真正退出才释放槽位；槽位耗尽时仍可暂停取消。

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
- 每个开发任务完成后必须运行 `cargo build`，确保 `target/debug/asterfiles.exe` 已更新，供用户直接测试。日常本地开发不计算或报告 SHA-256、文件时间等构建指纹。只有用户明确要求正式构建、发布验证，或用户确认某个 issue 已完成时，才运行本地 Release 构建；本地 Release 构建不等于发布，回复中必须明确称为“本地 Release 构建”，不得简称“发布”。如果有正在运行的 AsterFiles 进程阻挡打包，直接关闭进程。
- 每个 issue 经用户明确确认完成后，必须在本地执行一次 Release 构建，产物为 `target/release/asterfiles.exe`；该动作不包含版本更新、提交、标签、推送或 GitHub Release。用户确认前不得因该规则提前执行本地 Release 构建。

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
