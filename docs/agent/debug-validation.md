# Agent 调试与验证

## Windows CI（#106）

`.github/workflows/ci.yml` 在 main push、以 main 为目标的 PR 和手动运行时执行完整验证：验证脚本测试、Pester 脚本测试、格式、Clippy、Rust 测试、Debug 构建和全部 20 个无界面场景。Windows Server 2025 runner 提供 PowerShell 7、Rustup、Visual Studio C++ 与 Windows SDK；工作流安装 Python 3.13 和 Pester 4.10.1，Rustup 从 `rust-toolchain.toml` 安装固定版本及 rustfmt/Clippy。本地也需这些依赖；Pester 安装命令为 `Install-Module Pester -RequiredVersion 4.10.1 -Scope CurrentUser -Force -SkipPublisherCheck`。Rust 版本只有该 TOML 文件一处来源，标签构建同样读取它。固定 Server 2025 是因为现有 CopyFile2 稀疏复制依赖 Windows 11 22H2 之后的标志；Server 2022 会返回 Win32 87，不能作为该功能的验证环境。

Cargo 检查、测试、Debug 和本地 Release 构建均使用 `--locked`，依赖变化必须显式更新并提交 `Cargo.lock`。CI 缓存 Rust 依赖，只允许 main 保存缓存，不缓存 `artifacts/verify` 成功标记。PR 使用 `pull_request` 事件与只读权限，checkout 不保留凭据。工作流不执行发布；同一 PR/引用的新运行取消旧运行，最长 60 分钟。

在 Actions 的 Windows CI 运行页下载 `windows-verification-<run-id>-<attempt>`：其中包含 `verify/summary.json`、`logs/verify-*.log` 和 `state/`。成功或失败都执行上传，保留 14 天；安装依赖或 checkout 失败时可能没有仓库内验证产物，此时查看对应 Actions 步骤日志。验证入口的前置检查失败、命令启动失败及非零退出均写入日志与失败汇总；验证步骤失败时，后续必要步骤标为 skipped。超时或强制取消可能没有最终汇总，需查看已有日志与 Actions 终止原因。

云端复验（先等待成功运行完成，再触发失败演练，避免互相取消）：

```powershell
gh workflow run ci.yml --ref main
gh run list --workflow ci.yml --limit 5
gh run watch <成功运行编号> --exit-status
gh workflow run ci.yml --ref main -f failure_probe=true
gh run watch <失败演练编号> --exit-status
gh run download <失败演练编号> --dir artifacts/ci-failure-probe
```

`failure_probe` 默认关闭，只在显式手动运行时向 runner 临时 checkout 的 `src/main.rs` 追加未格式化函数。预期格式检查失败、后续步骤跳过、任务最终失败，但证据上传成功；核对下载汇总中的 `format: failed` 与对应日志。演练不提交文件，不改本地源码，也不创建标签。将正常与演练运行链接回写 Issue。`tools/test_verify.py` 另在独占临时目录实际启动返回 7 的 Python 子进程，验证日志、失败汇总、后续跳过和最终退出码，不操作应用窗口。

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

## 外部文件夹打开与前台激活（#110 / #117 / #118 / #119 / #121）

无界面回归包含 `issue_110_authorizes_actual_server_before_delivery_even_when_permission_is_denied`：使用真实本机命名管道核对接收 PID，模拟授权成功与拒绝，确认调用授权且拒绝不丢失含中文、空格与 UNC 的请求；发送前授权的严格顺序另由源码审查确认，接收线程中的标志检查不作为确定性的时序证明。既有 `external_tabs_are_created_in_the_most_recent_window` 与 `external_paths_open_as_new_tabs_in_the_active_window` 覆盖多窗口路由及连续新标签语义；`normalize_external_launch_path` / 启动参数测试覆盖盘符根 `D:"`、裸盘符与 `"%1\."` 尾缀修复。#118 覆盖 `/select` 与 `-select` 解析、文件路径分类为父目录 reveal、select 标志经命名管道保留，目录完成提交后按完整原始路径选中，以及 `SHOpenFolderAndSelectItems` 经后台 `IShellWindows` 注册把子项补为 reveal。#121 要求 Folder Open 不得在展示窗口或转交已运行实例前等待 `SelectItem`；资源管理器双击文件夹跳过陷阱；补选中后滚入可见区。下载器若启动时已有 `explorer.exe /select` 命令行，`external-launch-probe` 立即改写为 reveal。Free Download Manager 等不发送 `SelectItem` 的入口不在 #118（见 #120）。P4V 补选中仍可能慢于资源管理器，见 #122。#119 覆盖状态键丢失但 HKCU Folder 命令仍指向 AsterFiles 时的修复/恢复，不得把残留 AsterFiles 命令备份成「原先的资源管理器」。统一运行 `python tools/verify.py --quick`。

前台权限和窗口状态需要用户手动验收，无界面测试不能证明桌面激活成功。关闭旧实例后启动本仓库 Debug 程序，确认设置中的默认文件夹关联指向当前程序（若仍为旧 `"%1"` 模板，使用设置页修复/重新启用）；分别在普通窗口遮挡、最小化、最大化被遮挡、最大化后最小化时，从资源管理器双击文件夹与“此电脑”中的磁盘根。磁盘根应打开为 `X:\`（标签与地址栏不得出现 `X:"`），并显示并激活承载新标签的窗口，保持原有普通或最大化布局。再检查带空格/中文文件夹、多个 AsterFiles 窗口、连续打开多个目录和慢 UNC 目录；窗口应立即激活，加载完成不再次抢焦点，随后可正常切换其他应用。

诊断复用异步审计日志：Debug 为 `artifacts/logs/file-operation-audit.jsonl`，本地 Release 构建运行时为 `%LOCALAPPDATA%/AsterFiles/logs/file-operation-audit.jsonl`。筛选 `external-open-foreground-permission`、`external-open-forwarded`、`external-open-activation`、`shell-select-trap`、`external-launch-probe`：前两项记录转交进程的授权与路径发送结果，第三项记录接收进程的目标 HWND、是否从最小化恢复、系统调用返回值与实际前台 HWND，`shell-select-trap` 记录 Folder Open 是否接到 `SelectItem` 或 Explorer `/select` 命令行；`external-launch-probe` 记录父进程映像、本进程与 Explorer `/select` 命令行以便对照外部启动入口。用 PID 和时间关联一次打开；`active=false` 表示此次尝试后目标未成为前台，不能把路径发送成功误当成激活成功。`external-launch-probe` 为定位外部启动入口而包含命令行。

## Debug 优化配置（#100）

日常 Debug 构建对应用代码使用一级优化，对第三方依赖使用二级优化；保留行号调试信息、增量编译、调试断言和溢出检查。测试继承相同优化等级。第三方构建依赖和过程宏也受二级优化影响，因此首次切换配置需要重编依赖；后续通常可复用缓存。优化可能影响逐行调试和局部变量可见性。

性能比较可复用 `thumbnail-scheduler` 和 `folder-size-scheduler` 的 `--no-ui` 场景，比较相同输入的状态一致性与重复运行耗时。整进程计时包含启动和状态文件写入，不能代替真实渲染、滚动和标签切换的用户手动验收。性能证据保存至 `artifacts/perf/issue-100/`。

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

## Issue #16 侧栏磁盘占用条

```powershell
cargo run -- --agent-scenario drive-capacity --no-ui --agent-state-out artifacts/state/drives/capacity.json
cargo test issue_16_ -- --nocapture
```

无界面场景只使用确定性内存夹具，不启动窗口、不读取真实磁盘容量，也不访问 Shell、网络或目录。产物覆盖占用比例、零容量、防溢出、10% 警戒阈值、查询中与不可用状态、侧栏加载代次，以及文件任务完成后只刷新受影响卷。真实容量与 Windows 属性页对照、光驱无介质、断开映射盘、移动盘热插拔、窗口重新激活刷新、浅色/深色和 100%/125%/150% DPI 由用户手动验收；结构化运行日志写入 `artifacts/logs/drives/`，性能证据写入 `artifacts/perf/drives/`。
## Issue #15 快速访问

```powershell
cargo run -- --agent-scenario quick-access --no-ui --agent-state-out artifacts/state/quick-access/state.json
cargo test quick_access --quiet
```

无界面场景只验证原始路径身份、独立投放目标、单文件夹限制、文件任务隔离、加载代次和多窗口共享投影，不修改真实 Windows Shell。真实 Explorer 双向同步、文件列表拖入、地址栏图标拖入、Escape 与 100%/125%/150% DPI 由用户手动验收；结构化运行日志整理到 `artifacts/logs/quick-access/`。

## Issue #88 大图片目录缩略图调度

```powershell
cargo run -- --agent-scenario thumbnail-scheduler --agent-state-out artifacts/state/thumbnails/scheduler.json --no-ui
```

场景使用 2743 个纯内存项目，不启动窗口、不扫描真实目录、不访问 Windows Shell。它覆盖首屏计划被队列上限约束、大跨度视口跳转替换旧待处理项，以及旧视口结果被拒绝；产物为 `artifacts/state/thumbnails/scheduler.json`。单元测试另覆盖两秒延迟重试和最多一次自动重试。真实图片解码、占位清晰度、滚动手感和桌面窗口稳定性只能由用户手动验收；结构化运行日志和性能数据分别写入 `artifacts/logs/thumbnails/` 与 `artifacts/perf/thumbnails/`。

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
## Issue #62 文件任务中心

文件任务使用一个确定性无界面场景，不执行真实磁盘写入：

```powershell
cargo run -- --agent-scenario file-operation-center --no-ui --agent-state-out artifacts/state/file-operations/task-center.json
cargo test issue_62_ -- --nocapture
```

产物一次覆盖本地与网络运行任务、各资源域排队、冲突等待、暂停、部分完成和失败重试，并记录逐项操作能力、失败摘要、速度/剩余时间准备状态及“关闭窗口仅收起任务中心、后台任务保持不变”语义。真实视觉、键盘和无障碍交互仍由用户手动验收。

Issue #83 将该场景契约升级为 schema 2，并增加应用级撤销状态：是否可撤销、历史深度、最近操作类型、执行中状态和最近失败。专项无界面验证使用 `cargo test issue_83 -- --nocapture`，覆盖后进先出、有界历史、阶段化重试、强身份校验、独占复制根、隔离删除、目录外部新增和原路径占用；回收站条目身份由 Shell 的删除完成回调保存为绝对 PIDL，恢复时先落入同目录唯一临时名再无覆盖地移回原名。

Issue #81 的专项验证使用 `cargo test issue_81_ -- --nocapture`，通过临时目录与纯内存回调覆盖回收站准备计数、与 Shell 回收并行的递归项目/字节统计、重解析点边界、准备与执行阶段切换、Windows Shell 总体工作量、未知总量、当前项目、约 8 Hz 的进度合并及终态立即提交。测试不启动窗口、不操作真实回收站；无可靠百分比时任务条使用持续运动的不定进度，真实 Shell 行为、视觉反馈和 Explorer 对照基线由用户按人工验收清单执行。
## Issue #82 大目录永久删除

```powershell
cargo test issue_82_ -- --nocapture
```
性能证据单独运行：

```powershell
cargo test issue_82_fast_delete_performance_evidence -- --ignored --nocapture
```

专项测试使用临时目录和纯内存状态，不启动窗口。它覆盖无递归成本扫描的同卷目录原子移动、连续批次在上一批清理完成前移走、清理通道与普通本地通道隔离、全局清理串行、多选共享载荷根、记录认领与崩溃恢复、内部路径过滤和文件夹大小隔离，以及失败、取消、链接、UNC 和受保护路径边界。永久删除各阶段不触发任务窗口自动打开，其他慢任务仍遵守原有自动打开规则；`file-operation-center` 无界面场景同时导出两阶段任务文案与遗留位置投影。

真实 NTFS 的来源消失、空间释放、任务中心文案和多标签定向刷新由用户按人工验收清单执行。性能证据写入 `artifacts/perf/file-operations/`，分别记录来源消失时间、后台清理开始时间、空间释放完成时间、项目数、字节、CPU 和取消延迟；该测量不进入日常统一验证。


## Issue #64 目录首批与快捷方式解析

专项自动测试不打开窗口：

```powershell
cargo test issue_64_ -- --nocapture
```

目录枚举直接复用 Windows 枚举返回的普通条目元数据，只对符号链接和 junction 等重解析点进行一次跟随读取。`.lnk` 不再阻塞首批目录批次，而由独立有界单工作线程队列处理；只请求当前可见范围及前后一屏，执行前和回填时均校验 `TabId + RequestId + EntryId + 原始路径`，成功后只更新对应条目和行。首批 32、后续 256 的批次大小保持不变。

10 万普通文件与大量快捷方式的显式性能测量写入 `artifacts/perf/directory-loading/`，记录首批耗时、完整枚举耗时、跟随元数据读取数、快捷方式提交数和完成数。该测量不进入日常统一验证，也不得自动操作 AsterFiles UI。

```powershell
cargo test fs::directory_reader::tests::issue_64_directory_loading_performance_evidence -- --ignored --exact --nocapture
```

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

真实卷与 SMB 性能证据写入 `artifacts/perf/file-operations/`。每份记录系统版本、源/目标卷类型、数据集、复制模式、首个进度时间、吞吐、CPU、暂停/取消延迟、重试次数、临时项清理、是否从恢复状态继续和结果属性。10 GiB、跨卷、SMB 双向、限速/断网/NAS 重启、压缩/稀疏/符号链接等测试不进入日常验证；它们依赖真实设备，由用户按人工验收清单执行。Issue #42 将所有 UNC 复制、移动、永久删除和回收站操作放入受 Job Object 约束的辅助进程；取消立即终止，连续 10 秒无进度或冲突活动则超时终止，CopyFile2 的可重试网络错误最多自动重试一次。真实设备指标仍须由用户验收后写入产物。

```powershell
cargo test app::tests::issue_61_100k_small_files_performance_evidence -- --ignored --exact
```


## Issue #93 普通点击不得移动文件

专项命令：`cargo test issue_93 -- --nocapture`、`cargo test native_file_drop -- --nocapture`、`cargo test operation_audit -- --nocapture`；统一检查使用 `python tools/verify.py --quick`，包含 Debug 构建。

Slint testing 后端验证真实 clicked/pressed/Up 顺序、释放期间布局位移，以及实际列表/网格重建模型后的取消。内存状态与无窗口 COM 测试覆盖原始路径、标签/请求隔离、合法拖动只授权一次、孤立/取消/重复 Drop 零提交，以及复制/移动/快捷方式效果。测试不操作真实 AsterFiles 窗口或工作目录。

审计日志位于 `artifacts/logs/file-operation-audit.jsonl`，记录时间、PID、事件、原始来源/目标和结果。专项日志为 `artifacts/logs/issue-93-tests.log`，统一汇总为 `artifacts/verify/summary.json`。

人工验收仅用新建临时目录：创建 Bridge、Whitebox 与测试文件；在列表和网格分别连续单击、双击 Bridge 后侧键返回，确认目录位置不变；再验证主动拖动、Ctrl 复制、Shift 移动和 Escape 取消，并在 Explorer 核对结果。用户确认前保持 Issue 打开和 Project In review，不构建 Release。

事件顺序证据与长期排查入口见 [#93 postmortem](../postmortem/postmortem-2026-09-11-file-drag-safety.md)。

## Issue #91 普通目录分组网格

```powershell
cargo test --bin asterfiles issue_91 -- --nocapture
python tools/verify.py
```

仅普通目录分组的小、中、大、超大图标与平铺使用精确布局。完整轻量网格模型保留，显示节点限于可见区域及上下两行；布局包装复用文件内容模型，选择和图标更新不重置位置。未分组、内容、详情、列表、搜索与库使用原路径。

专项测试使用内存数据和 Slint 无窗口后端，覆盖精确底部、重建、缩放、局部更新、坐标命中、范围切换与短内容钳制。实际滚轮使用现有 winit 入口；专项测试调用其共用滚动处理，不能替代真实设备与 DPI 验收。日志 `artifacts/logs/issue-91-tests.log`，完整验证汇总 `artifacts/verify/summary.json`。

用户手动验收请使用临时目录：启用类型分组，逐个图标尺寸及平铺滚到底并拖动滑块；滚动后选择、全选、等待图标和大小回包；改变窗口宽度并切换分组；双击进入再侧键返回；确认普通点击没有移动文件，正常拖放及取消仍有效。内容模式原有分组滚动问题不属于此次修复。

## Issue #98 无缩略图文件的系统图标

完整验证：`python tools/verify.py`；专项测试：`cargo test issue_98 -- --nocapture`。已有 `shell-thumbnail --no-ui` 场景扩展为 Windows 实际图像提取证据，覆盖临时 PNG、MTL、未知扩展名和文件夹的图片来源、请求/返回尺寸及失败原因。指定 `ASTERFILES_THUMBNAIL_PROBE_PATH` 可只读验证用户样例，禁止修改该文件。结果写入 `artifacts/state/thumbnails/shell-png.json`，统一日志位于 `artifacts/logs/`，汇总为 `artifacts/verify/summary.json`。

应用异步审计日志中的 `grid_image_extracted` 和 `grid_image_failed` 区分真实缩略图、系统图标和两者均失败，携带原始路径、标签/请求身份和尺寸；系统图标成功不会进入缩略图失败重试。

人工验收：在 Debug 程序打开原截图目录，对照 Explorer 检查 MTL 与未知扩展名图标、PNG/文件夹缩略图；滚动离开再返回、切换图标大小、快速导航与关闭标签后，确认图片类型正确且不长期停留占位。Codex 不操作窗口，用户确认前不关闭 Issue 或执行本地 Release 构建。


## Issue #99 网格切换列表后的图标

专项测试：`cargo test issue_99 -- --nocapture`；统一验证：`python tools/verify.py --quick`，包含 Debug 构建。测试使用内存数据和 Slint 无窗口后端，验证网格切换到 List/Details 后补取普通图标、分组标题与搜索占位过滤、可见范围、缓存和在途去重，以及过期结果隔离。证据位于 `artifacts/logs/verify-test.log` 和 `artifacts/verify/summary.json`。

用户手动验收：打开原目录，先选网格再切 List，检查文件夹、MTL、OBJ 等系统图标；分别在分组和未分组下滚动、切换 Details、返回网格，再检查搜索结果切换 List。快速导航和关闭标签后，不应出现旧目录图标。Codex 不操作窗口；用户确认前仅更新 Debug 程序。

## Issue #105 会话与偏好原子保存

专项测试：`cargo test issue_105_ -- --nocapture`；统一验证：`python tools/verify.py`。测试在独立临时目录中覆盖首次保存、已有文件覆盖、截断解码，以及临时写入、落盘和 Windows 原子替换的确定性失败；失败后断言旧字节和旧会话仍可读取，且本次临时文件已清理；成功覆盖和首次保存都实际调用同目录 Windows `MoveFileExW`，覆盖额外使用 `MOVEFILE_REPLACE_EXISTING`。

Debug 诊断写入 `artifacts/logs/session-persistence.jsonl`，本地 Release 写入 `%LOCALAPPDATA%/AsterFiles/logs/session-persistence.jsonl`。事件区分无旧会话、读取成功、读取/解码失败、保存前校验失败、保存成功与分阶段保存失败，并记录错误类型、Windows 错误码和可读消息。会话及诊断文件访问都由启动/退出专用后台线程执行，不操作 AsterFiles 窗口。
## 常用操作命令序列（#111）

```powershell
./target/debug/asterfiles.exe --agent-scenario agent-actions --no-ui --agent-state-out artifacts/state/agent-actions/sequence.json
cargo test issue_111_ -- --nocapture
```

场景在本次独占的临时目录创建中文同名文件，复用真实应用状态、统一操作入口、后台目录及文件工作线程，贯通查询、打开目录、等待、选择、重命名、刷新、切换标签和取消。包含失效请求、关闭窗口/标签、重名冲突、缺失源文件与目录、加载中拒绝操作以及迟到结果隔离。取消场景暂缓通道交付以稳定验证“接受取消”与“后台完成取消”的差异，文件操作仍由真实工作线程执行。场景结束清理自己的临时目录；失败也写出检查与轨迹。

完整 `python tools/verify.py` 已包含该场景。证据位于 `artifacts/state/agent-actions/sequence.json`、`artifacts/logs/verify-agent-actions.log` 和 `artifacts/verify/summary.json`。`src/accessibility_tests.rs` 使用 Slint 无窗口后端读取控件身份及状态，不操作桌面窗口。

人工验收：启动 Debug 程序，分别在列表与网格选择同一文件，切换语言；使用无障碍检查工具核对名称、角色、选中、禁用、勾选及子菜单实际展开状态。再打开两个包含同名文件的窗口，分别重命名，确认只修改所选窗口的目标。加载占位和菜单分隔线不能被识别为可执行按钮。用户确认前 Issue 保持待审查；本地 Release 构建在确认后执行。

## 职责拆分的回归入口（#107）

目录队列测试：`cargo test --locked app::directory_loading`；文件执行测试：`cargo test --locked app::file_operation_worker`；任务协调测试：`cargo test --locked app::file_operation_coordinator`；窗口会话测试：`cargo test --locked app::window_sessions`。跨职责、应用动作及界面模型回归使用 `cargo test --locked app::`。每个切片接着运行 `cargo build --locked`，最终统一运行 `python tools/verify.py`。

拆分对照证据位于 `artifacts/issue-107/`：`baseline-summary.json`、`baseline-state/` 保存拆分前完整验证；各切片的 `*-tests.log` / `*-build.log` 保存测试和 Debug 构建输出。最终汇总仍使用 `artifacts/verify/summary.json`，场景状态仍使用 `artifacts/state/`。比较无界面输出时仅排除明确随运行变化的时间、临时夹具路径等字段；身份、取消和终态等业务字段必须保留并比较。

模块拆分不授权自动操作 AsterFiles UI。如需人工回归，使用临时目录：复制或重命名一个小文件并撤销；目录加载时切换标签；将标签移至另一窗口再关闭原窗口，确认文件内容、活动标签和目录展示正常。真实窗口结果由用户反馈。

## 临时联接测试的启动环境对照（Issue #109）

用户 Temp 下的联接创建失败时，应使用同一个探针对比自动启动与用户独立终端，并核对实际目标存在性；独立小程序仍可能继承启动环境。不能仅凭183或命令返回成功判断产品行为。已确认本机存在启动环境差异，未修改产品创建逻辑，具体环境机制未定。详见 [联接创建调查](../postmortem/postmortem-2026-09-12-junction-launch-environment.md)。

## Issue #103 网络目录分批与终止

```powershell
cargo test issue_103_ -- --nocapture
python tools/verify.py
```

测试不启动窗口、不连接 NAS：在独占临时目录创建真实文件，并由独立测试子进程运行正式枚举/批次协议。覆盖 4096、4097、8193 项完整且无重复、原始 UTF-16 身份、首批早于结束、持续进展刷新空闲期限、页面消费暂停下单槽背压、取消/关闭标签迟到拒收、网络等待期间本地目录可完成，以及挂起、崩溃、协议损坏、页面接收失败的进程回收。进程回收同时检查退出状态和 Windows 进程句柄已进入退出信号状态。

定向日志保存至 `artifacts/logs/issue-103/`；完整验证汇总仍为 `artifacts/verify/summary.json`。日志中的首批/完成毫秒数来自本机夹具，不代表 NAS 性能。真实验收由用户打开大于 4096 项的共享目录对照总数，并检查慢连接先出现内容、读取途中切换本地标签/关闭网络标签和断网后可重试。

## Issue #104 库来源隔离

专项命令：`cargo test issue_104_ -- --nocapture`；完整验证：`python tools/verify.py`，包含 Debug 构建。

测试通过真实辅助进程、临时目录和生产库调度入口验证：四个网络来源确实进入挂起后，健康库来源及普通本地目录仍能交付；取消与退出等待辅助进程回收；来源和批次去重、原始路径与不连续 Shell 来源索引、部分失败、全失败和空库，以及单来源消费确认背压。另验证流错误终态的真实不存在来源、权限错误与损坏记录分类，内部清理目录过滤，以及本地目录的文件夹大小状态。既有 #103 测试继续覆盖导航和关闭标签后迟到批次与终态拒收。专项日志位于 `artifacts/logs/issue-104/`，并发状态证据为 `artifacts/state/windows-libraries/issue-104-isolation.json`；完整验证汇总仍为 `artifacts/verify/summary.json`。自动测试不连接真实 NAS，不操作 AsterFiles 窗口。

真实库补验：使用含健康本地目录与离线共享目录的测试库，确认健康来源先显示；读取中切换到普通本地目录、关闭库标签并退出，确认不被离线来源拖住。按 Shell 来源顺序检查分组与部分失败提示。真实库检查可作为实际环境补验；用户授权以无界面功能验证作为验收依据时，在自动验证通过后按仓库收尾流程执行本地 Release 构建、提交并关闭 Issue，不将可自动验证的后台功能再次转交用户。

目录流确认竞争的症状、根因与确定性回归见 [#104 目录流消费确认排查](../postmortem/postmortem-2026-09-13-library-stream-acknowledgement.md)。
