# AsterFiles 架构索引

## 当前边界

```text
Slint UI
   ↓ 事件和只读模型
应用协调层
   ↓
Rust 文件核心
```

界面不直接执行磁盘、Shell、COM 或网络访问；后台结果按窗口、标签和请求身份隔离。

## 路径与文字约束

- 路径身份使用 `PathBuf/OsString`，调用 Windows 时使用宽字符接口；不能用 UTF-8 字符串作为文件操作的唯一依据。
- Slint 中只存放用于呈现的 UTF-8 文本和不可解释的条目 ID；打开、重命名等操作通过 ID 找回原始路径。
- 中文等正常 Unicode 文件名应原样显示。无法无损转成 UTF-8 的极端名称使用替代显示文本，但仍必须能通过原始路径正确操作。
- 正文字体使用 Windows UI 字体及系统中文回退；Segoe Fluent/MDL2 只用于符号，不承担中文显示。

## 维护入口

任务范围与完成状态以 [GitHub Issues](https://github.com/LiuYangArt/AsterFiles/issues) 为准，实施状态和顺序由 [AsterFiles Development](https://github.com/users/LiuYangArt/projects/2) 管理。本文只记录当前有效的架构边界，不维护任务清单。

路径身份、标签会话、后台加载、文件操作、Windows 集成和网络边界统一见 [foundation-plan.md](foundation-plan.md)。项目执行约束、验证命令和产物路径见仓库根目录的 `AGENTS.md`。
