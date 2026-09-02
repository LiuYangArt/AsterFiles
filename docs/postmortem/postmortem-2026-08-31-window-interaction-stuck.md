# Issue #2 窗口移动与缩放失效复盘

日期：2026-08-31

影响范围：Windows 无边框主窗口

状态：已修复并由用户确认

## 摘要

主窗口偶发无法移动和缩放，但内部菜单、列表和滚动条仍然正常。最初只有低概率症状，没有可靠复现步骤。第一次处理在没有消息证据时替换了整套移动与缩放实现，引入缩放期间客户区不能及时重绘、露出大块背景的严重回归，因此撤回。之后保留原行为，仅增加可开关诊断日志；真实日志最终证明触发点是最大化窗口仍命中缩放边缘。

## 用户可见症状

- 标题栏不能拖动窗口。
- 四边和四角不能调整窗口大小。
- 窗口内普通交互完全正常。
- 重启应用后恢复。

## 证据与根因

正常移动或缩放的 Windows 消息序列为：

`WM_NCLBUTTONDOWN → WM_ENTERSIZEMOVE → WM_EXITSIZEMOVE`

故障日志中，最大化后点击顶部边缘产生了 `WM_NCLBUTTONDOWN(HTTOP)`，但 Windows 因最大化窗口不能缩放而没有进入系统缩放循环，因此后续没有 `WM_ENTERSIZEMOVE` 和 `WM_EXITSIZEMOVE`。

winit 0.30.13 在发送非客户区按下消息前先把内部 `dragging` 标记设为真，并且只在收到 `WM_EXITSIZEMOVE` 时清除。该次无效缩放没有结束消息，标记永久残留；后续移动和缩放共用这一个标记，所以都被直接忽略。内部控件不依赖该状态，因此仍然可用。

## 修复

- 最大化时把主窗口缩放边缘宽度设为零，避免向 winit 发出 Windows 必然拒绝的缩放请求。
- 还原后自动恢复原有 14px 缩放边缘。
- 保留 Slint/winit 原有的异步系统缩放链路，不替换命中层，不改变实时重绘行为。
- 保留开发工具中的诊断入口，供未来窗口消息异常继续取证。

## 失败方案及教训

首次方案用自定义 UI 命中区和同步 `SendMessageW(WM_NCLBUTTONDOWN)` 全面替换 Slint/winit 行为。它规避了 winit 的持久状态，却改变了 Windows 模态缩放与渲染更新的时序，导致拖动缩放时客户区落后于窗口边界。

本次最重要的教训是：窗口系统问题不能仅凭源码中的可疑状态就重写整条交互链。必须先取得故障前后的系统消息配对，确认哪个请求没有进入系统循环，再在最靠近无效输入的地方阻止它。

## 回归验证

- `cargo fmt --check`：通过。
- `cargo clippy --all-targets --all-features -- -D warnings`：通过。
- `cargo test`：133 项通过。
- `python tools/verify.py`：通过，汇总位于 `artifacts/verify/summary.json`。
- Debug 构建通过。
- Release 首次构建发现旧版程序仍在运行并占用目标文件；关闭该进程后 `cargo build --release` 通过。
- 用户在真实 Windows 窗口确认修复可用并同意完成 Issue。

## 防复发规则

1. 最大化、全屏和固定尺寸窗口不得发起边缘缩放。
2. 无边框窗口的移动/缩放故障先记录请求和 `WM_NCLBUTTONDOWN / WM_ENTERSIZEMOVE / WM_EXITSIZEMOVE`，不先替换系统交互链。
3. 修复优先阻止无效请求，避免复制 Slint/winit 已经承担的命中、捕获和渲染时序。
4. 自动测试不能证明真实 Windows 模态移动与缩放；完成状态必须包含用户手动验证。
5. 验证缩放时必须观察客户区是否逐帧重绘，不能只确认最终窗口大小正确。

## 长期排查入口

- 窗口声明与最大化状态：`ui/app-window.slint`
- 标题栏移动入口：`src/app.rs`
- Windows 消息诊断：`src/platform/windows/window_trace.rs`
- 诊断日志：`artifacts/logs/window-interaction-diagnostic.jsonl`
- 任务记录：GitHub Issue #2
