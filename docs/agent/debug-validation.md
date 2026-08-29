# Agent 调试与验证

## 统一验证

在仓库根目录运行：

```powershell
python tools/verify.py
```

命令依次检查格式、静态问题、测试、无界面 Agent 场景和 Release 构建；某一步失败后仍继续执行其余独立检查，确保始终尝试生成 Release。终端输出 JSON Lines；汇总写入 `artifacts/verify/summary.json`，完整命令日志写入 `artifacts/logs/`，状态导出写入 `artifacts/state/`。

验证失败后先读取汇总中的失败步骤，再打开对应日志，避免加载无关的大日志。成功汇总包含 `target/release/asterfiles.exe` 的 UTC 文件时间和 SHA-256。

## 确定性 UI 场景

无界面导出权限页状态，不读取真实受限目录，也不会启动窗口：

```powershell
cargo run -- --agent-scenario permission-denied --no-ui
```

默认产物是 `artifacts/state/permission-denied.json`。可用 `--agent-state-out <路径>` 指定输出。

需要人工查看时可去掉 `--no-ui`。这会直接打开构造好的权限页，跳过会话恢复和目录读取；自动化验证默认禁止打开窗口。

## 状态字段

- `current_path`：当前页面展示的目标路径；
- `page_state`：稳定的页面状态名称；
- `visible_page_operations`：页面内部可见操作，不包含顶部导航；
- `error_type`：稳定的错误分类。

权限页的页面内部操作只能是 `request_windows_access`。该状态与 UI 显示条件共用同一动作模型，并通过 Slint 无窗口测试后端检查实际可见组件树。顶部后退、刷新继续属于全局导航，不得被误报为页面内“返回”或“重试”。
