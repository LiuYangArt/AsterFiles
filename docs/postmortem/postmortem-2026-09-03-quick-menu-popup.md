# Issue #21 右键菜单附属窗口复盘

日期：2026-09-03

影响范围：Windows 快速菜单跨主窗口显示、首次呈现、子菜单窗口复用、Shell 子菜单异步投影

状态：用户已确认跨窗定位、无白闪和本轮子菜单修复看上去可用；Issue 仍按项目流程等待最终验收

## 摘要

Issue #21 把快速菜单从主窗口内部矩形迁移为独立的 Windows 附属弹窗，使菜单只避让目标显示器工作区，不再受主窗口客户区裁切。迁移过程中先后出现普通窗口边框、首次白闪、子菜单反复销毁重建、双重悬停高亮，以及 Shell 子菜单永久停在“正在加载”等问题。

最终方案不是把菜单改回系统 `HMENU`，也不是另写菜单内容模型，而是保留 #13 的菜单数据与命令身份，只替换承载层：根菜单和每层子菜单使用无边框 owner popup；窗口以最终样式隐藏创建，首帧完成前由 DWM cloak 隔离；子菜单按深度复用窗口槽；Shell 内容仍只按 Shell 自己的完整请求身份接收。

## 用户可见症状

- 菜单最初仍被主窗口边界限制，无法真正跨出主窗。
- 改为独立窗口后，外围短暂出现普通 Windows 标题栏、关闭按钮或系统边框。
- 深色菜单显示前先闪过白色窗口；Debug 尤其明显。
- 连续悬停多个子菜单时，子菜单窗口先关闭再创建，位置和内容切换笨重。
- “排序方式”与“刷新”等相邻行可能同时高亮。
- NanaZip 等 Shell 子菜单可能长期显示“正在加载 Windows 菜单…”，成功加载后重复打开仍不稳定。

## 根因

### 1. 窗口类型设置得太晚

只在 Slint 窗口创建后删除标题栏样式，Windows/DWM 已经有机会合成一次普通顶层窗口。关闭系统阴影只能改变阴影，不能阻止普通窗口首帧出现。

正确边界是：在底层窗口创建钩子中一次给出 owner、隐藏、无边框、普通层级、工具窗口和不进任务栏等属性；创建后只核验并补齐原生样式，不再把“普通窗转菜单窗”当成正常显示流程。

### 2. 隐藏创建不等于首帧已经有内容

窗口首次可见时才可能获得真正的合成表面。即使先隐藏创建、设置模型和位置，`show()` 与 Slint/Skia 第一帧之间仍存在窗口内容未定义的间隙，DWM 可能先合成白色表面。

最终用明确的呈现状态处理：`Hidden → Cloaked → ShownCloaked → Presented`。窗口先 cloak，再 show 并请求首帧；收到重绘边界、等待一次保守的合成间隔并 `DwmFlush` 后才解除 cloak。任一步失败、owner 销毁或会话关闭，都先解除 cloak 再隐藏，避免不可见窗口残留焦点。

### 3. 子菜单窗口生命周期绑错了内容生命周期

早期实现把每次 sibling hover 都视为新窗口：旧子菜单隐藏或销毁，再创建新窗口。这既放大首帧成本，也让焦点、DPI、位置与迟到结果更难保持一致。

最终按深度维护窗口槽：同一层替换分支身份、模型、尺寸和位置，HWND 保持不变；进入下一层才使用下一槽；浅层换分支时隐藏并失效全部后代。窗口身份与菜单内容身份分开管理。

### 4. 把弹窗状态误当成 Shell 请求身份

迁移后曾在 `SubmenuLoaded` 上增加“当前弹窗槽必须存在并匹配”的额外门槛。弹窗 generation 属于窗口链，Shell 的 `session_id + request_id + submenu_request_id + token` 属于 COM/HMENU 会话，两者不是同一代次。合法 Shell 结果可能被弹窗门槛拒绝，而加载标志只在接受结果后清除，于是永久停在加载状态。

修复后，Shell 结果只由 Shell 完整身份和当前 `WindowId + TabId + RequestId` 接受；弹窗槽只决定把已接受的模型显示在哪个窗口。非空动态子菜单在同一 Shell 会话内按原 token 缓存，空结果不作为成功缓存，保留后续重试机会。

### 5. 鼠标悬停与键盘活动项成为两个视觉真源

独立窗口迁移后，行背景同时依据 `TouchArea.has-hover` 和 Rust 投影的 `active-index` 着色。异步 hover timer 使旧活动项可能尚未更新，而鼠标已经进入新行，因此两行同时高亮。

修复后，每个弹窗列表只有一个本地 `hover-index`。存在鼠标悬停时它优先且抑制键盘活动项着色；鼠标离开后才恢复键盘活动项。Rust 的活动项继续负责键盘导航和命令身份，不再直接与鼠标状态竞争绘制。

## Firefox 参考与采用边界

Firefox/Gecko 的公开源码证明了两个成熟模式：

1. Windows popup 在 `CreateWindowExW` 之前就选定 popup 样式；`WindowType::Popup` 使用 `WS_POPUP`，扩展样式使用 `WS_EX_TOOLWINDOW`。普通 owner popup 只放在 owner 前方，不需要把所有菜单设为全局置顶。参考：[`nsWindow.cpp` 的窗口样式与层级](https://github.com/mozilla/gecko-dev/blob/5836a062726f715fda621338a17b51aff30d0a8c/widget/windows/nsWindow.cpp#L1308-L1349)、[`nsWindow.cpp` 的 popup 显示层级](https://github.com/mozilla/gecko-dev/blob/5836a062726f715fda621338a17b51aff30d0a8c/widget/windows/nsWindow.cpp#L1645-L1675)。
2. Firefox 对首次顶层窗口明确记录了“可见前没有 backing surface、未定义内容会白闪”的 Windows/DWM 问题。其做法是首次 show 前 cloak，窗口表面存在后先填入主题背景，再解除 cloak。参考：[`nsWindow.cpp` 的首次呈现说明与处理](https://github.com/mozilla/gecko-dev/blob/5836a062726f715fda621338a17b51aff30d0a8c/widget/windows/nsWindow.cpp#L1534-L1643)。

必须保留的事实边界：Firefox 菜单是 Gecko/XUL 自绘 popup，不是 AsterFiles 的 Slint 窗口；上述 cloak 代码在当前 Firefox 源码中针对首次顶层窗口，不是可直接复制的菜单专用实现。AsterFiles 采用的是经过真实白闪证据验证的同一呈现原则，并补上 Slint 重绘和菜单会话校验。Firefox 的 `MozillaDropShadowWindowClass` 只是类名；当前注册代码没有添加 `CS_DROPSHADOW`，不能据此推断应该开启 Windows 菜单阴影。

Firefox 的 popup 布局层也把锚点、屏幕翻转和窗口承载分开，并在已有 popup view 上重算位置；这支持了 AsterFiles“内容替换/重定位，不因 sibling hover 重建窗口”的方向。AsterFiles 没有照搬 Gecko 的 XUL 状态机，而是采用更小的按深度窗口槽模型。

## 最终架构边界

- `QuickMenuState`、`ContextCommandRow`、Shell token 和原始 command ID 仍是 #13 的业务模型。
- `context-menu-open` 仍是统一会话真值；独立窗口不另建菜单内容真源。
- 根菜单和子菜单使用 owner popup，保持 `WS_POPUP + WS_EX_TOOLWINDOW`，不进入任务栏/Alt+Tab，不设全局置顶。
- 点击点转换为物理屏幕坐标；定位只约束目标显示器 `rcWork`，不避让主窗口。
- 根菜单右下优先并左右/上下翻转；子菜单右侧优先、空间不足左翻。
- Shell 枚举仍按 HMENU position 原序投影；不排序、不去重、不按标题特殊处理第三方扩展。
- `Loaded` 与 `SubmenuLoaded` 保持分层异步替换；父项不充当叶子命令。
- owner 销毁、导航/request 失效、外部失焦、执行命令和根关闭统一失效会话并隐藏整条窗口链。

## 未奏效或代价过高的路径

- 只关闭 Windows 阴影：不能解决普通窗口首帧和白色未定义表面。
- 创建后再切 `WS_POPUP`：存在系统先展示默认框架的竞争窗口。
- 仅依赖 `show()` 后的零延迟 timer：没有证明 Slint 首帧和 DWM 合成已经完成。
- 每次 hover 创建/销毁子菜单窗口：把内容变化升级为 HWND 生命周期变化。
- 用弹窗 generation 审核 Shell 内容：混合两个独立状态机，会误丢合法结果。
- 同时以鼠标 hover 与 active-index 绘制高亮：产生双真源。

## 验证与证据

- 纯逻辑测试覆盖负坐标、多屏工作区、左右/上下翻转、限高、分支替换、后代失效、owner/request 失效和重复 close-all。
- 回归测试覆盖 HMENU 原序、submenu token、叶子 command ID、Shell 完整身份拒绝迟到结果，以及非空子菜单缓存。
- `python tools/verify.py` 覆盖格式、Clippy、完整测试、无界面 Agent 场景和 Debug 构建；汇总位于 `artifacts/verify/summary.json`。
- 用户真实桌面验证确认菜单可跨主窗口、位置正确、白闪消失；最后一轮确认双高亮和永久加载修复看上去可用。
- 项目约束禁止 Codex 自动操作 AsterFiles UI；真实焦点、IME、Alt+Tab、多屏 DPI 和第三方 Shell 扩展仍由用户人工验收。

## 防复发规则

1. Win32 popup 的 owner、可见性、窗口类型、任务栏和层级属性必须在创建时确定；不得先创建普通窗口再改造成 popup。
2. 深色自绘窗口的“隐藏创建”不代表首帧安全；必须明确首帧表面何时可合成，并为失败路径解除 cloak。
3. 同层菜单内容变化只替换模型和位置，不重建 HWND；窗口池按深度管理，浅层换分支统一失效后代。
4. 窗口会话、Shell 会话、导航请求是三套身份；各自只审核所属结果，不把不同代次强行比较。
5. 异步结果若被拒绝，必须确认当前加载状态属于旧请求还是新请求；不能留下无人清理的 loading。
6. 每个列表只能有一个视觉悬停真源；键盘活动项与鼠标悬停必须有明确优先级。
7. 子菜单成功缓存只在所属 Shell 会话内有效；保留原 token 和命令 ID，动态空结果不得永久缓存。
8. 自动几何和状态测试不能代替真实 Windows 首帧、焦点、DPI 与第三方扩展验收。

## 长期排查入口

- 菜单业务、窗口协调和异步接收：`src/app.rs`
- popup 几何与窗口链状态：`src/quick_menu_popup.rs`
- Windows owner、样式、工作区、cloak 和焦点：`src/platform/windows/quick_menu_window.rs`
- 独立菜单视觉与鼠标/键盘状态：`ui/quick-menu.slint`
- Shell HMENU 枚举与动态子菜单：`src/platform/windows/context_menu.rs`
- 长期设计边界：`docs/windows-shell-menu-feature.md`
- 自动验证汇总：`artifacts/verify/summary.json`