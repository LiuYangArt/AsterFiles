# Issue #3 Everything 文件夹大小验证结果

## 结论

PASS。AsterFiles 已不再用 Query2 搜索结果推导文件夹大小，而是通过 Everything 1.5 的 Everything3 命名管道命令 18 直接取得索引值。本机实例 `1.5a` 对固定非空样本 `F:\CodeProjects\AsterFiles` 返回 `105452971398` 字节，临时空目录返回 `0`，二者均为真实索引结果。

## 实际执行机制

- 搜索、状态与分页：官方 WM_COPYDATA Query2。
- 文件夹大小：`\\.\PIPE\Everything IPC (1.5a)`，命令 18，UTF-8 完整路径，8 字节 `u64` 响应。
- 管道连接跨请求复用；断线后当前请求最多重连一次；overlapped 读写有有限超时，超时会取消。
- `UINT64_MAX` 映射为未索引；`0` 保留为合法大小；404、超时、断开和协议错误分别保留。
- 本地路径在 0/未索引时才尝试解析真实目标并重试一次；UNC 与映射网络盘不解析、不遍历。
- 回填继续校验 `TabId + RequestId + EntryId + 原始路径`；大小队列与搜索队列分离，不让连续慢查询阻塞搜索。

## 阶段结果

1. 协议单元验证：PASS。覆盖实例管道名、Unicode/扩展路径、0、`UINT64_MAX`、UNC 网络边界和响应状态。
2. 真实 Everything 验证：PASS。非空样本 `Indexed(105452971398)`；空样本 `Indexed(0)`；同一客户端连续查询成功。
3. 应用边界验证：PASS（自动）。状态映射与迟到/复用 EntryId 回填测试通过；未加入递归回退。真实快速导航、多标签、junction 与停止/重启场景仍待人工验收。
4. 项目验证：PASS。`python tools/verify.py` 全部通过；最终 `cargo build` 通过。

## 禁止机制使用情况

无。未使用 Query2 大小兼容层、SDK DLL、本地/网络递归扫描、展示文本路径或实例轮询。

## 证据

- 真实日志：`artifacts/logs/everything-folder-size-validation.log`
- 机器摘要：`artifacts/state/everything-folder-size-validation.json`
- 项目汇总：`artifacts/verify/summary.json`
- 验证计划：`docs/validation/2026-08-30-everything-folder-size-validation-plan.md`

## 未验证事项

- 未停止用户正在运行的 Everything 实例做破坏式停止/重启演练；断开、超时和单次重连由确定性状态及实现测试覆盖。
- 未执行真实 junction/符号链接、未索引位置、快速导航及多标签人工演练；相关自动边界已通过，但这些人工项不冒充已验收。
- 没有为人工截图临时启动 AsterFiles；真实 IPC 强断言、全量验证和 Debug 构建已覆盖本次代码边界。
