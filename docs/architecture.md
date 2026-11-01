# AsterFiles 架构索引

## 当前边界

```text
asterfiles（应用组装与协调）
  ├─→ asterfiles-ui（Slint 控件与只读展示模型）
  ├─→ asterfiles-platform-windows ─→ asterfiles-domain
  ├─→ asterfiles-webdav ──────────→ asterfiles-domain
  └─→ asterfiles-domain
```

界面不直接执行磁盘、Shell、COM 或网络访问；后台结果按窗口、标签和请求身份隔离。

## Cargo 工作区边界（#166）

| 包 | 实现入口 | 职责与依赖 |
| --- | --- | --- |
| `asterfiles` | `src/main.rs`、`src/app.rs` | 应用组装、会话保存、本地化和界面投影；通过平台包访问 Windows，普通依赖不包含 Windows API 包 |
| `asterfiles-domain` | `crates/domain/src/lib.rs` | 标签、条目、路径和网络身份、任务规则与调度；共享名称校验、物理几何和异步审计；不依赖 Windows API 或 Slint 包 |
| `asterfiles-platform-windows` | `crates/platform-windows/src/lib.rs`、`windows/`、`fs/` | Windows Shell、COM、文件读写、目录辅助进程和凭据；依赖领域包，不依赖 UI 或应用 |
| `asterfiles-webdav` | `crates/webdav/src/lib.rs` | WebDAV 路径、HTTP、XML、下载与槽位调度；依赖领域包，不访问 Windows 凭据管理器 |
| `asterfiles-ui` | `crates/ui/src/lib.rs`、`crates/ui/build.rs` | 单独编译 `ui/app-window.slint`；只依赖 Slint，不依赖应用、领域、平台或 WebDAV |

文件操作与 Windows 辅助进程互相配合，统一归属平台包，避免包之间循环依赖。WebDAV 凭据由 `windows/webdav_credentials.rs` 提供，应用负责连接客户端与凭据来源。工作线程优先级和时区读取也集中在平台包。

根包继续使用 `Cargo.toml` 中的应用版本，输出仍为 `target/debug/asterfiles.exe` 和 `target/release/asterfiles.exe`。Slint 生成代码只在 UI 包中编译，修改应用协调代码不再重编生成代码。kache 与 Rust 增量编译策略保持不变，详见 [构建缓存](agent/build-cache.md)。

`tools/check_workspace.py` 根据 Cargo 实际依赖图检查领域包的传递依赖，并检查应用的直接依赖和 UI 的传递依赖方向。统一验证同时检查、测试全部工作区成员，入口及证据见 [#166 验证](agent/debug-validation.md#issue-166)。

## 路径与文字约束

条目采用共享原始父路径的紧凑存储（#170），操作边界按需还原完整路径；目录读取、搜索显示与路径身份规则见 [长期边界](foundation-plan.md#条目路径存储170)。

- 路径身份使用 `PathBuf/OsString`，调用 Windows 时使用宽字符接口；不能用 UTF-8 字符串作为文件操作的唯一依据。
- Slint 中只存放用于呈现的 UTF-8 文本和不可解释的条目 ID；打开、重命名等操作通过 ID 找回原始路径。
- 中文等正常 Unicode 文件名应原样显示。无法无损转成 UTF-8 的极端名称使用替代显示文本，但仍必须能通过原始路径正确操作。
- 正文字体使用 Windows UI 字体及系统中文回退；Segoe Fluent/MDL2 只用于符号，不承担中文显示。

## 维护入口

任务范围与完成状态以 [GitHub Issues](https://github.com/LiuYangArt/AsterFiles/issues) 为准，实施状态和顺序由 [AsterFiles Development](https://github.com/users/LiuYangArt/projects/2) 管理。本文只记录当前有效的架构边界，不维护任务清单。

路径身份、标签会话、后台加载、文件操作、Windows 集成和网络边界统一见 [foundation-plan.md](foundation-plan.md)。项目执行约束和常用命令见仓库根目录的 [AGENTS.md](../AGENTS.md)；专项命令与证据路径见 [调试与验证索引](agent/debug-validation.md#issue-验证索引)。

## 应用操作与 Agent 状态（#111）

`src/app/actions.rs` 是应用协调层的常用操作入口，覆盖打开目录、刷新、切换标签、选择条目、重命名、查询和取消任务。控件回调与无界面序列复用同一实现；地址栏路径识别、搜索和主页专有刷新仍使用既有协调流程。这里不提供外部进程实时控制服务，也不增加协议或插件框架。

目标由窗口、标签、目录请求以及条目身份组成；路径始终保留 `PathBuf/OsString`。查询从真实窗口会话、可见条目和任务中心投影，附带状态版本、可执行操作与拒绝原因。加载失败、请求过期、条目消失或窗口关闭时明确拒绝，不能转投当前焦点窗口。目录接受回执携带请求身份，文件操作接受回执携带任务身份；接受和取消请求都不等于完成。

重命名先分配任务身份，后台解析映射盘并准备撤销证据，再进入现有资源队列；准备期间取消的任务不得重新入队。真实文件操作完成事件由界面与无界面场景共用的处理入口更新任务、撤销记录及后续排队任务。名称校验文案统一来自语言资源。

Slint 语义使用与语言无关的控件标识。条目标识包含窗口、标签和请求身份，列表与网格给出一致的名称、选中及可用状态；未物化条目由应用模型查询覆盖，维持原虚拟化。应用命令标识保持稳定，第三方 Shell 命令标识限定在菜单会话内。菜单勾选与展开状态来自实际菜单模型，加载占位和分隔线不提供动作。Slint 无窗口语义检查不能代替 Windows 实际无障碍树验收。

## Agent 场景与正式构建（#169）

`src/agent_debug/scenarios.rs` 的登记表统一维护场景名称和专属导出入口，参数解析和输出名称也读取这张表。`main.rs` 共用输出路径与无界面退出处理；Windows 资源管理器固定项和缩略图探针随分发移入该模块，平台能力仍由平台包提供。

正式 Release 保留现有本机命令行调试入口，便于直接诊断实际构建，不增加 feature 或第二套构建组合。正式目录、网络辅助进程和 Shell 应用角色不属于调试探针；入口职责、写入影响与验证命令见 [#169 验证说明](agent/debug-validation.md#issue-169)。

## 应用协调层的职责入口（#107、#163）

`src/app.rs` 负责应用组装、共享状态定义、事件分发和界面投影。回调按区域注册，Agent 状态导出和跨职责测试使用独立文件。`AppState` 仍是窗口注册表、任务中心、撤销记录和缓存的唯一共享存储；字段保持私有，同一把锁继续保证跨职责更新的一致性，不复制状态或增加锁层。

| 职责 | 实现入口 | 状态与依赖边界 |
| --- | --- | --- |
| 目录按需显示 | `src/app/directory_model.rs`：`DirectoryRows`、`DirectoryGrid`、`PositionedDirectoryGrid` | 共享原始条目快照与显示索引；按请求生成列表和网格行；模型直接提供条目位置与分组几何，选择、图片和内容更新只通知变化行；Everything 保留原有稀疏分页 |
| 图像缓存 | `src/app/icon_cache.rs`：`IconCache`、`BoundedImages` | 四套图像缓存仍由 `AppState.image_cache` 持有；普通和类型图标保留原容量规则，两套预览缓存各限 128 项，按最后写入顺序淘汰，读取不改变顺序；改名迁移在缓存内部完成 |
| 查询与系统集成状态 | `src/app/app_state.rs`：`EverythingState`、`IntegrationState` | 仍是 `AppState` 的字段，沿用同一把锁和原有请求代次、忙碌状态及错误记录，不建立独立状态副本 |
| 窗口回调组装 | `src/app/callbacks.rs`：`wire_callbacks` | 复用既有工作通道与确认窗口，按原顺序注册；`callbacks/` 下按地址栏、文件列表、导航、侧栏、标签、设置、菜单和任务划分职责 |
| Agent 状态导出 | `src/app/agent_exports.rs`：`export_*_state` | 从真实应用状态或领域模型生成无界面验证报告；`app` 保留原导出入口，报告格式与必要检查不变 |
| 目录加载 | `src/app/directory_loading.rs`：`spawn_directory_workers` | 自有本地/网络队列、主机执行集合与取消状态；只通过 `DirectoryRequest` / `DirectoryEvent` 传递原始路径及标签、请求身份，不依赖应用状态或 UI |
| 文件后台执行 | `src/app/file_operation_worker.rs`：`spawn_file_operation_worker` | 自有本地、网络和清理工作队列、冲突互斥与进度合并；依赖领域、文件系统和 Windows 平台层，不依赖应用状态或 UI |
| 文件任务协调 | `src/app/file_operation_coordinator.rs`：`enqueue_operation`、`prepare_retry`、`finish_file_operation` | 在共享应用锁内更新任务、撤销和目录刷新登记；后台重命名准备、排队、失败回退到任务终态、完成后下一项派发保持同一条链路 |
| 窗口会话协调 | `src/app/window_sessions.rs`：`AppState` 的窗口/标签生命周期方法及 `WindowState` 查询 | 使用 app 私有注册表，负责身份分配、标签排序/复制/关闭、跨窗口迁移和关闭决策；不创建或操作原生窗口 |

目录事件消费、目录监听、文件事件的窗口通知及全局界面刷新仍在 `app.rs`，因为它们共同连接目录、任务和窗口呈现。取消动作继续由 `actions.rs` 执行统一身份检查。`WindowSessions::lock()` 的活动窗口选择语义保持不变。协调子模块复用父层必要入口；工作模块不反向依赖协调模块，也不相互依赖。

职责测试就近放在对应模块；跨职责和界面投影回归位于 `src/app/tests.rs`，保留原测试名称，共用测试夹具位于 `src/app/test_support.rs`。测试辅助不进入运行时构建。

目录显示模型只保存来自标签会话的展示快照，选择和文件身份仍由 `TabSession` 管理（#165）。列表与网格共享原始条目；网格切片仅在请求行时建立，存活期间保持嵌套模型身份。框选、可见区请求和滚动查询直接使用显示索引，不为输入重建完整分组或生成整目录的 `FileRow`。实现边界见 [长期边界](foundation-plan.md#大目录按需显示与索引边界165175)，验证见 [#165 专项](agent/debug-validation.md#issue-165)。

## 复制执行与网络隔离（#149）

网络辅助入口组装隔离请求，再调用文件核心。请求明确持有取消控制、临时文件登记、扫描进度、传输进度和恢复状态。文件核心沿递归复制及跨卷移动传递同一执行上下文；普通本地入口创建本地上下文，无需网络登记。

`crates/platform-windows/src/windows/copy_execution.rs` 定义本地与隔离执行。隔离执行缺少登记路径时直接失败；隔离文件传输创建 `.asterfiles-copy` 文件或 Robocopy 暂存目录前，必须成功登记。登记对象可以随执行上下文跨线程传递，同一对象的登记更新串行完成，各任务互不共享所有权记录。恢复状态显式共享给进度监视器，查询结束时恢复进入查询前的状态，任务结束时清零。

依赖方向为网络辅助入口 → 文件核心 → Windows 复制能力。复制能力不调用网络业务入口；临时文件所有权位于 `copy_execution/temporary_copy.rs`，进程隔离和快照工具分别位于 `process_job.rs`、`network_snapshot.rs`。这些工具沿用已有 Windows Job Object、文件锁和二进制格式，不建立通用后端框架。

已确认写入位置、旧读取失效、四个真实读取槽位、250 ms 停止等待和退休暂存路径保护继续遵守 #137 的约定。克隆上下文只用于同一任务的控制和报告；同一任务的文件传输仍串行，不能并行写入多个当前暂存文件。

## 目录加载隔离与消费确认（#150）

`app/directory_loading.rs` 是普通目录、网络目录和库来源的统一读取与批次提交入口。所有真实目录枚举在既有 Windows 辅助进程内执行，主进程只处理请求、取消、协议和已读取的条目。普通路径也可能通过重解析点访问 SMB，因此不保留进程内枚举分支。

`DirectoryEvent::Batch` 必须携带消费确认。固定容量事件通道限制后台积压，`app.rs` 的事件泵等一次页面回调完成后才投递下一次。库调度器转交来源确认，并以非阻塞发送保留来源之间的取消与完成处理。条目身份、首批显示和迟到结果拒收沿用原规则。

## 标签导航状态转换（#151）

`crates/domain/src/lib.rs` 的 `TabSession` 负责目录请求的完整状态转换。批次经 `accept_directory_batch` 接受；完成经 `finish_directory_navigation` 一次提交排序、条目、位置、历史和选择；失败、取消分别经 `fail_directory_navigation`、`cancel_directory_navigation` 收尾。请求代号、位置、历史、待提交条目和索引只在标签内部修改，外部只读。

入口核对当前请求、目录来源和加载阶段，完成时额外核对原始位置。失败和取消保留上次成功的位置与条目；重复终态、过期请求以及切换搜索后的目录事件均拒收。UNC 分批期间的选择在成功提交后保留，本地和库沿用原有完成选择规则。

`app.rs` 负责选择对应标签、提供目录偏好与错误分类，并处理图标、窗口焦点、Shell 创建识别和重命名呈现。目录批次无论接受或拒绝，均归还 #150 的消费确认。事件消费返回接受结果，拒绝的完成事件不触发后续焦点、滚动或后台查询。会话恢复使用 `TabSession::new_at` 构造初始位置，不调用零散提交步骤。
