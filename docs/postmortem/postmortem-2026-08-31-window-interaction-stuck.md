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

## 2026-09-23 复发（#126）

最大化时把缩放边宽设为 0 之后，症状仍会出现一次。Slint 把缩放方向记在最近一次鼠标移动上，按下时不再重算。光标停在边缘时用 Win+↑ 或拖到屏幕顶部最大化，下一次左键仍会调用 `drag_resize_window`。Windows 拒绝后同样没有 `WM_EXITSIZEMOVE`。

补救放在窗口子类，不替换 Slint/winit 的命中和模态循环：`WM_NCLBUTTONDOWN` 返回时如果没有看到 `WM_ENTERSIZEMOVE`，补发 `WM_EXITSIZEMOVE`，让 winit 清除 `dragging`。已经进入循环的请求不补发。

## 2026-09-24 跨屏缩放与定位冲突（#129）

后续跨屏问题涉及两条混合 DPI 边界（125%/150%、150%/100%）。#128 为阻止连乘，在整段标题栏拖动的 `WM_WINDOWPOSCHANGING` 中覆盖外框宽高并清除 `SWP_NOSIZE`；此时 winit 已按另一套尺寸计算抓取点和位置，后置改宽高破坏了两者的一致性。原基准与恢复帧还按线程共享，缺少窗口隔离。

首次改为 `InnerSizeWriter` 交接客户区尺寸后，2026-09-25 用户实际拖动仍出现持续变大。日志 PID 51732 / HWND 4067692 中，1915×1207@144 先正确变为 1277×805@96；Windows 建议 x=3274，但 winit 把 x 改为 3201，把窗口推回旧屏并立即触发 96→144 的反向 DPI。回到 1915×1207 后，下一个系统移动帧又放大为 2873×1811。原适配把这条迟到的普通尺寸通知误当成恢复尺寸，下一次继续连乘。证据摘录位于 `artifacts/state/window-assessment/issue-129-growth-excerpt.jsonl`；不是仅靠比例单元测试能发现的问题。

源码对应 winit 0.30.13 的 `WM_DPICHANGED`：其定位修正用尚未移动的 HWND 所在屏幕校正新矩形。此次改为复用 Windows 的完整建议矩形：无边框正常窗口在 `WM_GETDPISCALEDSIZE` 提供固定基准计算的外框；仅在原生 DPI 消息栈内一次性恢复 winit 请求的位置和尺寸，再让 winit/Slint 保持缩放通知与重绘。删除 `InnerSizeWriter`、缓冲事件队列与尺寸回调纠正；普通移动不持续改写。基准只允许在首次 DPI 查询前接受恢复尺寸，之后冻结到松键。嵌套栈与基准按 HWND 隔离。

该方案遵循微软的 [WM_GETDPISCALEDSIZE 协议](https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-getdpiscaledsize)，其目标是让 Windows 在跨屏往返时保持鼠标与窗口位置关系。`WM_WINDOWPOSCHANGING` 的一次适配限定在已核对的 winit DPI 请求标志内；升级依赖时复审，不能推广成整段拖动的宽高钳制。现场额外放大的调用来源不能仅凭消息标志归罪第三方吸附模块。

另外，锁定的 winit 0.30.13 把 POINTS 地址当成 `WM_NCLBUTTONDOWN` 坐标。适配层在系统入口重新取得并打包屏幕坐标，保留 winit 自身的 `dragging` 与合成松键；直接绕过其入口会漏掉该清理行为，因此已删除项目的重复拖动实现。无效请求恢复保留，并改为按 HWND 隔离。

原生适配必须在 Slint 实际收到窗口事件、取得有效 HWND 后安装，不能在组件初始化时捕获 0 后永久沿用；每个窗口的事件入口记录成功安装的句柄，销毁时清除。

窗口日志改为有界后台写入，记录 DPI 建议、尺寸交接、前后几何和丢失计数，避免逐条刷盘干扰手感。自动回归以 `python tools/verify.py` 为入口，真实跨屏与最大化恢复按 `docs/agent/debug-validation.md` 的 #129 步骤由用户验收。Windows 恢复先后顺序、第三方吸附与真实渲染延迟仍须从现场日志区分，不能由纯状态测试替代。
## 防复发规则

1. 最大化、全屏和固定尺寸窗口不得发起边缘缩放。最大化前缓存的缩放方向也算一次发起。
2. 无边框窗口的移动/缩放故障先记录请求和 `WM_NCLBUTTONDOWN / WM_ENTERSIZEMOVE / WM_EXITSIZEMOVE`，不先替换系统交互链。
3. 修复优先阻止无效请求；系统未进入循环时，只补系统本来会发送的 `WM_EXITSIZEMOVE`。
4. 自动测试不能证明真实 Windows 模态移动与缩放；完成状态必须包含用户手动验证。
5. 验证缩放时必须观察客户区是否逐帧重绘，不能只确认最终窗口大小正确。

## 长期排查入口

- 窗口声明与最大化状态：`ui/app-window.slint`
- 标题栏移动入口：`src/app.rs`
- 卡住恢复：`src/platform/windows/window_drag_recovery.rs`
- Windows 消息诊断：`src/platform/windows/window_trace.rs`
- 诊断日志：`artifacts/logs/window-interaction-diagnostic.jsonl`
- 任务记录：GitHub Issue #2
