# 构建与缓存策略（#140）

## 日常入口

首次准备运行 `python tools/build.py setup`。它读取 `rust-toolchain.toml`，安装固定 Rust 及 rustfmt、Clippy，并下载校验过的 kache 0.27.0 官方 Windows x64 程序。工具位于 `.cache/tools/`，不安装系统服务、不修改全局 Cargo 配置。

```powershell
python tools/build.py build --locked
python tools/build.py test --locked
python tools/build.py run --locked
python tools/verify.py
python tools/build.py status
python tools/build.py clean
```

`build.py` 的 Cargo 入口在构建前和结束后维护缓存，Cargo 失败时也执行收尾。完整验证只在整轮开始、结束维护，不在每个检查之间清理。日志位于 `artifacts/logs/verify-*.log`；空间报告位于 `artifacts/verify/build-cache-*.json`，GC 输出位于 `artifacts/verify/kache-gc.json`。

普通 `cargo build --locked` 仍通过项目 `.cargo/config.toml` 使用 kache，产物仍为 `target/debug/asterfiles.exe`。直接 Cargo 不执行前后空间维护，日常使用上面的统一入口；临时直接 Cargo 后执行 `python tools/build.py finish`。不要覆盖 `CARGO_TARGET_DIR`、`RUSTC_WRAPPER`，以免绕开项目管理范围。

## 存储与预算

| 内容 | 路径 | 策略 |
| --- | --- | --- |
| 当前编译输出 | `target/` | 8 GiB 收尾预算 |
| 编译结果缓存 | `.cache/kache/` | 4 GiB blob 预算，由 kache GC 淘汰 |
| 缓存工具、索引与运行信息 | `.cache/` | 与 target 合计不超过 13 GiB |
| 验证日志和状态 | `artifacts/` | 不计入编译缓存，不自动删除 |

这些是受管构建的收尾预算，不是文件系统硬配额。构建进行中允许临时超额。kache 的上限不含索引和日志，因此脚本另检查总量；超出总预算时回收项目专属 kache 数据。清理不会触碰其他仓库或用户全局 Cargo 下载缓存。

Debug/Test 的 Cargo 增量编译与 kache 自适应增量均关闭，避免两套增量数据长期累积。保留应用的行号调试信息及现有优化设置。Debug 和本地 Release 使用同一个 target 根下的标准子目录，不按日期、分支或验证步骤复制构建树。

Rust 工具链、Cargo.lock、Cargo.toml、Cargo 配置或 kache 配置变化时，脚本退休旧 target。超过 target 预算时也退休整个编译树，而不是只清理本包。最终 Debug/Release 的 EXE 与 PDB 会保留，供手动测试；其他输出可重建。正在运行的 Cargo/rustc 会阻止清理。受管目录中的目录联接和符号链接会使操作失败，不追踪到外部路径。

空间报告分别记录文件逻辑总量、按文件身份去重的逻辑总量，以及 Windows `FileStandardInfo.AllocationSize` 分配量。预算采用分配量。文件系统元数据和其他软件缓存不在报告中。先前截图 65.2 GB 与现场约 18.61 GiB 逻辑总量不一致，不能据此推断为硬链接、稀疏文件或磁盘损坏。

## kache 的作用和限制

官方设计使用内容寻址缓存，复用输入相同的编译结果。当前 F 盘为 NTFS，默认从缓存恢复时复制文件；不能承诺零复制或让 target 与缓存自动共享磁盘块。不启用官方标为不安全的 Windows 硬链接恢复，也不为缓存重格式化磁盘。缓存专用于本仓库，开启 local-only，不上传远端，不安装系统服务或启用后台自动 GC。显式 GC 会临时启动项目专属后台进程，脚本在 GC 后停止它。

Rust 1.95 是从源码编译 kache 的最低要求，不是 kache 能缓存的项目最低版本。本项目升级到固定 1.95.0，并使用官方预编译 kache，分别管理工具链与缓存工具版本。

Windows 可执行文件缓存默认关闭，最终程序仍由 Cargo 正常编译链接。构建脚本运行本身也可能重跑，Slint/Skia 的生成输出不能假定全部命中。确认缓存收益需要删除受控依赖输出后重新构建，检查 kache 的 local_hits；没有改动时 Cargo 显示 Fresh 不能证明 kache 命中。

官方依据：[配置](https://github.com/kunobi-ninja/kache/blob/v0.27.0/docs/getting-started/configuration.mdx)、[去重限制](https://github.com/kunobi-ninja/kache/blob/v0.27.0/docs/deduplication.mdx)、[固定发行版](https://github.com/kunobi-ninja/kache/releases/tag/v0.27.0)。

## CI 与标签打包

Windows CI 与标签构建先执行同一个 setup，再恢复或使用 Cargo 下载缓存与 kache。云端不保存 target，避免不断上传历史构建树。缓存键包含工具链、构建配置、依赖锁文件和提交；CI 仅 main 保存。标签打包通过 `release.ps1` 调用受管构建入口。

本地 Release 构建仍只在用户明确要求或确认 Issue 完成后执行。工具链升级与缓存配置修改不触发发布，也不改变版本、标签或推送行为。