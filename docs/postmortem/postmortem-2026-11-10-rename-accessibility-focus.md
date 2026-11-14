# 改名框创建期间切换焦点导致退出（#188）

## 症状与证据

单击唯一选中文件的名称，等待改名时程序退出。相邻文件可能正常，重复测试的结果也可能不同。2026-11-10 用户复现进程 3056 的日志确认：`.claude.json.backup` 已经过延迟检查，进入编辑器激活阶段，随后发生 `RefCell already borrowed`。

完整栈保存在 `artifacts/logs/issue-188-crash/confirmed-user-stack.txt`，原始错误记录为同目录的 `confirmed-user-crash.json`。栈依次经过 Slint 1.17.1 的 `accesskit::NodeCollection::build_new_tree`、条件编辑器初始化、`WindowInner::set_focus_item` 和 `WinitWindowAdapter::handle_focus_change`。最后一处在 `winitwindowadapter.rs:1593` 再次借用正在更新的无障碍适配器。

## 根因

编辑器在 `TextInput.init` 中立即调用 `focus()`。Slint 条件组件按需创建：它既可能由正常布局创建，也可能由 Windows 无障碍树遍历创建。后一条路径已持有无障碍适配器的独占借用，立即改变焦点又需要同一个适配器，触发 Rust 检查并退出。文件名称只是当时的目标，差异来自创建时序。

先前无界面用例用布局或控件查询提前创建编辑器，测试后端也没有真实 Windows 无障碍适配器，因此没有暴露嵌套借用。旧日志只保存鼠标与文件操作，无法确定根因；新增 panic 栈记录后，用户再次复现才确认调用路径。

## 修复

编辑器创建只安排一个零延时、单次的后续回调，退出控件构建后才聚焦并设置初始名称选区。校验失败后的重新聚焦也走这个入口，避免另一条同步焦点路径。待聚焦阶段不按失焦提交；组件取消或移除后不再运行其计时器。

不关闭无障碍功能，不捕获并忽略借用错误，也不修改依赖缓存中的框架源码。F2、右键和延迟改名共用同一个编辑器。

## 回归与排查入口

`src/app/rename_interaction_tests.rs` 检查无障碍查询实例化编辑器时不立即接管键盘；下一事件轮才聚焦并选中主文件名，取消后不再聚焦。详情、列表、内容、普通和分组网格均覆盖，原有文字点击、拖选、Enter、Esc 与长名称用例继续执行。

回放旧的同步聚焦实现时，新用例在详情布局失败：无障碍查询后立即输入会得到 `Q.backup`，证明构建期间已获取焦点。恢复修复后同一用例通过。对比日志为 `artifacts/logs/issue-188-crash/focus-before-fix.log` 和 `focus-tests.log`。

运行 `cargo test --workspace --locked rename_ -- --nocapture` 和 `python tools/verify.py --quick`。日志与汇总分别在 `artifacts/logs/issue-188-crash/`、`artifacts/logs/verify-test.log` 和 `artifacts/verify/summary.json`。实际 Windows 复验由用户对原文件执行，不由 Agent 操作窗口。

若同类问题再次出现，先查 `artifacts/logs/file-operation-audit.jsonl` 中对应用户进程的 `rust-panic`，再对照 `rename-delay-fired` 和 `rename-editor-*` 阶段。测试子进程的 `issue-188-owned-panic-diagnostic-probe` 是预期记录，应按进程号区分。

## 后续回归：提交接受不等于后台完成（#187）

2026-11-14 用户反馈：点击另一条目后，新条目已选中，原改名框仍存在。用户进程 95912 的日志显示，编辑 `D:\Unreal Projects` 后单击另一项，数秒后才记录同路径改名，实际磁盘执行立即成功。

提交入口此前只在后台完成后隐藏编辑框。目录改名准备会收集撤销所需的整棵目录树身份；即使名称没改，也等待这一步。原组件测试只记录提交回调和新选择，没有执行实际提交入口，也未检查旧编辑器退出，因而漏掉这个时序。

修复区分界面编辑与后台任务：原名完全未变时直接结束；合法新名称被接受后立即结束，并按任务保存原输入。失败时只在原目录仍有效、且没有后来的编辑器时恢复，避免旧任务关闭或覆盖新编辑。后台目录遍历仍留在工作线程。

`src/app/rename_interaction_tests/submission.rs` 以实际回调检查后台未完成时的编辑框退出，并用独占磁盘夹具检查成功、冲突和失败恢复。原实现的失败证据为 `artifacts/logs/issue-187-submit/before.log`。用户日志提取为同目录 `user-rename-events.json`。
