# 分组图标模式滚动几何复盘

日期：2026-09-11
影响范围：按类型分组的图标、平铺、内容模式滚动与命中；`Flickable`/`ListView` 内容高度推断
状态：已修复；用户已确认真实窗口中滚动正常

## 摘要

`E:\SF_ActiveDocs\ProjectFD\Env\NordensLobby`（26 项，按类型分组）在中等图标模式下滚到底会回弹、拖拽滚动条时滑块闪动且页面位置跳动。根因不是分组逻辑，也不只是滚动条本身：Slint 1.17.1 内置 `ListView` 对整张列表只维护**一个**平均行高，用“可见行平均高度 × 行数”估算内容高度、用“平均高度 × 起始行号”估算窗口起点，并在每次重新布局时按估算值反写 `viewport-y`。分组标题行（32/48px）与图标卡片行（132/180/204/78/68px）不等高，估算值随可见行组成变化，于是滚轮、滑块和命中计算全部落在与实际内容不一致的坐标系里。

修复把文件区域的滚动几何收敛为协调层的唯一来源：Rust 投影给出每个视觉行的绝对 `top` 与内容总高度，Slint 侧改用普通 `Flickable` 只绘制窗口内的行，滚轮由容器统一处理，Rust 只在显式场景写入位置。

## 用户可见症状

- 分组图标模式下滚到内容末尾后继续滚，页面会向上跳回一段，永远到不了真实底部。
- 拖拽自绘滚动条时滑块闪动、页面位置来回跳；估算偏小时滑块会被画到轨道之外。
- 无界面复现测得：滚到底后重建投影模型，位置从精确底部 `-556` 被改写到 `-1452`（越过内容末尾 896px）；连续滚轮停在 `-613.14`（估算值），真实底部是 `-556`。
- 详细信息模式正常，切回图标模式立即复现；库视图的标题行是 48px，在详细信息模式下同样会复现。
- 同一根因还会让分组图标模式下的右键、框选、内部拖拽命中偏移：命中测试使用被估算污染的 `viewport-y` 配合精确行偏移，两套坐标系。

## 根因

### 1. `ListView` 只支持整表统一行高

`i-slint-core` 的 `model/repeater.rs` 中，列表虚拟化把当前物化实例的高度取平均后缓存为 `cached_item_height`，再据此计算：

- `viewport_height = cached_item_height × row_count`（内容总高度）
- `anchor_y = cached_item_height × offset`（窗口起点）
- 每次布局把 `viewport-y` 反写为 `-anchor_y + new_offset_y`
- 模型末尾未填满视口时执行 `vp_y += listview_height - y`（向上补正）

行高一致时平均值等于真实行高，这套估算与真实几何重合，所以未分组场景一直正常。标题行与图标卡片行不等高时，估算误差随窗口可见行的组成变化，本目录在 `3470~3960px` 之间摆动（真实内容高 `3560px`），滑块、滚轮边界和命中位置因此同时漂移。

### 2. 同一位置存在多个写入者与多个最大值

- `Flickable` 滚轮路径按估算的 `viewport-height` 判断能否滚动并夹取，因此文件区域内的滚轮根本不会走到 Rust 侧那段精确夹取。
- 行内 `scroll-event` 用估算高度自行夹取；窗口级滚轮用精确最大值。
- 自绘滚动条的 `maximum` 取估算高度，`value` 取 Rust 的精确位置，两者不同源。

### 3. 命中测试跨坐标系

`content_y = 指针 - 列表顶 + (-viewport_y)` 使用被估算污染的 `viewport_y`，再配合 `ListProjection`/`IconProjection` 的精确行偏移定位条目，于是在分组不等高时命中错位。

## 修复

- Rust 新增 `FileAreaProjection`、`file_view_window`、`project_file_view`、`file_area_models`：投影只物化视口上下各一屏的窗口行，每行带绝对 `top`，同时给出内容总高度与窗口覆盖范围。
- `ui/app-window.slint` 的列表与网格从 `ListView` 换成普通 `Flickable`：`viewport-height` 绑定 `file-content-extent`，行按 `top` 定位，删除行内估算夹取；自绘滚动条的 `maximum` 与 `page-size` 改用精确总高度与活动视口高度。
- 只有视口越出安全带时才重建窗口（`ensure_file_window`），普通滚动不重建模型；目录批次到达改为重建窗口投影，删除全量追加与投影合并阈值。
- 内容总高度或视口高度变化时统一夹取；目录模式滚轮只由容器处理，Rust 只在显露、键盘导航、视图切换等显式场景写入位置。
- 搜索模式复用同一机制，窗口使用局部坐标；网格缩略图计划把视口换算到窗口坐标系。

## 验证

- 无界面复现（`i-slint-backend-testing`）三条回归：投影重建后停在精确底部、滚轮滚到内容末尾停在精确底部、Rust 设置的位置被容器接受且滚轮从此继续。
- 增量加载集成测试：300 条分批条目 → 内容总高 `9600`（300 × 32px）、只物化 37 行、大跨度跳转后窗口首行 `top = 4384`。
- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo build` 通过；`cargo test` 623 passed（9 条失败全部是沙箱拒绝注册表写入与命名管道导致的平台测试）。
- `python tools/verify.py` 在本机沙箱需要 `--skip-process-check`（脚本准备阶段用 `Get-CimInstance` 关进程会被拒绝）。
- 用户在真实窗口确认原复现目录滚动、滑块与命中正常。

## 防复发规则

1. Slint `ListView` 只适用于整表行高一致的列表；出现分组标题、吸附行或不同高度的视觉行时，不得继续依赖它的虚拟化与内容高度。
2. 滚动几何必须只有一个来源；同一容器不允许同时存在“估算高度”和“精确高度”两条夹取链路。
3. 内容高度、窗口范围、命中、框选、拖拽和缩略图取范围必须消费同一份投影结果，不得各自重算行高。
4. 不等高行必须携带绝对 `top`（或等价偏移），坐标空间保持绝对，避免把窗口起始偏移散落到命中与选择逻辑里。
5. 物化窗口只在视口越出安全带时重建，滚动路径不得每帧重建模型。
6. 无界面测试可以直接发送滚轮事件并读取滚动位置，滚动类缺陷应先用这种复现把数值钉死再改实现。

## 长期排查入口

- 滚动几何投影：`src/app.rs`（`FileAreaProjection`、`file_view_window`、`project_file_view`、`file_area_models`、`ensure_file_window`）
- Slint 文件区域与滚动条：`ui/app-window.slint`（`list`/`grid` 的 `Flickable`、`clamp-file-scroll`、`LightScrollBar`）
- Slint 列表虚拟化实现：`i-slint-core` 的 `model/repeater.rs`（平均行高与 `viewport-y` 反写）与 `items/flickable.rs`（滚轮夹取）
- 相关设计边界：`docs/foundation-plan.md` 中的 Issue #88 与 Issue #91 段落
- 相邻历史记录：`docs/postmortem/postmortem-2026-09-01-directory-scroll-boundary.md`
- 本问题记录：GitHub Issue #91
- 自动验证汇总：`artifacts/verify/summary.json`
