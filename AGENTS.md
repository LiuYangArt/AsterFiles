# AsterFiles 开发说明

这是 Windows 优先的 Rust + Slint 文件管理器。

## 启动与验证

```powershell
cargo run --release
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

构建产物位于 `target/release/`。运行日志暂时输出到终端；UI 截图写入 `artifacts/ui/`，交互日志写入 `artifacts/logs/`，性能 artifacts 写入 `artifacts/perf/`。

## 项目约束

- 不在 UI 线程读取目录、提取缩略图或执行文件操作。
- UI 只依赖应用协调层给出的模型，不直接调用 Windows API。
- 窗口级快捷键统一在 winit 窗口事件入口处理；Slint 焦点域只处理地址栏等控件内输入，不能单独承担全局快捷键。
- Windows Shell/COM 代码放在独立的 `platform/windows` 模块，后续不得散落到 UI。
- 大目录必须增量加载、可取消、严格虚拟化；不要一次创建十万个 UI 节点。
- 不增加网络协议、插件或索引服务，除非当前里程碑明确需要。
- 注释说明目的、性能约束或 Windows 平台决策，不复述代码。
- 修改完成后至少运行与变更范围相符的最小验证，并保留可读取的错误输出。
- 每个开发任务完成后必须运行 `cargo build --release`，确保 `target/release/asterfiles.exe` 已更新，供用户直接测试；交付时报告 Release 文件时间与 SHA-256。

## task
@docs/task-list.md
- 任务完成后更新文档状态
- 涉及路径、后台加载、多标签页、本地化或网络边界时，同时遵守并更新 `docs/foundation-plan.md`

## 架构红线

- 文件身份始终保留为 Rust 的原始路径或稳定 ID；展示字符串不得反向承担打开、重命名等操作身份。
- UI 线程禁止执行 `exists`、`is_dir`、目录枚举、元数据读取、Shell/COM 或网络探测。
- 所有目录加载携带 `TabId + RequestId`；只有对应标签的最新请求可以更新页面。
- 新导航、关闭标签和退出必须取消旧任务；慢任务不得占住全局唯一工作线程。
- 目录和网络结果采用分批提交；不得等待完整列表后才显示首批内容。
- 用户可见文案进入语言资源；新增硬编码文案需在当前切片内迁移。

## 参考项目

UI/交互参考 Files， 源码： F:\CodeProjects\Files
网络部分参考WinSCP https://github.com/LiuYangArt/winscp
