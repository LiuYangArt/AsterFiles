# AsterFiles

AsterFiles 是一个面向 Windows 的高性能现代文件管理器原型。

当前软件版本由 `Cargo.toml` 统一管理，采用 GNU General Public License v3.0（仅此版本）。

当前版本已完成本地浏览闭环、Everything 搜索与文件夹大小、基础文件操作、剪贴板和经典右键菜单。网络协议、缩略图和拖放暂不包含。

项目仍在开发中，后续内容以 GitHub Issues 为准。搜索及文件夹大小依赖用户自行安装并运行 Everything 1.5 x64，同时需要在 AsterFiles 设置中完成配置。

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

## 发布

推送与 `Cargo.toml` 版本一致的标签（例如 `v0.0.1`）会自动生成 Windows x64 portable ZIP 和 SHA-256 校验文件。已有标签也可以在本地补发：

```powershell
gh workflow run release.yml --ref main -f tag=v0.0.1
```

本地检查发布包可运行 `./tools/release.ps1 -Tag v0.0.1`，产物写入 `artifacts/release/`。Everything 是可选的外部依赖，不包含在发布包中。

## 设计原则

- UI 线程只负责呈现和交互，磁盘工作放到后台线程。
- 目录切换采用“最新请求优先”，避免旧请求排队拖慢体验。
- 文件核心不依赖 Slint，为后续替换列表实现或做性能基准保留清晰边界。
- 优先本地路径；网络协议、全盘索引和插件系统后置。

详细范围与性能目标见 [docs/architecture.md](docs/architecture.md)，基础架构边界见 [docs/foundation-plan.md](docs/foundation-plan.md)，UI 与交互约束见 [docs/ui-interaction-design.md](docs/ui-interaction-design.md)。任务与实施顺序以 [GitHub Issues](https://github.com/LiuYangArt/AsterFiles/issues) 和 [AsterFiles Development](https://github.com/users/LiuYangArt/projects/2) 为准。
