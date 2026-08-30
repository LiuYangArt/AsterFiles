# Debug UI 陈列室布局与验证复盘

日期：2026-08-30
影响范围：Debug 设置页、独立确认窗口共享按钮、日常构建验证
状态：已修复

## 摘要

为难以触发的永久删除、文件冲突、退出确认和文件进度窗口增加 Debug UI 陈列室后，入口按钮先后出现文字越过边框、按钮宽度过窄以及整行越过卡片右边界。过程中还误用了旧 `CommandButton`，与独立窗口已经统一的按钮视觉发生分叉，并在没有真实检查截图的情况下过早宣称问题已解决。

最终删除旧 `CommandButton`，让陈列室和权限页复用 `StandaloneActionButton`；按钮内部统一处理图标、文字安全边距与裁切，陈列室行由父布局决定内容区宽度，两个按钮各占剩余宽度的一半。日常验证同时改为默认 Debug 构建，Release 仅由用户明确触发。

## 用户可见症状

- 英文 `File progress window`、`Delete confirmation` 等文字越过按钮边框。
- 四个陈列室按钮只占页面左侧一小块，宽窗口下比例失衡。
- 修正按钮宽度后，整组按钮又越过卡片右边界。
- 陈列室按钮与确认窗口按钮视觉不一致。
- Debug 程序运行时会锁住 `target/debug/asterfiles.exe`，后续构建报 `Access is denied`。

## 根因

### 1. 共享按钮禁止伸展，但调用处假设会自动铺满

`StandaloneActionButton` 默认设置了 `horizontal-stretch: 0`。调用处仅写 `horizontal-stretch: 1` 并不能可靠覆盖已有尺寸约束，结果按钮仍接近默认宽度，长英文文本自然越界。

### 2. 文字没有被按钮内容区约束

按钮中的文字最初只有居中设置，没有明确安全边距、可用宽度或溢出策略。Slint 会正常绘制完整文字，即使它超过按钮边框。

### 3. 父级宽度引用层级错误

为了强制铺满，按钮行声明了 `width: parent.width`。这里的 `parent` 解析到包含内边距之前的更外层宽度，导致行宽再叠加左右内边距后越过卡片。布局中的 `parent` 必须结合实际层级验证，不能仅凭局部阅读推断。

### 4. 没有先清点现有视觉入口

陈列室最初使用旧 `CommandButton`，而独立确认窗口已使用 `StandaloneActionButton`。旧组件来自此前内嵌确认面板，迁移后仅权限错误页仍在使用，实际已经成为冗余视觉分支。

### 5. 过早根据代码推断视觉结果

第一次修改后只完成编译和测试，没有检查真实窗口截图，就声称英文不会出框。用户截图随后证明结论错误。编译通过只能证明语法与类型正确，不能证明 Slint 布局约束在真实窗口尺寸下正确。

## 修复

- 删除 `CommandButton`，权限页和 Debug 陈列室统一使用 `StandaloneActionButton`。
- `StandaloneActionButton` 增加可选图标入口；无图标时不保留空位。
- 共享按钮统一提供左右安全边距，并对文字使用 `overflow: elide`。
- 陈列室每行由 `VerticalLayout` 内容区自动确定宽度，不再重复声明错误的 `parent.width`。
- 两个按钮显式使用 `(parent.width - spacing) / 2`，保持等宽且不越过内容区。
- `python tools/verify.py` 默认构建 `target/debug/asterfiles.exe`；正式 Release 改为 `python tools/verify.py --release`，只在用户明确要求时执行。
- 构建前检查并关闭正在运行的 AsterFiles Debug 进程，避免程序文件被 Windows 锁定。

## 回归验证

- `CommandButton` 在项目中已无定义和引用。
- `cargo check`：通过。
- `cargo test`：94 项通过。
- `cargo build`：通过，Debug 程序已更新。
- `python tools/verify.py`：格式、Clippy、测试、无界面 Agent 场景和 Debug 构建全部通过。
- 验证结束后无残留 AsterFiles 进程。

## 防复发规则

1. 新页面不得创建新的通用按钮视觉；先检索并复用当前独立窗口的共享按钮组件。
2. 共享按钮必须自身约束文字：安全边距、有限内容宽度和溢出策略不能依赖调用方补救。
3. 横向等分布局应由实际内容区计算；不要在带内边距的嵌套布局中随意写 `width: parent.width`。
4. 修改 Slint 布局后必须检查中英文、最小窗口宽度和常用宽窗口；编译与领域测试不能替代真实视觉检查。
5. 未亲自查看真实窗口或截图前，不得声称“不会出框”“已经对齐”等视觉结论。
6. Debug UI 陈列室必须复用正式组件和正式文本资源，只替换安全的演示数据与回调。
7. 日常开发构建使用 Debug；Release 构建只由用户明确触发。
8. Windows 构建出现 `failed to remove ... asterfiles.exe` 时，先检查正在运行的对应 Debug/Release 进程，不要重复构建。

## 补充：按钮图标垂直对齐回归

### 症状

权限页的“使用 Windows 请求访问权限”按钮中，锁图标明显高于文字的视觉中心。第一次修复只给内容容器补充完整高度，并继续依赖 `Image.vertical-alignment: center`；编译和无界面测试通过，但用户复测截图显示问题没有改善。

### 根因与错误判断

`HorizontalLayout` 中的图片只有固定 `16px` 高度，没有一个明确占满按钮高度的布局槽。`vertical-alignment` 在该约束组合下没有可靠地产生预期位置。第一次修复调整的是外层约束，却没有直接约束图片的纵向坐标，因此“代码看起来居中”和真实渲染结果不一致。

同时，现有无界面权限页测试只验证按钮数量和可访问名称，不验证像素位置；它通过不能作为图标已对齐的证据。

### 最终修复

- 在共享 `StandaloneActionButton` 内为图标增加固定宽度、纵向伸展的容器。
- 图标使用 `y: (parent.height - self.height) / 2`，按按钮内容区的实际高度显式计算纵向位置。
- 修复放在共享按钮内部，当前及以后所有复用该组件的图标按钮共同生效；纯文字按钮不受影响。
- 权限页只保留自身需要的宽度和内容左对齐设置，不再承担图标纵向修补。

### 新增防复发规则

1. Slint 图标与文字对齐时，不把 `vertical-alignment` 当作像素结果保证；先确认图标是否拥有明确的纵向布局空间。
2. 共享控件的对齐缺陷必须在共享组件内修复，不在单个页面增加偏移量。
3. 自动测试未覆盖几何位置时，必须明确其验证边界；真实截图才是视觉对齐的验收证据。
4. 用户截图证明修复无效后，应重新检查真实布局约束，不能继续用编译通过或代码推断为原结论辩护。
## 长期排查入口

- 共享独立窗口按钮与 Debug 陈列室：`ui/app-window.slint`
- 陈列室演示数据和安全回调：`src/app.rs`
- 统一验证：`tools/verify.py`
- 验证说明：`docs/agent/debug-validation.md`
- Debug 程序：`target/debug/asterfiles.exe`
- UI 截图：`artifacts/ui/`
