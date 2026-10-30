# 地址栏外部点击退出遗漏（#177）

## 症状

首轮修复后，用户确认 Enter 已能退出编辑。但点击文件列表、侧栏和空白区仍保留编辑框。

## 根因

主窗口的菜单清理入口匹配所有左键按下，即使菜单没有打开也会执行，然后返回并继续把点击交给 Slint。首轮新增的地址栏处理放在这个返回之后，实际收不到左键事件。

列表、侧栏和空白区主要使用 TouchArea，点击不主动转移键盘焦点。因此，编辑器失焦回调也不能代替外部按下处理。

首轮测试直接调用地址栏边界检查，验证了“收到点击位置后能收起”，没有经过真实点击前的菜单清理入口。测试通过不能证明生产事件能走到那里。

## 修复

把地址栏外部按下和菜单关闭判定放入同一个前置入口。先收起地址栏，再决定是否进入菜单清理。菜单清理和点击传播规则保持不变，不增加覆盖页面的点击遮罩。

## 回归与证据

新增无窗口测试以原生左键事件进入生产共用入口，再向 Slint 的真实列表、侧栏和空白控件派发点击。检查收起编辑框，并分别检查首次选择、侧栏导航和框选起点。旧顺序下，三个测试均在收起检查处失败。

命令：`python tools/build.py test --locked issue_177 -- --nocapture`，随后运行 `python tools/verify.py --quick`。修复前日志为 `artifacts/logs/issue-177/outside-before-fix.log`；修复后专项和统一日志为 `outside-after-fix.log`、`outside-quick.log`。机器结果位于 `artifacts/state/issue-177/`。

真实窗口的首次点击效果由用户复验。用户确认前，#177 保持打开。

## 后续排查入口

先看 `src/app.rs` 中的 `prepare_context_menu_event` 和 `should_close_context_menu`，确认按下处理发生在提前返回前。再看 `src/app/address_edit/tests.rs` 的 `issue_177_native_` 测试。仅检查 Slint 边界判定或焦点回调不足以覆盖原生事件顺序。
