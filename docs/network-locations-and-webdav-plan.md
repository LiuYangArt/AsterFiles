# WebDAV 网络位置实施方案

首个交付切片：[Issue #157](https://github.com/LiuYangArt/AsterFiles/issues/157)。上层协议需求：[Issue #31](https://github.com/LiuYangArt/AsterFiles/issues/31)。后续写操作章节用于明确架构边界，不代表已经纳入 #157。

## 结论

AsterFiles 可以支持 OpenList 的 WebDAV，但不能把 WebDAV 地址伪装成本地 `PathBuf` 再交给 Windows 文件 API。应把它作为应用自己的“网络位置”，在后台使用 HTTP/WebDAV 客户端访问，继续复用现有的标签、请求代号、分批加载、取消和文件操作协调边界。

本方案研究日期为 2026-10-13。文档和公开 Issue 使用脱敏示例，不保存用户 NAS 地址、账户或密码：

```text
http://nas.example:5244/dav/<挂载名称>/
用户名：OpenList 专用账户
密码：OpenList 登录密码
```

OpenList 官方文档说明，WebDAV 地址格式为 `http[s]://主机:端口/dav/`，端口与 Web 界面一致；读操作需要 WebDAV Read 权限，写操作还需要 WebDAV Management 以及具体文件系统权限。生产环境应优先使用 HTTPS。

首个切片已经可以添加位置、浏览目录，并把文件或选定目录下载到本地。还没有用用户的 NAS 做真实登录验证，因此不能证明当前 OpenList 版本、夸克驱动和账户权限已经可用。截图中的 HTTP 不等于安全建议：Basic 认证在 HTTP 上没有加密。保存时不再额外要求勾选确认。建议使用只开放目标目录、按需授予写权限的专用账户，避免使用管理员账户。

如果用户已用第三方工具把 WebDAV 挂成 Windows 盘符，现有应用有机会按普通路径访问，但仍需实测。Windows WebClient 的认证、文件大小和服务配置限制，以及挂载工具自身的取消行为，不应成为 AsterFiles 原生接入的长期基础。此次不修改系统映射、注册表或 NAS 设置。

## 参考实现

- OpenList 官方文档：[WebDAV](https://doc.oplist.org/guide/advanced/webdav)
- OpenList 服务端实现：[server/webdav.go](https://github.com/OpenListTeam/OpenList/blob/main/server/webdav.go)
- OpenList 驱动：[drivers/webdav](https://github.com/OpenListTeam/OpenList/tree/main/drivers/webdav)
- WinSCP WebDAV 实现：[WebDAVFileSystem.cpp](https://github.com/winscp/winscp/blob/master/source/core/WebDAVFileSystem.cpp)

WinSCP 通过 neon 处理 `PROPFIND`、`GET`、`PUT`、`MOVE`、`COPY`、删除、目录创建、认证、重定向和 TLS。AsterFiles 不需要照搬 WinSCP 的 C++ 层；Rust 侧应选成熟 HTTP 客户端，并补齐 XML 多状态响应解析。

核对的用户参考仓库提交为 `LiuYangArt/winscp@82fe067bda930ecb8e75dbea39eaeef0a7404f91`，OpenList 为 `482464c2affc5eeba5d4a25bb272aeb352ec5918`。WinSCP 的 `fcResumeSupport` 明确返回 false，不能把它当作 WebDAV 任意续传已获验证的依据。只参考协议处理和交互，不直接引入 WinSCP 可执行文件、C++ 构建链或 neon FFI。AsterFiles 是 GPL-3.0-only；若以后实际复制源码，必须单独核查来源文件许可证并保留声明。

### 依赖选择

直接依赖已经写入 `Cargo.toml`：`reqwest`、`tokio`、`url`、`quick-xml` 和 `percent-encoding`。HTTP、TLS、URL 和 XML 交给这些库，应用只实现所需 DAV 方法、身份和错误映射。密码写入 Windows 凭据管理器，目标名只含连接代号。异步运行时独立于 Slint 事件循环；取消覆盖连接、响应体读取和页面背压等待。

已比较的现成封装：

- [reqwest_dav](https://github.com/niuhuan/reqwest_dav)：研究时 crates.io 最新为 0.3.3，Apache-2.0；`list_rsp` 先读取完整响应文本再返回列表，不满足首批渐进提交。其 raw 接口可取响应，但仍需自行实现流式解析，增加此层收益有限。
- [Apache OpenDAL WebDAV](https://github.com/apache/opendal/tree/main/core/services/webdav)：已有成熟存储抽象，但当前 lister 将响应体转为字节后反序列化。当前只加一个协议，没有理由同时引入通用存储框架。
- [reqwest](https://github.com/seanmonstar/reqwest) 与 [quick-xml](https://github.com/tafia/quick-xml)：官方仓库未归档，研究时仍有近期维护；可组合出流式响应和增量 XML 解析。具体兼容版本及 TLS 特性在实施时用项目固定工具链验证，不在研究阶段添加包。

## 分阶段范围

### 第一阶段：可浏览和下载

- 新增 WebDAV 网络位置配置：名称、协议、主机、端口、路径、用户名、密码。
- 密码只保存到 Windows 凭据存储；会话和日志不得写入明文密码。
- 后台用 `PROPFIND Depth: 1` 分批取得目录；解析名称、目录标记、大小、修改时间和稳定相对路径。
- 文件下载走后台任务，支持取消、进度和临时文件；不在 UI 线程访问网络。
- 所有结果携带 `WindowId + TabId + RequestId`，迟到结果必须丢弃。
- 保存位置并在侧栏显示；历史、面包屑、返回和刷新使用同一远端身份。会话只存连接 ID、无密钥连接配置和远端路径。
- 首次交付允许下载文件及选定目录到本地；逐层枚举并有界排队，不跟随根外条目或循环目录。同名冲突默认不覆盖；远端名称不符合 Windows 文件名规则时明确报错，不静默改名。
- 文件双击提供“下载后打开”，下载完整成功后才调用本地打开入口；不支持远端编辑自动回传。下载本身不自动执行文件。

### 第二阶段：写操作

- 上传：先 `PUT` 到任务独占的临时名称，成功后 `MOVE` 到最终名称。临时名写入和提交能力需要实际验证；不支持时拒绝此上传路径，不无提示覆盖现有文件。
- 新建目录：`MKCOL`。
- 重命名和移动：同主机使用 `MOVE`。
- 删除：`DELETE`。
- 复制：服务端支持时使用 `COPY`；不支持时明确报错，不偷偷退化为本地复制。
- 写操作沿用现有任务中心、冲突确认、取消和审计日志。取消只停止客户端等待/发送，不能保证服务端已经停止；请求已发出而结果未知时先核查目标，再允许重试，不自动重放写请求。
- 删除明确提示远端永久删除，不承诺 Windows 回收站或撤销。临时上传清理只删除本任务明确记录的对象，失败时报告精确位置供用户处理。

### 第三阶段：可靠性和兼容性

- Basic 认证起步，按真实服务需要再加入 Digest；不默认实现 Windows 集成认证。
- 第一阶段就实现安全重定向：OpenList 的 GET/HEAD 可按存储配置返回 302 到网盘直链或代理地址。DAV 控制和写请求默认拒绝跨源重定向；下载允许 HTTP(S) 直链，但跨源必须剥离 Authorization、Cookie 和源站专用头，永不向新主机重试 OpenList 密码。拒绝 HTTPS 降级、循环和超过 5 次跳转。直链不缓存到会话；日志不记录签名查询串。
- 处理 `207 Multi-Status`、`401/403/404/409/423/429/5xx`，把服务器返回的错误转换为用户可理解的提示。
- 以 ETag、Last-Modified 和路径身份做缓存校验；不把展示名称作为操作身份。
- 首版支持下载取消，不承诺暂停续传或崩溃恢复。后续下载续传必须验证 `206`、Content-Range、对象长度和强 ETag；忽略 Range 返回 `200` 时禁止追加。上传续传无通用 WebDAV 保证，留在本轮范围外。
- 认证、证书校验、错误分类和重定向不是可延后的可靠性补丁，第一阶段即须完成。后续只扩展经真实服务验证的额外能力。

## 与当前架构的衔接

- 扩展 `NavigationLocation`，增加带稳定 ID 的 WebDAV 位置；不要把 `http://...` 塞进 Windows 路径。
- 新增独立 WebDAV 执行资源，与本地/UNC 目录队列隔离；目录仍按批次提交并可取消。
- WebDAV 条目保留原始 URL 路径和稳定条目 ID，展示文本只用于显示。
- 网络位置配置属于 AsterFiles 自有配置；Windows Explorer 的 NetHood 导入仍保持只读来源。
- 缩略图、Shell 图标和 Windows 文件监听默认不访问 WebDAV；先提供通用图标和手动刷新。

导航、条目、网络位置和目录请求已经使用独立的 WebDAV 身份。远端条目不保存假的 Windows 路径，后续操作不会误入 Shell。会话格式为 ASTF18，不保留旧格式兼容或迁移。

位置身份采用 `ConnectionId + 经验证的服务端资源路径`。按 URL 路径段编码中文、空格、`#`、`%` 和 `+`；只解码一次，不把编码斜杠转成本地分隔符。不按 Windows 规则折叠远端大小写。验证 `href` 的来源、配置根和直接子项关系，剔除集合自身及重复项；拒绝根外、父级跳转和异常路径。连接设置变更使旧请求和身份失效。密码、直链及其签名均不得进入条目身份。

首版对远端禁用 Windows Shell 菜单、Everything 搜索、目录监听、递归目录大小、撤销和缩略图提取。文件可以拖到桌面或其他文件夹，放下时按复制把内容下载到目标位置；文件夹拖出仍不做。UI 按明确能力显示动作，不用异常兜底。既有本地和 SMB 通路原样保留，不为了 WebDAV 改写网络复制辅助进程。

WebDAV 没有标准目录分页。对 `207` XML 使用命名空间感知的增量解析，逐个处理 `response/propstat` 状态，禁止外部实体，限制单条目/错误体/XML 深度；批次沿用最多 256 项或 1 MiB、单来源一批确认的背压规则。服务端若先完整枚举再发送，客户端无法消除首字节等待，必须允许随时取消，不能声称一定立即显示首项。已显示部分目录后 XML 损坏应显示部分失败，不当作完整成功。

后台同连接的目录读取与文件传输使用不同的有界槽位，避免下载占满后无法导航；建议初始上限分别为 2 和 2。连接超时、无字节进展超时与主动取消分开；UI 背压等待不算网络超时。不设置不合理的整文件固定下载期限。日志复用现有异步审计管线，附连接 ID、请求 ID、任务 ID、方法、状态码、耗时和脱敏错误。

## 验收与风险

先运行本地可控 HTTP 测试服务，无窗口测试真实客户端：根目录、中文及保留字符、同名大小写、空目录、深层目录、大目录、错误认证、混合 propstat、损坏 XML、挂起、断网取消、背压和迟到结果。慢服务/页面背压期间取消目标为 3 秒内结束请求；用至少十万条模拟目录验证首批在完整响应结束前提交，并记录在途批次数、内存和总耗时。

下载测试覆盖跨主机 302、禁止凭据泄露、重定向循环、过期签名、长度不匹配、磁盘空间不足、冲突和取消临时文件。用已知内容逐字节比较结果，不以状态码或进度条代表文件完整性。无可靠长度/校验值时明确只证明 HTTP 响应完整结束，不能推断云端版本一致。

真实 OpenList 验证使用用户单独授权的测试目录、专用账户和明确文件；由实施时提供的无界面探针运行，密码从凭据存储读取，不放命令行、环境转储或仓库。不得读取整盘、修改挂载设置或删除已有云端文件。需记录 OpenList 版本、驱动、代理/302 模式和实际支持的方法；不把 OPTIONS 宣告能力当作写操作成功证明。

实施验证命令为 `python tools/build.py test --locked webdav -- --nocapture`，随后完整 `python tools/verify.py`，后者包含 Debug 构建。日志放 `artifacts/logs/webdav/`，结果放 `artifacts/state/webdav/`。真实 OpenList 探针仍需用户单独授权的测试目录，密码从凭据管理器读取。

真实 UI 由用户手动验收：添加位置 → 浏览多层目录 → 切换标签并取消慢请求 → 下载到本地并打开 → 重启确认位置保留且不暴露密码。第二阶段另验上传、重命名、新建目录、永久删除提醒和权限拒绝。不自动操作 AsterFiles UI，也不构建本地 Release。

主要风险是不同 WebDAV 服务的 XML、重定向、锁和 Range 支持不一致。第一版应记录服务器能力和原始状态码，避免把“OpenList 可用”误认为“所有 WebDAV 服务器都兼容”。

任务范围、验收清单和状态只维护在 GitHub Issues；本方案保留设计边界，不作为第二份任务看板。其他协议、虚拟盘驱动、云同步、远端编辑回传和网盘账号登录均不在本轮范围内。
