# 进入文件夹后意外触发文件复制或移动

## 症状

用户在 Debug 版本中进入 folder A 后，出现将 folder A 复制到 folder B 的文件操作对话框。此前还出现过从 `F:\Temp` 移动到 `F:\SBoxProjects` 的准备对话框。用户确认该复现与剪贴板无关。

## 根因

详情列表文件行额外保存了 `pressed-here`，拖动移动和释放逻辑使用这个保存状态，而网格视图使用控件自身的当前 `pressed` 状态。鼠标捕获丢失、目录导航或列表替换后，保存状态可能与实际指针按压状态脱节，后续鼠标移动或释放被当作同一次文件拖动，进而提交复制或移动任务。

## 修复

删除详情列表专用的 `pressed-here` 状态及其赋值/清理，拖动移动和释放统一要求当前控件处于 `pressed` 状态。这样文件点击、进入目录和列表更新不会复用过期拖动候选；有效拖动仍沿用现有路径保护和复制/移动语义。

## 回归验证

- `cargo test --quiet internal_drag`：2 项通过。
- `python tools/verify.py --quick`：格式、Clippy、全量测试和 Debug 构建通过。
- `cargo build`：通过。
- Debug 产物：`target/debug/asterfiles.exe`。
- 真实窗口复现仍需用户在 Debug 版手动确认；项目禁止代理自动操作 AsterFiles UI。

## 后续排查入口

若再次出现，先收集 `artifacts/logs/` 中的拖放日志，并记录来源目录、目标目录、视图模式、鼠标按下到释放过程及是否发生目录导航。重点检查是否出现没有有效按压状态却提交拖放任务的事件序列。
