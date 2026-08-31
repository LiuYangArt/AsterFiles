# AsterFiles

AsterFiles 是一个面向 Windows 的高性能现代文件管理器原型。视觉体验参考 Files，底层使用 Rust，界面使用 Slint。

当前版本已完成本地浏览闭环、Everything 搜索与文件夹大小、基础文件操作、剪贴板和经典右键菜单。网络协议、缩略图和拖放暂不包含。

## 启动

```powershell
cargo run
```

## 验证

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
python tools/verify.py
```

日常验证生成 `target/debug/asterfiles.exe`。只有需要正式 Release 时运行 `python tools/verify.py --release`。

## 设计原则

- UI 线程只负责呈现和交互，磁盘工作放到后台线程。
- 目录切换采用“最新请求优先”，避免旧请求排队拖慢体验。
- 文件核心不依赖 Slint，为后续替换列表实现或做性能基准保留清晰边界。
- 优先本地路径；网络协议、全盘索引和插件系统后置。

详细范围与性能目标见 [docs/architecture.md](docs/architecture.md)，前期架构实施见 [docs/foundation-plan.md](docs/foundation-plan.md)，UI 与交互约束见 [docs/ui-interaction-design.md](docs/ui-interaction-design.md)，后续实施顺序见 [docs/task-list.md](docs/task-list.md)。
