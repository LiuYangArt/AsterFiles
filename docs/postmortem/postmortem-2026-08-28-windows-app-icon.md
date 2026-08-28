# Windows 应用图标透明度与构建缓存复盘

日期：2026-08-28
影响范围：AsterFiles 首次接入正式应用图标后的 Windows Release 构建
状态：已修复

## 摘要

AsterFiles 首次接入正式图标后，桌面快捷方式出现白底和主体占比偏小。把源 PNG 改为透明背景后，用户进一步发现只有部分显示尺寸透明，其他尺寸仍出现白色方块。

问题由三层叠加造成：原始生成图是白色画布；Pillow 默认生成的多尺寸 ICO 将各尺寸写成 BMP 条目，Windows Explorer 对其中透明通道的呈现并不一致；Windows 会缓存快捷方式图标，而 Cargo 原先也不会在 ICO 变化时自动重新执行资源嵌入。

最终将源图确定性处理为透明 PNG，裁掉外围留白并提高主体占比；ICO 的 10 个尺寸全部改为 32 位 PNG 条目；构建脚本显式监听 RC 和 ICO 文件变化。重新构建后，各尺寸四角透明，EXE 可反向提取出正确图标。

## 用户可见症状

- 桌面快捷方式图标周围显示白色方块。
- 图标主体较小，外围空白明显。
- 调整桌面图标大小后，某些尺寸透明、某些尺寸仍为白底。
- 修改 ICO 后直接运行 `cargo build --release`，EXE 时间和哈希一度没有变化。

## 根因

### 1. 生成图本身是白底

最初的 1024×1024 PNG 四角像素为纯白，Alpha 通道全为 255。把它直接转换为 ICO，只会把白色画布完整保留下来。

### 2. ICO 内部编码不统一

ICO 是多个尺寸图像的容器，不是单张图片。Pillow 默认保存时把 16、20、24、32、40、48、64、96、128、256 共 10 个尺寸写成 BMP 条目。

这些条目虽然包含透明信息，但 Windows Explorer 在不同缩放级别、缓存路径和 ICO 内部编码下的处理并不一致，因此出现“只有特定大小透明”的现象。只检查最大尺寸或只打开 ICO 预览，无法证明全部尺寸正确。

### 3. 构建系统没有监听间接资源

`build.rs` 编译的是 `asterfiles.rc`。只修改 RC 引用的 `asterfiles.ico` 时，Cargo 不一定知道需要重新运行构建脚本，因此旧图标可能继续留在 EXE 中。

### 4. Windows 图标缓存干扰判断

桌面快捷方式会缓存多档图标。即使 EXE 已更新，旧快捷方式仍可能继续显示旧图标，所以不能只凭桌面观察判断构建产物是否正确。

## 修复

- 从原始 PNG 中提取文件夹主体，生成真实 Alpha 通道。
- 裁掉白色画布和阴影留白，将主体占比提高到约 92%。
- 保留 1024×1024 透明 PNG 作为 Slint 窗口和任务栏图标。
- ICO 内包含 16、20、24、32、40、48、64、96、128、256 十个尺寸。
- 每个 ICO 条目统一使用 32 位 PNG 编码，避免 BMP 透明度差异。
- `build.rs` 显式监听 `assets/windows/asterfiles.rc` 和 `assets/windows/asterfiles.ico`。
- Release 构建后从 EXE 反向提取关联图标验证嵌入结果。

## 回归验证

- 透明 PNG 的 Alpha 范围为 `0..255`，四角 Alpha 为 0。
- 逐项检查 ICO 的 10 个尺寸：均为 32 位 PNG，四角透明，同时包含透明和不透明像素。
- 从 `target/release/asterfiles.exe` 反向提取图标成功。
- `cargo fmt --check`：通过。
- `cargo build --release`：通过。
- 修复后 Release SHA-256：`A77E78725DD3F09278E7C0D90E76603E0EFB90FDB1AA3703944FBA73305CF92E`。

## 防复发规则

1. 应用图标源文件必须检查 Alpha 通道，不能把视觉上的白色背景误认为透明。
2. ICO 验收必须逐项枚举全部尺寸，不能只检查默认帧或最大帧。
3. Windows 目标的 ICO 条目统一使用 32 位 PNG；若因兼容旧系统改用 BMP，必须单独验证每个尺寸。
4. 图标内容占比应在最终 ICO 的小尺寸中检查，不能只看 1024×1024 原图。
5. RC 引用的每个间接资源都必须通过 `cargo:rerun-if-changed` 显式声明。
6. 构建后必须检查 EXE 时间、SHA-256，并从 EXE 反向提取图标；源 ICO 正确不等于 EXE 已更新。
7. 桌面验证应删除并重新创建快捷方式。Windows 缓存结果只能作为显示验证，不能作为资源嵌入的唯一证据。

## 长期排查入口

- 窗口和任务栏图标：`assets/app-icon.png`
- Windows 多尺寸图标：`assets/windows/asterfiles.ico`
- Windows 资源声明：`assets/windows/asterfiles.rc`
- 资源构建入口：`build.rs`
- Slint 图标绑定：`ui/app-window.slint`
- EXE 反向提取结果：`artifacts/icon/embedded-exe-icon.png`
- Release 程序：`target/release/asterfiles.exe`
