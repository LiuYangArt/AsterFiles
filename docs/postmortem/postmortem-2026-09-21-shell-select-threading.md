# 外部打开目录后选中延迟（#120 / #122）

## 症状与判断纠正

P4V 约三秒后才补选中；FDM 只打开目录且另起 Explorer；用户补充 Lark 也慢。同机 Files 设置为默认文件管理器后，三者都可打开并选中，P4V/FDM 接近即时、Lark 稍慢。

仅看到 AsterFiles 命令行中的父目录，或其日志未记录 SelectItem，不足以证明来源软件没有传递文件身份。Windows 可先打开父目录，再通过 Shell COM 交付选择。此前将 FDM/Lark 排除于通用方案的判断错误。

## 已证实的延迟与根因

原有真实 Shell 集成测试先注册接收端，再调用 SHOpenFolderAndSelectItems，选中正确但耗时 4.12 秒，几乎等满测试四秒寿命。这隔离了界面构建与调用软件的影响。

Rust windows-implement 0.60.2 默认令实现对象 Agile，允许 COM 绕过 STA 线程归属；接收对象却持有 Rc/RefCell 和线程所属的隐藏 HWND。原 SelectItem 直接 PostQuitMessage，其作用对象是调用线程的消息队列，不能保证退出接收窗口的循环。Files 的 C++ 对象不提供敏捷封送，并通过 PostMessage(hwnd, WM_CLOSE) 交给窗口所属线程退出。

## 修复决策

- 移植 Files 的 STA 对象及 WM_CLOSE → DestroyWindow → WM_DESTROY → PostQuitMessage 完成流程，收到选择立即返回；保留超时仅作无选择请求的资源寿命。
- 外部启动的那个进程在主线程接收，先注册再交付给主应用；主应用不再另建接收器。冷启动通过内部 UI 角色争取单实例并取得首个请求，避免递归启动或多开主页。
- 选择绑定原导航的标签、请求和原始路径；不通过当前窗口/同名目录猜测目标，不二次打开标签。
- 去掉按父进程跳过接收，以及扫描其他 Explorer 命令行、关闭其窗口的旧补救路径。诊断与文件身份分离。
- 沿用已有后台目录加载、原始路径与前台授权机制；不增加按 P4V/FDM/Lark 名称区分的代码。

参考：本机 Files 源码 abdfcb543（v4.2.21），src/Files.App.Launcher/OpenInFolder.cpp、FilesLauncher.cpp；MIT 归属见 THIRD_PARTY_LICENSES.md。Files 的独立原生启动器负责协议激活，AsterFiles 用同一个可执行文件区分短期入口与 UI 角色，复用命名管道；保持接收进程/主线程的职责及 500ms + 10s 时序，不复制其打包协议等待。

## 回归与排查入口

`cargo test --locked issue_122 -- --nocapture`、`cargo test --locked issue_118 -- --nocapture`、`python tools/verify.py`。两种真实 Shell 调用形式均要求在四秒寿命前完成；修复后针对测试曾测得 39 / 49 毫秒。这些数据只证明已注册接收器的系统调用与交付，不代表用户点击到 UI 定位的耗时。

详细证据：artifacts/logs/issue-122/targeted-tests.log、artifacts/verify/summary.json。运行诊断：artifacts/logs/file-operation-audit.jsonl 中 shell-select-received（线程一致性、到达耗时）、shell-select-trap（完成耗时）、external-open-activation（实际前台）。若收到选择但交付晚，检查消息循环与生命周期；若没有收到，检查默认关联指向的程序及接收注册，不能直接归因为来源软件缺少能力。

P4V/FDM/Lark 的冷启动、已运行实例、无多余 Explorer、选中与前台交互，遵守仓库规则由用户手动验收；Issue 是范围与验收状态的唯一来源。

系统接口说明：[SHOpenFolderAndSelectItems](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shopenfolderandselectitems)。

## 第二次定位：局部修复不能证明完整启动链路

用户随后确认 P4V 已即时选中，但 FDM 先打开目录、稍后又出现 Explorer，Lark 只打开目录。对应运行日志里 P4V 约 60ms 收到选择，另两者约 3s 后超时。旧测试提前注册接收器，遗漏了由外部启动进程接收的生命周期；通过这些测试不能宣称 Files 方案已完整移植，更不能断言来源应用缺失文件身份。

继续对齐 Files 后，接收窗口移至原始启动进程的主线程；先最多 500ms 等待首个选择，未到就交付目录并继续泵消息 10s。主实例通过每请求独立的 IPC 连接接收首包与后续包，后续结果直接绑定原标签和导航代次；主实例不再重复注册。同步删除额外的 SWC_EXPLORER/IShellBrowser，向 Shell 返回真实程序路径并隐藏/cloak 接收窗口。

IPC 写入缓冲不等于应用已接收；客户端在服务端 ConnectNamedPipe 前快速退出可能使入口遇到 ERROR_NO_DATA。每阶段增加应用入队后的确认，客户端有界等待，异常连接不能结束主实例的监听。

新增无界面跨进程探针：`target/debug/asterfiles.exe --agent-shell-launch-probe artifacts/state/shell-launcher/state.json`。该探针明确只验证接收进程身份、主线程、双阶段选择和回收，不把直接向隐藏接收窗口发送的选择等同于三款外部软件的端到端验收。

## 第三次定位：Known Folder 的 PIDL 不一致

用户复测仍然只有目录，说明上一轮的进程生命周期与 IPC 修复已经生效，但 Shell 没有把后续选择交给 Aster 的接收窗口。`artifacts/logs/file-operation-audit.jsonl` 显示 FDM/Lark 的接收窗口都已注册并存活约 10.5 秒，却没有 `shell-select-received`；这不是窗口提前退出或主实例漏收。

对同一个目录分别调用 `SHParseDisplayName`、桌面 `IShellFolder::ParseDisplayName` 和 `ILCreateFromPathW` 后发现，`D:\Downloads` 及其子目录的前者返回 Known Folder 别名 PIDL，后两者返回相同的文件系统 PIDL；普通 P4V 路径三种结果才相同。`IShellWindows::FindWindowSW` 只用前者能找到旧实现的窗口，使用 Files 所用的桌面解析方式则找不到它。证据见 `artifacts/logs/issue-122/pidl-method-comparison.json` 与 `findwindow-before-fix.json`。之前的临时目录测试和 P4V 路径恰好不会暴露这个差异，所以误以为链路已经通用。

修正 `parse_pidl`，改为完全复用 Files `OpenInFolder.cpp` 的 `SHGetDesktopFolder` → `ParseDisplayName`，不按下载目录、来源软件或路径名称加分支。回归探针改用独立调用方的 `ILCreateFromPathW`，并增加真实 Downloads 子目录用例；修复后的四个无界面用例（早选、晚选、纯目录、Downloads 晚选）均通过，结果保存在 `artifacts/logs/issue-122/findwindow-after-fix.json`。这证明 Windows 能在 FDM/Lark 使用的 Known Folder 场景中找到 Aster 接收窗口；桌面前台授权仍需用户手动确认。
