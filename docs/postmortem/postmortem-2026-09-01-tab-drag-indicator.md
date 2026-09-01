# 标签拖动插入指示器实现复盘

日期：2026-09-01
影响版本：`feat: show tab insertion indicator` 至 `fix: render native tab insertion indicator`
修复提交：63fe346
状态：已修复，用户已完成真实窗口确认

## 摘要

为标签拖动添加 Firefox 风格的插入指示器时，经历了三轮实现：先用 Slint Rectangle 覆盖层，再请求重绘，最后用独立原生窗口。自动测试均通过，但用户两次在真实窗口中看不到指示器。

核心教训：Windows `DoDragDrop` 处于模态 OLE 拖动循环时，会阻塞 winit/Slint 的主事件循环绘制，导致 UI 属性更新无法及时渲染。

## 用户可见症状

- 拖动标签越过阈值后，来源标签隐藏，拖动卡片跟随鼠标，但落点位置没有小竖条。
- 同窗排序和跨窗口拖入都看不到指示器。
- 普通点击、关闭、中键关闭未受影响。

## 根因

### 1. 错误假设 Slint 属性更新会即时渲染

第一版在 Slint 场景里增加一个 `Rectangle`，用 `tab-drag-insertion-index` / `external-tab-dragging` 控制显隐，并调用 `request_redraw()`。

问题：当 Windows `DoDragDrop` 进入 OLE 拖动循环后，它会接管鼠标和消息分发，winit 事件循环暂停；`request_redraw()` 只是排入队列，实际绘制被推迟到拖动结束后，因此拖动期间看不到任何变化。

### 2. 错误地用重绘请求绕过阻塞

第二版改为每次落点变化都遍历所有窗口调用 `request_redraw()`，希望强制 Slint 绘制。

问题：OLE 模态循环未退出前，底层窗口消息泵不在主线程执行；重绘请求无法兑现。用户仍看不到指示器。

### 3. 临时状态属性残留在 Slint 中

Slint 里留下 `external-tab-dragging`、`external-tab-insertion-index`、`tab-drag-insertion-index` 和对应回调；Rust 侧 `project_cross_window_drop` 也保留对 Slint 回调的调用。这些临时实现需要在最终方案后清理。

### 4. 对 HWND 拖动循环生命周期理解不足

`DoDragDrop` 在 `ole32` 内部有自己的消息循环，主线程被占用。任何依赖主事件循环的视觉反馈（Slint、winit、普通 GDI 渲染）都会被延迟，除非使用独立的原生 HWND 并自己处理窗口过程。

## 修复

- 不再依赖 Slint 绘制指示器。
- 新增独立的 `NativeInsertionIndicator` 原生窗口：
  - `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST`
  - 不接收输入、不抢焦点、不注册 `IDropTarget`。
  - 使用 `UpdateLayeredWindow` 用纯 GDI 绘制 `5px` 宽、标签栏高度、强调色、圆角的竖条。
- 拖动落点更新时，直接按目标窗口标签栏物理坐标定位指示器窗口，独立于 Slint 重绘循环。
- 离开目标窗口、Escape、无效释放、目标窗口关闭、拖动完成/取消时隐藏并销毁。
- 同窗排序与跨窗口移动复用同一指示器逻辑。
- 清理所有 Slint 侧临时状态属性、Rectangle 覆盖层和回调。
- 单标签窗口从状态层直接拒绝拖动，避免无意义的“拖出创建新窗口”。

## 回归证据

- 用户真实窗口确认同窗排序和跨窗口拖入均能看到指示器。
- 自动测试：
  - 插槽计算：首/尾、中点边界、5px 缝隙、左右滚动、栏外、设置标签前后区间。
  - 拖动状态机：阈值前不排序、阈值后进入拖动、释放后提交、取消后恢复。
  - 跨窗口：目标命中、来源保留、事务提交/回滚、100%→150% DPI 缩放。
  - 单标签拒绝拖动。
- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --no-fail-fast`、`python tools/verify.py` 全部通过。
- Debug 构建产物：`target/debug/asterfiles.exe`，SHA-256 在提交信息中。
- 未构建 Release（按项目规则等待用户确认 issue 完成后才构建）。

## 防复发规则

1. 只要涉及 `DoDragDrop`、模态对话框或平台级拖动循环，就不能假设 Slint/winit 能在拖动期间绘制。
2. 拖动过程中的视觉反馈应使用独立原生 HWND / 平台 API，或完全在拖动循环结束后统一显示；不得依赖主事件循环。
3. 临时 Slint 属性和回调必须在方案确定后立即清理，避免留下幽灵状态。
4. DPI 和滚动偏移必须在状态层用纯函数计算并单独测试，不要在 UI 代码里用简单公式推导。
5. 跨窗口拖动必须使用物理屏幕坐标 + 目标窗口客户区命中，不能依赖来源窗口的本地坐标。

## 长期排查入口

- 拖动状态机：`src/app.rs` 中的 `TabDragSession` 及相关回调。
- 原生拖动卡片与指示器：`src/platform/windows/tab_drag_indicator.rs`。
- 插槽计算：`compute_tab_insertion_slot` / `hit_test_tab_bar`。
- Slint 标签栏：`ui/app-window.slint`。
- 自动验证汇总：`artifacts/verify/summary.json`。
- 相关提交：`2157f17`、`ace85b4`、`63fe346`。
