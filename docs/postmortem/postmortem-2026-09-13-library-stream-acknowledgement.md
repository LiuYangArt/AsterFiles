# Issue #104：目录流消费确认与 Windows 文件删除竞争

## 症状与证据

收尾的完整并行验证中，`issue_104_library_source_hides_internal_cleanup_directory` 报 `InvalidData: data after directory completion`；此前专项和完整验证曾通过。首份失败日志保留在 `artifacts/logs/issue-104/finish-failed-test.log`，包含辅助进程退出及句柄回收证据。不能把这种偶发失败当作仅需重跑的测试波动。

## 根因

目录流以父进程删除批次槽文件确认消费，子进程原来用 `std::fs::metadata` 轮询槽。Windows 上该查询会打开文件句柄；与删除交错时可能先遭遇访问拒绝，随后备用查询发现文件已不存在，最终仍返回最初的访问拒绝。

新增的来源错误终态又把发送／确认错误与目录枚举错误混在一起：即使成功完成帧已被消费，确认阶段失败仍会生成第二个错误终态，父进程因此可能报告完成后出现数据。首份日志未记录额外帧内容与确认阶段的原始错误码，无法断言本次实际收到的帧类型；上述竞争与二次终态问题由实现分析及确定性回归分别验证。直接复测通过不能排除这条竞争路径。

## 修复决策

确认轮询改为 Windows `GetFileAttributesW` 的无句柄查询，只把文件或路径不存在认作消费确认，其余真实错误继续传播。保留单槽背压和取消回收行为。

来源枚举错误可以发布结构化错误终态；协议发送／确认失败直接结束，不再尝试把它包装成另一个来源错误帧。正常批次、来源原始身份与错误码均保留。

## 回归与排查入口

`cargo test issue_104_ -- --nocapture` 包含持有允许删除共享的旧槽句柄、删除槽后确认可完成的 Windows 测试，以及注入完成帧传输失败、断言不会产生第二个终态的回归。完整检查使用 `python tools/verify.py --release`，验证记录位于 `artifacts/verify/summary.json` 和 `artifacts/logs/issue-104/`。

以后遇到完成后多帧、来源偶发无权限或辅助进程异常退出，先核对来源错误与传输错误的边界、批次消费确认顺序及原始 Windows 错误码。相关实现集中在 `src/platform/windows/network/directory.rs`，库聚合位于 `src/library_loading.rs`。
