# Agent 调试与验证

## 统一验证

在仓库根目录运行：

```powershell
python tools/verify.py
```

Debug 程序的设置页包含仅开发构建可见的“开发工具 / UI 陈列室”，可直接打开永久删除、文件冲突、退出任务确认和文件进度窗口。陈列室复用正式窗口组件，演示按钮只关闭演示窗口，不修改真实文件或任务；Release 构建不显示该入口。

验证分为三档：

```powershell
python tools/verify.py --quick       # 小改动：格式、Clippy、测试、Debug 构建
python tools/verify.py               # 完整 Debug：再运行全部无界面场景
python tools/verify.py --release     # 收尾：复用未过期的完整验证并构建 Release
```

脚本启动时只关闭可执行路径属于本仓库 `target/debug` 或 `target/release` 的 AsterFiles，不影响其他目录的同名程序。默认首个失败即停止，后续步骤在汇总中标为 `skipped`；只有为了集中诊断多个独立失败时才使用 `--keep-going`。完整验证先构建一次 Debug，随后所有场景直接调用该程序，不重复 `cargo run`。成功完整验证记录当前工作树内容指纹；`--release` 在内容未变化时直接复用，若希望强制重跑则加 `--no-reuse`。

终端输出 JSON Lines；汇总写入 `artifacts/verify/summary.json`，完整命令日志写入 `artifacts/logs/`，状态导出写入 `artifacts/state/`。验证失败后先读取汇总中的第一个失败步骤，再打开对应日志，避免加载无关的大日志。

用户确认 Issue 完成后运行：

```powershell
./tools/finish-issue.ps1 <编号> -Message '<提交说明>' -Paths @('<文件1>', '<文件2>')
```

该入口只暂存 `-Paths` 明确列出的文件；存在其他未暂存或未跟踪内容时拒绝继续。随后按顺序执行 Release 收尾验证、差异检查、提交、证据回写、Project `Done` 和关闭 Issue；任一步失败都立即停止，不自动推送、打标签或发布。

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
## P3.D0 拖放底座状态

无界面导出原生拖放底座的稳定初始状态，不创建桌面窗口，也不执行文件操作：

```powershell
cargo run -- --agent-scenario drag-drop-foundation --no-ui
```

统一验证写入 `artifacts/state/drag-drop/foundation.json`。`drag_drop` 对象包含生命周期、是否注册、源数量、目标、协商效果、拒绝原因、最后事件和事件序号。无界面场景固定为 `unregistered`；真实窗口创建后才在主 winit/STA 线程完成注册。生命周期测试直接验证多窗口逐个注销、重复注销、统一退出注销和线程本地清理，保证每个窗口最多成功注销一次，且析构阶段不再调用 Windows 拖放撤销接口。

## Issue #15 快速访问

```powershell
cargo run -- --agent-scenario quick-access --no-ui --agent-state-out artifacts/state/quick-access/state.json
cargo test quick_access --quiet
```

无界面场景只验证原始路径身份、独立投放目标、单文件夹限制、文件任务隔离、加载代次和多窗口共享投影，不修改真实 Windows Shell。真实 Explorer 双向同步、文件列表拖入、地址栏图标拖入、Escape 与 100%/125%/150% DPI 由用户手动验收；结构化运行日志整理到 `artifacts/logs/quick-access/`。

## Issue #5 文件夹大小调度

```powershell
cargo run -- --agent-scenario folder-size-scheduler --no-ui --agent-state-out artifacts/state/folder-size/scheduler.json
```

场景不启动窗口、不扫描真实目录、不写 Everything 配置。产物记录普通首屏、重复滚动和后部滚动的提交数，完整大小排序的终态数量与最终刷新次数，以及取消后旧代次是否被拒绝。统一验证执行该场景；日志位于 `artifacts/logs/`，后续人工性能测量写入 `artifacts/perf/`。
## Issue #13 快速菜单搜索

无界面导出纯菜单模型状态，不创建窗口、不读取真实 Shell 菜单，也不执行文件操作：

```powershell
cargo run -- --agent-scenario quick-menu-search --no-ui --agent-state-out artifacts/state/context-menu/search.json
```

该状态同时证明单一“新建”根项、自有“文件夹”首项、Shell 重复文件夹过滤，以及其余模板顺序和原始命令身份保留。

产物记录大小写搜索、中文搜索、空结果、原始 Shell command ID 保留，以及过滤不会发起 Shell 查询。专项单元测试：

```powershell
cargo test quick_menu --quiet
cargo test context_menu --quiet
```

真实窗口的中文输入法、第三方扩展、动态/自绘菜单、DPI、多屏和边缘定位禁止 Agent 自动操作，由用户手动验证。菜单加载、迟到结果、调用和错误写入程序日志；实测耗时汇总放在 `artifacts/perf/`，用户截图放在 `artifacts/ui/`。

## Issue #21 快速菜单附属弹窗

无界面导出纯物理定位与窗口链会话状态，不创建桌面窗口，也不读取目录、元数据、Shell/COM 或网络：

```powershell
cargo run -- --agent-scenario quick-menu-popup --no-ui --agent-state-out artifacts/state/context-menu/popup.json
cargo test quick_menu_popup --quiet
```

产物覆盖负坐标显示器、根菜单双向翻转、加载态切换后根矩形稳定、子菜单独立重定位与工作区限高、多层 branch、同层分支替换、旧身份拒绝、跨窗口事件拒绝和请求过期 close-all；首帧屏障由 `hidden → cloaked → shown → presented` 运行状态与真实桌面人工验收共同确认。附属窗创建时直接携带 owner 与 popup/tool-window 样式；每个子菜单深度只创建一个可复用槽，同层悬停只换内容和位置，隐藏槽不参与焦点判断。真实首次/重复显示无白闪、连续同层悬停只有一个高亮、同一 Shell 子菜单重复打开不重新加载、多级 Left/Escape、任务栏/Alt+Tab、外部点击、中文输入法、100%/150% 双屏和 owner 关闭只能由用户手动验证；结构化运行日志包含 `quick_menu_popup_opened`、`quick_menu_popup_repositioned`、`quick_menu_submenus_repositioned`、`quick_menu_submenu_opened`（含 `depth`、`branch`、`reused`）和 `quick_menu_popup_closed`，整理后写入 `artifacts/perf/`。

## Issue #44 Windows Libraries

```powershell
cargo run -- --agent-scenario windows-libraries --no-ui --agent-state-out artifacts/state/windows-libraries/foundation.json
cargo test windows_libraries --quiet
```

该场景只运行确定性内存夹具，不启动窗口、不读写真实 Windows Library，也不访问来源目录。产物记录 Shell 顺序与固定状态映射、库稳定身份、多来源分批合并、同名条目的独立 `EntryId` 与原始路径、Shell 默认保存目录、无默认保存目录错误、部分来源失败、取消以及迟到批次和终态拒收。纯 Rust 投影测试另验证来源组严格保持 Shell 顺序、同名来源以稳定身份区分、组头展示来源名与真实路径、空来源保留组头，以及增量批次合并后不产生重复组。真实 Explorer 的系统库与自定义库名称、顺序、固定状态、图标、外部增删改名、多窗口同步、聚合浏览、列表/网格分组布局和 100%/125%/150% DPI 必须由用户手动验收；结构化运行日志写入 `artifacts/logs/`，用户截图写入 `artifacts/ui/`。

## Issue #10 网络底座状态

```powershell
cargo run -- --agent-scenario network-foundation --no-ui --agent-state-out artifacts/state/network/foundation.json
```

该场景不打开窗口、不访问网络，验证网络位置来源分离、原始 UNC 身份、设备发现代次、取消与迟到结果拒收。实现层另由单元测试覆盖深层 UNC/认证辅助进程编解码，以及本地/网络文件任务双资源域；这些字段不是实际网络性能证明。真实 NAS、设备发现阻塞、错误凭据、凭据冲突、Explorer 互操作和文件操作只能由用户人工验证；耗时与取消指标写入 `artifacts/perf/network/`，运行日志写入 `artifacts/logs/network/`。未完成这些实证前，Issue #10 保持进行中。

## Issue #20 文件列表直接键入定位

```powershell
cargo run -- --agent-scenario file-list-type-select --no-ui --agent-state-out artifacts/state/file-list/type-select.json
cargo test issue_20 --quiet
```

场景只操作已加载的内存模型，不打开窗口、不访问文件系统、Shell/COM、网络或 Everything。产物记录单字符、连续前缀、同字符循环、稀疏结果身份和请求隔离；超时、上下文清理、分组投影、滚入可见区与输入分流由专项测试覆盖。真实键盘手感、中文输入法、各视图、分组和 DPI 只能由用户手动验证。
## P2 文件任务状态

文件任务提供三个确定性无界面场景，不执行真实磁盘写入：

```powershell
cargo run -- --agent-scenario file-operation-running --no-ui
cargo run -- --agent-scenario file-operation-conflict --no-ui
cargo run -- --agent-scenario file-operation-partial --no-ui
```

统一验证把结果写入 `artifacts/state/file-operations/`。稳定状态名分别为 `running`、`waiting_conflict` 和 `partially_completed`，用于任务中心、冲突等待与部分完成回归检查。

## Issue #61 大目录复制流水线

专项无界面测试运行：

```powershell
cargo test issue_61_
```

测试使用临时目录或内存事件，不启动 AsterFiles 窗口。验证内容包括：扫描未完成时总文件数与总字节数保持未知且不生成百分比；发现数量单调增长并只计一次；首个文件在扫描终态前开始复制；稳定进度按时间与累计字节或文件门槛合并到约 5–10 Hz；完成、失败、冲突、暂停、继续、取消和扫描终态立即提交；暂停与取消能在扫描、文件和块边界生效；普通进度只更新任务中心，不重建其他主窗口模型。

10 万小文件场景属于显式人工性能测量，不进入日常测试和统一验证，也不由 Agent 操作应用界面。测量结果写入 `artifacts/perf/file-operations/`，至少记录首个复制开始时间、扫描完成时间、总耗时、原始进度事件数、任务中心模型提交数、主窗口目录模型刷新数和进程 CPU 时间；证据应能判断复制是否早于整树扫描完成、持续进度是否保持设计频率，以及普通进度是否造成无关窗口刷新。

## Issue #60 CopyFile2 专项验证

自动回归不启动窗口：

```powershell
cargo test issue_60_ -- --nocapture
cargo test fs::file_operations::tests:: -- --nocapture
```

专项测试覆盖 CopyFile2 的内容、修改时间、Windows 属性、NTFS 备用数据流、取消清理，以及暂停后对同一临时目标恢复。统一验证继续通过 `cargo test` 纳入这些测试。

真实卷与 SMB 性能证据写入 `artifacts/perf/file-operations/`。每份记录系统版本、源/目标卷类型、数据集、复制模式、首个进度时间、吞吐、CPU、暂停/取消延迟、重试次数、临时项清理、是否从恢复状态继续和结果属性。10 GiB、跨卷、SMB 双向、限速/断网/NAS 重启、压缩/稀疏/符号链接等测试不进入日常验证；它们依赖真实设备，由用户按人工验收清单执行。#42 完成前，SMB 无限阻塞不计为 CopyFile2 层已解决。

```powershell
cargo test app::tests::issue_61_100k_small_files_performance_evidence -- --ignored --exact
```
