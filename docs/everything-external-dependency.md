# Everything 外部依赖使用说明

Everything 1.5 x64 是 AsterFiles 搜索和索引文件夹大小功能的外部依赖。AsterFiles 不携带 Everything 程序，不安装或管理其 Service，也不会因此请求 UAC。普通文件浏览、路径导航和 UNC 导航不依赖 Everything。

## 首次使用

1. 打开“设置 → Everything”。
2. 如果尚未安装，点击“下载 Everything”打开 voidtools 官方下载页，安装 Everything 1.5 x64，并按需启用文件夹大小索引。
3. 回到 AsterFiles，点击“自动发现”，或填写 Everything 程序路径和实例名称。
4. 点击“测试连接”；路径有效但程序未运行时，可点击“启动 Everything”后再次测试。推荐在 Everything 安装时启用后台运行或 Service，否则每次使用搜索前都需要先启动 Everything。

AsterFiles 只连接用户配置的实例。下载入口不会在应用内下载安装包，也不会静默创建服务、实例、配置或数据库。

## 不可用时

搜索页会保留用户输入并给出具体原因和下一步：

- 未安装：说明需要安装并配置 Everything，提供“下载 Everything”和“前往设置”；
- 未运行：提示启动 Everything，并提供“前往设置”；进入设置后可点击“启动 Everything”；
- 路径、实例或版本不匹配：显示具体原因并提供“前往设置”；
- 连接失败或超时：允许重试并提供“前往设置”；
- 文件夹大小索引未开启：只提示大小不可用，不影响搜索能力。

“前往设置”直接打开单例设置标签并定位 Everything 分类。Everything 不可用或位置未被索引时，AsterFiles 不会递归扫描磁盘或网络共享作为回退。

## 数据与权限边界

Everything 的程序、Service、配置、索引和升级均由用户管理。AsterFiles 不停止、覆盖、修改或卸载这些内容；卸载 AsterFiles 也不会清理 Everything 数据。搜索查询继续通过 Everything 官方 Query2 IPC，文件夹大小通过 Everything3 命名管道读取索引结果。
