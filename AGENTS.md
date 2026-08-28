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

构建产物位于 `target/release/`。运行日志暂时输出到终端；性能 artifacts 统一写入 `artifacts/perf/`。

## 项目约束

- 不在 UI 线程读取目录、提取缩略图或执行文件操作。
- UI 只依赖应用协调层给出的模型，不直接调用 Windows API。
- Windows Shell/COM 代码放在独立的 `platform/windows` 模块，后续不得散落到 UI。
- 大目录必须增量加载、可取消、严格虚拟化；不要一次创建十万个 UI 节点。
- 不增加网络协议、插件或索引服务，除非当前里程碑明确需要。
- 注释说明目的、性能约束或 Windows 平台决策，不复述代码。
- 修改完成后至少运行与变更范围相符的最小验证，并保留可读取的错误输出。
