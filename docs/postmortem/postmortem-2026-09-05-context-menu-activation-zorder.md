# 右键菜单激活与层级回归

## 症状

独立右键菜单弹窗出现时，主窗口投影短暂变暗或缩放；首次打开时菜单可能位于主窗口后方。由于菜单保持非激活，主窗口内部左键点击不会产生失焦事件，菜单无法按预期关闭。

## 根因

Slint/winit 在弹窗显示和样式更新时会重新调用原生窗口显示与样式接口。默认显示路径可能激活弹窗，导致 owner 收到失活/激活切换；仅设置 owner 和工具窗口样式不足以稳定行为。非激活弹窗还会绕过依赖 `Focused(false)` 的关闭路径。

## 修复

- 弹窗创建时使用 `with_active(false)`、owner、popup/tool-window 样式。
- 原生 HWND 子类拦截 `WM_MOUSEACTIVATE`、`WM_STYLECHANGING` 和 `WM_WINDOWPOSCHANGING`，持续保留非激活属性。
- 使用 `SW_SHOWNOACTIVATE` 显示，并在 owner 窗口内部左键按下时关闭菜单。
- 隐藏弹窗后再解除 DWM cloak，减少未就绪帧闪现。
- 子菜单关闭不再主动切换焦点。

## 验证

`python tools/verify.py --quick`、`cargo build`、`cargo test quick_menu_popup --quiet` 均通过；真实投影、主题和多窗口行为由用户手动验收。

## 排查入口

查看 `artifacts/logs/window-interaction-diagnostic.jsonl` 中的 `WM_ACTIVATE`、`quick_menu_native_show` 和菜单关闭事件，重点确认 owner 是否发生非预期激活往返。
