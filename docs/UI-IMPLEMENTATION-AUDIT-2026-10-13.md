# UI 实现审计：错位、焦点与状态一致性

日期：2026-10-13（Asia/Singapore）。基线：OcHub 0.5.24，main / 76410200193d4f2a22ffe721e688bb8322d19466，包含当前未提交的性能改动及用户原有改动。

后续修复、测试结果和仍未完成的平台验收见 [UI 修复与验收记录](UI-FIXES-2026-10-13.md)。下文保留修复前的审计证据。

**结论：问题集中在公共弹窗契约、实际内容宽度、焦点与草稿生命周期，以及动态内容的显示契约。仅调整页面 padding 无法解决。** 本轮归并为 17 组问题：4 组 P1、13 组 P2；11 组有实际界面或 AX 证据，6 组由源码链路确认但未完成相应运行场景复现。没有发现足以定为 P0 的证据。

本轮只做审计，新增本报告及取证资料，未修复生产 UI。三名子代理分别检查公共布局、表单弹层、业务页面；主代理独占原生验收，并交叉核对源码。优先级表示修复顺序：P1 是误改、丢稿或核心编辑受阻；P2 是可达性、可读性、状态误导或局部布局问题。

## 1. 范围与证据边界

源码覆盖公共外壳和组件适配、连接列表/编辑器、应用设置、设置子页、网络代理、远程节点、模型供应商、MCP、技能、用量、会话、工具、主题和关于。公共库检查的是 Cargo.lock 实际锁定的 ochub-ui 0.6.2 / c9b7766，GPUI 为当前 patch 的 ba106c5；不是根据旧设计文档推断现状。

原生验收复用前轮 just qa-app 已构建的固定包 /tmp/OCHUB-QA.app，Bundle ID io.ochub.debug.qa。本轮没有修改 GPUI，因此没有重复构建。通过 node_repl/sky 读取 AX 树、按索引操作、截图并刷新核对。使用隔离测试 home 和 XDG_DATA_HOME，数据均为虚构名称、example.invalid URL 和 synthetic 密钥；未执行真实应用配置写入或外部连接测试。QA 最后一个进程以退出码 0 退出，固定包保留。

| 尺寸 / 状态 | 实际覆盖 |
| --- | --- |
| 960×640，中文，深色 | 全部主要页面首屏/空态；Claude 应用设置；Codex 编辑器、折叠/展开预览、原始配置弹窗；工作区菜单；远程添加/手动添加；设置禁用项；侧栏滚动；Tab/Return |
| 960×640，英文，浅色 | 长连接名称/URL；长 MCP 数据；已安装远程技能；用量手动价格；Kimi 字段堆叠、动态文案及长行预览 |
| 1200×700，英文，浅色 | 连接页和 Kimi 双栏编辑器上部；确认约 400px 预览与剩余表单并存。模型网格滚动取证遇到驱动 noWindowsAvailable，未取得可靠的网格截图 |
| 1200×820，英文，浅色 | 启动连接页截图；后续出现 AX 已变化但截图仍为旧页面的现象。尚未区分采集、遮挡或提交链路，不能据此宣布应用冻结 |
| 静态覆盖 | 九种内置应用 schema 的文案路径；加载/错误处理；草稿回调；长错误列表；请求详情；公共组件键盘语义 |

未覆盖 Windows/Linux、多显示器恢复、日文实际画面、所有第三方主题、真实远程故障、所有弹窗内容组合和所有数据规模。完整原始证据位于 /tmp/ochub-ui-audit-20261013/；本报告引用的关键截图和 AX 摘录已保存到 [取证目录](assets/ui-audit-2026-10-13/)。截图有原始 Retina 尺寸，表中尺寸为逻辑窗口尺寸。

上一轮 workspace 测试和性能结果仍见 [性能报告](PERFORMANCE-2026-10-13.md)，本轮没有重跑或把它们冒充为布局验收。

## 2. 已确认的问题

“运行证据”只代表这一组至少有所述样本被实测，不代表所有同类控件都已验证。源码确认项的触发步骤是待执行验收用例。

| ID | 优先级 | 问题 | 证据 |
| --- | --- | --- | --- |
| UI-01 | P1 | 原始配置弹窗没有接管焦点，输入会改到背后名称 | 运行 + 源码 |
| UI-02 | P1 | 原始配置弹窗固定宽度大于宿主区域，右侧出窗 | 运行 + 源码 |
| UI-03 | P1 | 工作区菜单的 Escape 穿透并丢弃未保存编辑 | 运行 + 源码 |
| UI-04 | P1 | 应用设置开关保存会重建输入框、覆盖目录草稿 | 源码 |
| UI-05 | P2 | 侧栏和普通按钮缺少完整的键盘可达性 | 运行 + 源码 |
| UI-06 | P2 | 长连接名称/URL 绘制到按钮背后，图标被压扁 | 运行 + 源码 |
| UI-07 | P2 | 手动价格表操作列过窄，删除按钮被裁切 | 运行 + 源码 |
| UI-08 | P2 | 编辑器断点用整窗宽度；网格表头/行列宽不一致 | 源码 |
| UI-09 | P2 | 文件预览长行被裁切，列表没有相应横向范围 | 运行 + 源码 |
| UI-10 | P2 | 动态 schema 绕过本地化，英文界面混入中文 | 运行 + 源码 |
| UI-11 | P2 | 隐藏帮助文案的包装器同时吞掉动态禁用原因和元数据 | 运行 + 源码 |
| UI-12 | P2 | 应用行为等开关只绘制外观，AX 缺少名称和状态 | 运行 + 源码 |
| UI-13 | P2 | 弹层和页面快捷键路由不一致 | 运行 + 源码 |
| UI-14 | P2 | MCP 首次加载中、读取失败、真实空列表共用空态 | 源码 |
| UI-15 | P2 | 工具页刷新提前报成功，读取失败被转为空数据 | 源码 |
| UI-16 | P2 | 请求“详情”仍截断完整 ID 和模型标识，不能取回原值 | 源码 |
| UI-17 | P2 | 预览错误区不受限，能占满固定高度并挤掉代码 | 源码 |

### UI-01：弹窗输入泄漏到背后表单

复现：在 Codex 新建连接名称输入 UI focus test → 展开写入预览 → 点击“编辑” → 不再点击正文，直接输入 -MODAL-FOCUS-PROBE。AX 明确记录背后的名称变成 UI focus test-MODAL-FOCUS-PROBE；弹窗内原始配置没有变化。关闭弹窗后，名称中可见追加内容。

根因：[provider_editor.rs](../crates/app/src/provider_editor.rs) L1016–1033 创建 CodeEditor 后仅设置 raw_edit；没有转移焦点。ochub-ui 的 components.rs L1147–1180 只提供遮罩和鼠标 occlude，没有焦点范围、背景键盘隔离或关闭后焦点恢复。鼠标遮挡不等价于模态行为。

修复：在独立 UI 库建立有状态 Modal 宿主，打开时聚焦初始控件，限制 Tab/Shift-Tab，拦截背景页面快捷键，关闭时恢复原焦点。验收必须验证实际输入去向，不能仅检查遮罩是否出现。

证据：[弹窗画面](assets/ui-audit-2026-10-13/raw-modal-min.jpeg)、[关闭后被改动的名称](assets/ui-audit-2026-10-13/modal-focus-leak.jpeg)、[AX 取证](assets/ui-audit-2026-10-13/interaction-evidence.txt)。

### UI-02：弹窗相对于错误的可用尺寸布局

960px 最小窗口扣除 252px 侧栏，编辑器宿主仅约 708px；原始配置弹窗却固定宽 760px。实测右侧超出窗口，底部“应用”也贴出右边界。

根因：[provider_editor.rs](../crates/app/src/provider_editor.rs) L3514–3516；[main.rs](../crates/app/src/main.rs) L502；[product_ui/shell.rs](../crates/app/src/product_ui/shell.rs) L10。公共 modal_overlay 没有宿主宽高上限和边距契约。Remote 的多个弹窗另有 720/760px 高度常数，但本轮手动添加的初始表单实际完整可见，不能说它已被裁切。

修复：基于实际宿主尺寸设置宽高上限；明确弹窗属于整窗还是内容区；固定头尾、正文单独滚动。验收 960×640、长标题、长错误、动态增行。见 [原始配置弹窗](assets/ui-audit-2026-10-13/raw-modal-min.jpeg)。

### UI-03：Escape 关闭错误的层级并丢稿

复现：修改连接名称 → 打开左上工作区下拉 → Escape。实际退出编辑回到列表，工作区菜单却仍展开，未出现草稿保护。

根因：[app_ui.rs](../crates/app/src/app_ui.rs) L637–684 的 cancel_active 未优先处理 workspace_scope_open；[provider_editor.rs](../crates/app/src/provider_editor.rs) L293–307 继续发出 Cancelled；app_ui.rs L3314–3316 销毁 editor。下拉本身也没有消费 Escape。

修复：按最上层弹出控件 → 当前弹窗 → 页面草稿的顺序处理取消；建立明确的事件消费规则。菜单关闭不得引起页面取消。证据：[Escape 前](assets/ui-audit-2026-10-13/workspace-before-escape.jpeg)、[Escape 后](assets/ui-audit-2026-10-13/workspace-after-escape.jpeg)。

### UI-04：设置回调覆盖未保存目录

源码确认：Claude 应用设置内，编辑配置目录但不保存，再切换行为开关；开关调用 persist_mutation，成功回调不区分被修改的设置，重新创建整个 config_dir 输入。输入期间遇到异步保存回调也有相同覆盖窗口。

位置：[app_settings_view.rs](../crates/app/src/app_settings_view.rs) L201–205、L220–230。因为相关开关在 AX 中不可见，本轮遵循索引操作约定，没有将这个场景标记为运行复现。

修复：目录草稿和已存设置分开，维护 dirty 状态；仅目录自身保存或显式重新加载时替换输入实体。验收应覆盖“改目录 → 切其他开关”和“保存进行中继续输入”。

### UI-05：有 AX 按钮名称，但 Tab 无法到达

Codex 列表连续按 Tab 五次，AX 仍聚焦窗口，没有可见按钮焦点；再按 Return 无动作。侧栏和普通按钮源码仅设置 role/label/click，没有完整的 focusable、Tab 次序和键盘激活。

位置：[product_ui/shell.rs](../crates/app/src/product_ui/shell.rs) L90–130、L147–181；[app_ui.rs](../crates/app/src/app_ui.rs) L3373–3388、L3406–3428；锁定 ochub-ui components.rs L180–208。已核对锁定 GPUI：role 和 click 不会自动补焦点行为。

修复：公共 Button、侧栏、分段选择器等统一实现焦点、激活和焦点样式；保留已有设置行中较完整的实现。AX 可点击不能替代键盘验收。证据见 [键盘轨迹](assets/ui-audit-2026-10-13/interaction-evidence.txt)。

### UI-06：连接卡片缺少可收缩文本边界

英文、960×640、长名称及长 URL 时，文字直接绘制到 Edit/Duplicate/Delete/Switch 后面，图标也被横向压扁。1200px 连接页仍能看到同类覆盖，因此不能只靠提高最小窗口宽度修复。

位置：[product_ui/providers.rs](../crates/app/src/product_ui/providers.rs) L80–109 文本列缺少 flex_1/min_w_0 和明确截断/换行；L113–122 操作区固定不缩；L31 外层硬裁切。图标也未固定为不收缩。相同文件的当前连接 hero 已有更合理的文本约束。

修复：文本列占剩余宽度，图标固定尺寸，操作区有明确预算；名称和 endpoint 分别规定截断/换行及完整值入口。证据：[长连接覆盖操作区](assets/ui-audit-2026-10-13/provider-long-text.jpeg)。

### UI-07：价格表把完整按钮塞进等宽小列

960×640 英文界面，添加一条虚构手动价格后，Delete 按钮右侧超出表格并被裁掉。按钮不是纯图标，它包含图标、文字和两侧 padding。

位置：[usage_view.rs](../crates/app/src/usage_view.rs) L3238–3283；ochub-ui components.rs L158–175、L204–206、L1332–1363。表格六列等分，操作组不换行，没有独立列宽。

修复：为操作列明确尺寸，或者使用有完整可访问名称的纯图标操作；模型列获得更合理的宽度。不能只缩小全表字号。证据：[价格表右侧裁切](assets/ui-audit-2026-10-13/pricing-actions-clipped.jpeg)。

### UI-08：整窗断点不能代表表单宽度

[product_ui/provider_editor.rs](../crates/app/src/product_ui/provider_editor.rs) L8–19 在整窗 1200px 开启双栏，在 1050px 以上就停用字段堆叠；[provider_editor.rs](../crates/app/src/provider_editor.rs) L4132–4134 传入的是整窗宽度。1200px 扣除 252px 侧栏、48px padding、16px 间隙和至少 400px 预览，只剩约 484px 表单，还包含列表内侧留白。Kimi 六个文本列仍走横表：第一列固定 96px，其余五列只能分享很小的空间。

表头另在 provider_editor.rs L2946 为删除列留 72px，而实际行 L3009–3016 使用本地化后的自然按钮宽度，二者没有共享列定义。

修复：按实际表单宽度和列数选择行卡片或横表；表头/内容共用列宽；必要时允许受控横滚。验收 1049/1050、1199/1200、不同语言和不同列数。本轮已取得 [1200px 双栏上部](assets/ui-audit-2026-10-13/editor-split-1200.jpeg)，网格处尚无可靠运行截图，不能将预测的挤压幅度当实测值。

### UI-09：配置预览长行不可完整查看

Kimi 的长 base_url 在预览右边界被直接裁断。源码确认其 uniform_list 没有对应的横向内容宽度：provider_editor.rs L3175–3183 行内容不缩，L3347–3356 使用默认列表，L3314/L3365 外层裁切；锁定 GPUI 默认 FitList，横向范围仍按视口宽度计算。

修复：为长行设置真实横向滚动范围，或提供可切换换行；保留虚拟化。不能只给外层加 overflow_x_scroll 而忽略列表内容宽度。原始 CodeEditor 自身已有双轴滚动，勿重复嵌套。证据：[长行被截断](assets/ui-audit-2026-10-13/preview-long-line.jpeg)。本轮未取得有效的横向手势轨迹，“没有横向范围”由锁定源码确认。

### UI-10：动态表单没有本地化契约

英文界面实测仍有“自定义 / 官方订阅”“模型”“默认模型别名”“能力”等中文。它们不是用户输入或协议值。

schema 的 FormField、FormSection、GridColumn、Preset、ConfigIssue 直接持有 String；[provider_config/mod.rs](../crates/core/src/provider_config/mod.rs) L384–395 直接返回九种 codec；编辑器直接展示 preset.name（L3930–3934）、field.label（L2557）、section.title（L4045）、column.label（L2821/L2934）、issue.message（L3433）。现有 TOML key 完整性检查无法发现绕过 catalog 的字符串。

| 应用 | 确定进入 UI 的样本位置，路径前缀为 crates/core/src/provider_config/ |
| --- | --- |
| Kimi | kimi_code.rs L40、L75、L87–95、L108、L116、L343–344；验证 L265–324 |
| Claude / Claude Desktop | claude.rs L108、L134–148；claude_desktop.rs L73、L94–100 |
| Codex | codex.rs L57、L63、L100、L113–123、L147–151 |
| Grok Build / Cherry Studio | grokbuild.rs L31–87；cherry_studio.rs L22–26、L116 |
| OpenCode / OpenClaw / Hermes | opencode.rs L66–148；openclaw.rs L56、L100–106；hermes.rs L65–69 |

修复：显示文本用稳定 key，校验用 code＋参数；保持字段 ID、协议值、品牌和用户内容。先移除 provider_editor.rs L3990–3992 对中文分节标题的行为判断，改用稳定 section ID，避免翻译改变表单逻辑。schema 帮助当前根本未展示，不把它误报成“看到了中文帮助”。证据：[英文中的中文预设](assets/ui-audit-2026-10-13/schema-untranslated.jpeg)。

### UI-11：精简说明的包装器丢掉动态信息

[layout.rs](../crates/app/src/layout.rs) L5–38 无条件丢弃 page/section 的说明、switch/select 的 description。实测设置中“启动时不显示窗口”“后台托盘驻留”变灰，却不显示调用方明确传入的依赖原因。

被吞内容包括 [settings_view/root.rs](../crates/app/src/settings_view/root.rs) L203–214、L225–233 的依赖说明，以及 [settings_view/apps.rs](../crates/app/src/settings_view/apps.rs) L61–74 的最后一个应用不能关闭说明。page_header 的同类行为还吞掉连接计数、导入来源路径和会话时间（app_ui.rs L3961–3970/L3148–3152、sessions_view.rs L1575–1582/L1653）。

修复：区分静态文档说明与动态状态/禁用原因/元数据；不要简单恢复所有冗长帮助，也不要统一丢弃。证据：[缺少禁用原因的设置页](assets/ui-audit-2026-10-13/disabled-settings.jpeg)。

### UI-12：部分开关没有 AX 身份

Claude 应用设置截图能看到两个行为开关，但 AX 只有返回、目录输入和保存按钮，没有开关名称或状态。根因：[app_settings_view.rs](../crates/app/src/app_settings_view.rs) L254–268 使用裸 row + layout::toggle + on_click。

[skills_view.rs](../crates/app/src/skills_view.rs) L1850–1856 的仓库启用开关，以及 provider_editor.rs L2870–2885/L2993–3002 的部分网格开关存在同类源码缺口。不是所有开关都有问题；Gateway L2532–2546 与本轮网络代理 AX 都有 Switch 语义。

修复：复用语义完整的 switch，传入 label、toggled、disabled，并提供键盘激活。验收要比对状态变化前后 AX。见 [应用设置画面](assets/ui-audit-2026-10-13/app-settings-switches.jpeg) 及 [AX 摘录](assets/ui-audit-2026-10-13/interaction-evidence.txt)。

### UI-13：取消/保存路由没有统一覆盖弹层

Remote → 添加 SSH 连接后按 Escape，实测弹窗保持开启。源码 [app_ui.rs](../crates/app/src/app_ui.rs) L637–684 没有 Section::Remote 的取消分支。

另外，[settings_view.rs](../crates/app/src/settings_view.rs) L597–628 的保存/取消漏掉 data_dir_editing；远程目录编辑窗由 root.rs L584–590/L969–1008 创建。app_ui.rs L597–633 的保存路由也未覆盖网络页。这两个场景尚未运行验证。

修复：最上层弹窗先消费快捷键，再路由页面，并明确忙碌状态下能否关闭。证据：[Escape 后仍开启的远程弹窗](assets/ui-audit-2026-10-13/remote-escape.jpeg)。此项与 UI-03 共用事件层级修复，但当前后果不同。

### UI-14：MCP 的失败状态伪装成空列表

[mcp_view.rs](../crates/app/src/mcp_view.rs) L132–163 没有加载状态，首次加载时 servers 为空，读取失败时也清空 servers；L736–764 只判断 is_empty 并展示“尚未配置 / 添加”。已有列表刷新期间会保留旧数据，不能说每次加载都显示空态。错误 toast 不会修正失败后页面持续表达的“未配置”。

修复：区分 Loading/Error/Empty/Loaded，失败保留旧数据并提供重试。验收延迟响应、远程断线及真实零条目。本轮只验证了真实空态与有数据态，未注入远程故障。

### UI-15：工具页的成功反馈早于结果

[tools_view.rs](../crates/app/src/tools_view.rs) L3888–3897 在点击刷新后立即设置 Success，而读取尚在后台；L236–240/L280–294 用 unwrap_or_default 丢弃应用/备份读取错误。

修复：必要读取完成后再报成功；保留错误与旧数据，并呈现部分失败。验收“旧数据存在 → 下一次读取失败”，不能只验证本地成功响应。

### UI-16：请求详情无法提供完整标识

[usage_view.rs](../crates/app/src/usage_view.rs) L3000–3023 将 request/provider/model 等放入固定三列；L4255–4267 的 detail_cell 一律 truncate，没有复制、完整查看或 tooltip。进入详情后，长 ID 仍然不可完整取得。

修复：标识支持可选换行和复制完整值；按实际宽度调整列数。验收长 request ID、不同实际/请求/计费模型。此项由渲染链路确认，未注入请求日志做截图。

### UI-17：错误区域可耗尽预览高度

[provider_editor.rs](../crates/app/src/provider_editor.rs) L3382–3391 将全部 issues 放入不可收缩且不限高的区域，外层预览却限高并裁切。Kimi 校验每个模型可产生多条错误（kimi_code.rs L283–318），不存在“错误总是很少”的上限。

修复：错误区域有高度预算和内部滚动，保留计数和字段定位；短窗口仍留出可用的表单和代码区。验收十个无效模型、长别名、多条多行错误。本轮未构造此状态，因此不声称已经测到代码高度为零。

## 3. 已排除的误报与仍需验证的候选

| 项目 | 本轮判断 |
| --- | --- |
| MCP 长名称/长 URL | 960px 英文样本能正常换行，Edit/Delete 完整。不能仅因缺少 truncate 就报越界 |
| Skills 长名称 | 已安装样本能截断名称、保留三个操作；“有更新时再多一个 Update”尚未测，不列为已确认覆盖问题 |
| Skills 顶部统计卡 | 英文说明换行造成卡片高低不齐，属于局部视觉一致性候选，优先级低于上述功能缺陷 |
| Codex 密集页头 | 960px 时操作换成两排，内容区被挤压；换行本身不是错误。需另定标题最小宽度和紧凑动作策略再验收 |
| 侧栏底部入口 | 九个应用全开启时设置/关于在视口外，但滚动可达；无明显滚动条影响可发现性，不能说入口不存在 |
| Gateway 展开后的虚拟列表 | 锁定 GPUI 会重新测量可见项；不能因调用点只 notify 就断言重叠。离屏异步更新仍需专测 |
| Usage 大数据表 | provider/model 行全部构建在一个外层 List 区块中（usage_view.rs L2628–2773、L3714–3717）；有扩展性风险，但本轮未测 FPS 或延迟 |
| Remote 长内容和额度弹窗 | 公共尺寸约束不足，但初始手动添加实际可见；较多额度/长错误需另测 |
| 1200×820 旧截图 | AX/像素不一致尚未定位；未发现宽高专属停绘分支，日志无相关 Metal 错误。不作为已确认 UI 冻结 |
| 字体、颜色、透明侧栏 | 保留项目既有 Zed 风格、Helvetica Neue 和 macOS 原生材质；没有因为通用 web 审美清单而判错。未做全部主题的对比度数值测量 |

## 4. 应保留的已有实现

- 外壳主内容已有 min_w_0/min_h_0，侧栏独立滚动；公共滚动体把滚动条固定在页面边缘。
- 编辑器已有窄窗折叠预览、字段堆叠和列表测量；问题是宽度依据与约束组合，不是完全没有响应式。
- dropdown 和模型建议菜单已有 deferred/anchored/window margin 和高度限制。
- 网络页主机/端口、用户名/密码组合有 wrap 和最小宽度；其 Switch AX 正确。
- Sessions 已逐消息虚拟化、长消息折叠，并在展开时重新测量。
- MCP、Skills、Theme 切语言后重新测量，Skills 数据/过滤变化也统一刷新列表。
- Gateway 主行和操作能换行，模型选择器有高度限制和滚轮隔离。
- 同步设置已分离草稿与已存值，并在离开时确认；可作为其他设置的参考。
- About 本轮未发现独立的高置信错位；主题字标及忙碌状态保护应保留。

## 5. 推荐修复顺序与完成标准

| 顺序 | 范围 / 归属 | 完成标准 |
| --- | --- | --- |
| 1 | 公共 Modal、焦点、弹层取消顺序：独立 ochub-ui + OcHub 宿主 | UI-01/02/03/13；960×640 内头尾可达，输入不入背景，Escape 只关闭最上层，关闭恢复焦点 |
| 2 | 草稿和异步回调：OcHub | UI-04；其他开关保存不改目录草稿，保存过程中继续输入不丢失，离开策略一致 |
| 3 | 文本/表格/预览宽度：组件库契约 + 业务组合 | UI-06/07/08/09/17；长内容不覆盖操作，图标不变形，断点按内容宽度，完整值可读 |
| 4 | 键盘与 AX：以公共控件为主 | UI-05/12；Tab/Shift-Tab/Enter/Space 可达，焦点可见，所有开关有名称、状态、禁用语义 |
| 5 | 动态文案与状态：core schema + app | UI-10/11/14/15/16；稳定翻译 key、错误 code、禁用原因保留，加载/失败/空态区分，详情可取完整标识 |

公共组件应修在独立 ochub-ui 仓库并升级 OcHub 的依赖 pin；不要在主仓库复制一套组件实现。每批 GPUI 修复后按 AGENTS.md 运行 just qa-app，覆盖同一个固定包，执行对应原生回归，退出但保留包。若修复改变用户操作或功能，在相邻 ochub-docs 更新文档并验证、提交。本轮审计没有改变操作或功能，因此没有修改用户文档站。

不建议一次性重做所有页面。先形成可复用的 Modal、可收缩信息行、共享表格列、完整值查看、Switch 和状态视图，再迁移业务调用。回归应围绕实际用户不变量，避免只断言源码中出现某个样式方法。

## 6. 与应用插件化的衔接

用户已明确目标：新增受管应用只安装插件，无需修改 OcHub 主程序。此前已实测自定义插件在管理页显示启用但侧栏不出现；见 [Config 与插件审计](CONFIG-PLUGIN-AUDIT-2026-10-13.md)。这属于既有功能缺口，不计入本轮新增 17 组问题。

UI 重构应同时保留这一目标：页面身份使用 AppId 和动态描述/能力；schema 的显示 key、字段稳定 ID、校验 code 进入插件契约；菜单与动作按能力生成。尤其不能继续根据中文 section.title 或 AppType 枚举控制动态表单行为，否则修好当前内置页面后，新插件仍需要修改主程序。

## 7. 取证索引

关键截图文件、源文件名和 SHA-256 见 [evidence.json](assets/ui-audit-2026-10-13/evidence.json)。交互轨迹见 [interaction-evidence.txt](assets/ui-audit-2026-10-13/interaction-evidence.txt)。这些是本轮实际截图与 AX 输出，没有生成“修复后”效果图。

对运行证据不足的项目，后续应使用本报告的触发条件补测；不要把静态推理升级为已通过验收。当前所有列出的问题仍待修复。
