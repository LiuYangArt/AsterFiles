# Issue #3 Everything 文件夹大小验证计划

## 要回答的问题

Everything 1.5 已启用文件与文件夹大小索引时，AsterFiles 是否能通过 Everything3 命名管道命令 18 取得真实文件夹大小，并严格区分 0、未索引、未命中、超时、断开和协议错误。

## 固定环境与样本

- Everything：`C:\Program Files\Everything 1.5a\Everything64.exe`，实例 `1.5a`。
- 非空样本：从 `F:\` 中选择 Everything 已索引且大小大于 0 的普通目录。
- 空样本：在本地临时目录创建空文件夹，等待 Everything 收录后验证 0；若实时索引未及时收录，记录为环境限制，不用递归结果替代。
- 网络样本：UNC 或映射网络路径，只验证不会解析、遍历或隐藏回退。
- Oracle：同一实例的 Everything3 命名管道命令 18 原始 `u64` 响应；冻结后不因结果调整。

## 阶段门槛

1. 协议单元验证：管道名、UTF-8 路径、消息头、响应码、`UINT64_MAX`、0、超时和断开均有确定性测试；失败则停止后续阶段。
2. 真实 Everything 验证：已索引非空目录必须返回 `Indexed(n)` 且 `n > 0`；不再接受 `Indexed | NotIndexed`。
3. 应用边界验证：大小仍在独立后台队列，回填校验 `TabId + RequestId + EntryId + 原始路径`，首批目录不等待大小；网络位置无递归回退。
4. 项目验证：`python tools/verify.py` 与最终 `cargo build` 均成功。

## 状态定义

- PASS：四阶段全部通过，真实非空样本取得非零索引值。
- STOP：出现可复现的协议、状态或回填差异，后续阶段不运行。
- BLOCKED：Everything 服务、索引配置或本机环境确实阻止真实验证，且已记录替代检查，不能写成 PASS。
- NOT RUN：前置门槛失败。

## 禁止机制

- 不使用 Query2 搜索结果代替命令 18 的文件夹大小结果。
- 不递归扫描本地或网络目录，不从展示文本重建路径。
- 不修改 Everything 索引配置，不轮流猜实例，不引入 SDK DLL。

## 产物

- 机器摘要：`artifacts/state/everything-folder-size-validation.json`
- 原始日志：`artifacts/logs/everything-folder-size-validation.log`
- 结果文档：`docs/validation/2026-08-30-everything-folder-size-validation-result.md`
