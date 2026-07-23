# OCHUB UI 重构设计文档

> 状态：**已完成**（Phase 1-5 全部落地于 `ui-overhaul` 分支）。
> 方向：**保持 Zed 风做精做细 · 视觉与信息架构一起改 · Theme 运行时化（只实装浅色，预留深色）**。
> 验收：`cargo check --workspace` 零警告；全部 14 个视图 + 外壳完成迁移并逐页截图核验。

## 1. 目标与非目标

**目标**
- 建立完整的设计令牌（token）体系与组件库，消灭各视图手搓样式。
- 统一页面骨架与版心，消灭"800px 居中 vs 全宽手搓"两套体系。
- 信息架构升级：侧边栏分组合理化、编辑供应商页重组、网关页仪表盘化。
- 为深色模式建立运行时 Theme 架构（本次不实装深色色板）。

**非目标**
- 不改动 `core`/`server` 任何逻辑；视图与后端交互代码原样保留。
- 不追求风格突变：配色保持现有暖沙中性色 + 蓝 accent。
- 不做品牌重塑、不引入新字体（继续 Helvetica Neue）。

## 2. 现状核心问题（扫描结论）

| 问题 | 现状 |
|---|---|
| 状态条 | 14 份逐行拷贝，仅 1 处用 `status_banner` |
| 表单字段行 | 8 个变体（颜色/字重/间距各异），无共享 `Field` |
| 按钮 | 50+ 手搓、8 份本地 `action_button` 包装、尺寸分裂（py_1/1p5/2） |
| 开关/选择器 | 三套并存：`layout::toggle`、GREEN pill 开关、8 种各自实现的 pill 选择器 |
| 徽章/状态点 | 圆角/底色混用，圆点 4 种尺寸，无共享组件 |
| 卡片 | `layout::group` 仅 2 视图、`components::panel` 仅 1 视图，~50 处手搓 |
| 表格 | 2 套实现（usage `table_shell` / provider_editor `render_grid`） |
| 空状态 | ~15 处纯文本，无图标无 CTA |
| 模态 | 仅 1 个真模态；**删除确认全应用 0 处**（点击即删） |
| 折叠区块 | 3 份雷同 disclosure |
| 指标 tile | 4 个变体 |
| 页面骨架 | 一半视图手搓 header/body；mcp/prompts/sessions 完全不用 components |

## 3. 设计令牌（Token）

### 3.1 色彩（值不变，语义化重组）

| 令牌 | 现值 | 用途 |
|---|---|---|
| `bg` | `0xfcfcfb` | 窗口画布 |
| `mantle` | `0xf4f4f2` | 侧边栏/次面板 |
| `surface` | `0xffffff` | 卡片 |
| `surface_hover` | `0xeeeeea` | 悬停/选中幽灵层 |
| `panel` | `0xf8f8f6` | 分组控件面板 |
| `inset` | `0xf1f1ed` | 中性按钮/内陷填充 |
| `border` / `border_strong` | `0xe7e6e2` / `0xd6d5d0` | 发丝线 / 强分隔 |
| `text` / `subtext` / `muted` | `0x222019` / `0x6b6a64` / `0x91908a` | 三级文字 |
| `accent` / `accent_hover` / `accent_soft` / `accent_text` | `0x2563dd` … | 交互主色 |
| `success` / `warning` / `danger`(+`_soft`) | green/yellow/red 系 | 状态色 |
| `teal` / `mauve` / `peach` | 现有 | 辅助标记色 |
| `sidebar_*`、`header` | 现有 | 外壳专用 |

### 3.2 字阶（语义化，收敛现有散落的 text_xs/sm/xl）

| 档位 | px | 字重 | 用途 |
|---|---|---|---|
| `display` | 20 | BOLD | 页面大标题（hero） |
| `title` | 16 | BOLD | 页面标题（page_header） |
| `heading` | 14 | SEMIBOLD | 区块标题（section_header） |
| `body` | 13 | NORMAL/MEDIUM | 正文、行标签、按钮 |
| `caption` | 12 | NORMAL/MEDIUM | 说明文字、徽章、表头 |
| `mono` | 12 | — | 代码/路径/key（等宽） |

### 3.3 间距 / 圆角 / 阴影
- 间距：4pt 网格，直接映射 GPUI 的 `gap_1..gap_6`、`p_2..p_8`；页面级节奏：卡片内 16，区间 12，节间 24。
- 圆角：`md`(6) 控件、`lg`(8) 卡片、`full` 徽章/开关——保持现状，文档化。
- 阴影：`shadow_panel`（卡片近无影）、`shadow_hover`、`shadow_popover`（仅浮层/模态），已有，不变。

## 4. Theme 运行时架构（Phase 1）

```
pub struct Theme { pub bg: u32, pub mantle: u32, … }   // 全部现为字段
pub const LIGHT: Theme = …;                             // 唯一实装色板
static CURRENT: RwLock<Theme>;                          // OnceLock 初始化,深色时整体替换
pub fn bg() -> Rgba; pub fn text() -> Rgba; …           // 访问函数,内部 rgb()
```

- 调用点迁移：`theme::c(theme::BG)` → `theme::bg()`；`theme::translucent(theme::MANTLE, a)` → `theme::mantle().alpha(a)`。
- `theme::c()`/`translucent()` 保留给非 token 的局部色值使用。
- Phase 1 是纯重构：**视觉零变化**，单独 commit，截图对比验收。

## 5. 页面骨架与版心

统一为 `layout::page` + `page_header`（标题/副标题/右侧操作槽）+ 滚动 body：

| 版心 | 宽度 | 适用 |
|---|---|---|
| `content_column()` | 800 居中 | 表单/设置类：设置、认证、MCP、提示词、技能、会话、编辑供应商 |
| `wide_column()`（新增） | 1080 居中 | 数据密集：用量、高级工具、网关、供应商列表 |

消灭多处手搓全宽 body（gateway/usage/tools/app_ui/provider_editor）与 5 处手搓 page_header。

## 6. 组件清单（Phase 2）

| # | 组件 | 消灭的重复 | 要点 |
|---|---|---|---|
| C1 | `status_banner(level, msg)` | 14 份状态条拷贝 | Info/Success/Error 三级；视图底部统一放置 |
| C2 | `field(label, req, help, control)` | 8 个表单变体 | 竖排 label(caption SUBTEXT)+控件+help；另有 `field_row` 横排版 |
| C3 | 按钮体系收敛 | 50+ 手搓/8 包装 | 尺寸 sm/md 两档；tone: Primary/Neutral/Danger/Ghost；删除全部本地包装 |
| C4 | `segmented(options, selected)` | 8 种 pill 选择器 | INSET 底 + 选中 SURFACE 带发丝边；用于鉴权变量、方言、app 切换 |
| C5 | `toggle(on)` 唯一开关 | GREEN pill 开关 | 全部迁到 `layout::toggle` 滑块，文案移到 `row_label` |
| C6 | `badge(tone, label)` + `status_dot(size)` | 徽章/圆点乱象 | tone 六色 soft 底；dot sm(6)/md(8) 两档 |
| C7 | `card()` / `group(rows)` 唯一卡片体系 | ~50 处手搓 | 圆角 lg、BORDER 发丝、p_4；`panel` 并入 |
| C8 | `table(headers, rows)` | 2 套表格 | grid 对齐、caption 表头、行 hover；行可点变体 |
| C9 | `empty_state(icon, title, hint, cta?)` | 15 处纯文本 | 居中、MUTED 图标+标题+提示+可选按钮 |
| C10 | `modal()` / `confirm_dialog()` | 1 模态 + 0 删除确认 | 从 raw modal 提炼；**为全部删除操作补确认** |
| C11 | `disclosure(title, detail, expanded)` | 3 份折叠 | 卡式头 + chevron |
| C12 | `stat_tile(icon?, label, value, detail?)` | 4 个变体 | 用于工具/用量/网关状态 |
| C13 | `pagination()` | 2 套分页 | footer 式，统一会话与用量 |
| C14 | 组件画廊 `gallery_view.rs` | — | dev-only（`MS_GALLERY=1` 时侧边栏出现入口），渲染全部组件全部状态 |

## 7. 信息架构决策

1. **侧边栏分组**：应用 / 工具（MCP·技能·用量·会话·高级工具）/ **网络（中转网关）** / 系统（主题·设置）。
2. **编辑供应商**：保持整页双栏（左表单、右文件预览），预览栏 sticky；表单分区用统一 `section_header`+`field`；鉴权变量二选一改 `segmented`；**角色模型映射由表格改为行卡片列表**（每行：角色徽标 + 模型 ID 全宽输入 + 显示名 + 1M 开关 + 删除），根治截断。
3. **供应商列表（首页）**：迁入 page 骨架 + `wide_column`；hero 卡与供应商卡用统一 `card`/`badge`。
4. **网关页**：状态区升级为 `stat_tile` 仪表盘行（运行状态/端点/渠道/密钥）；渠道列表卡片化；编辑器沿用内联卡但换统一 `field`/`segmented`。
5. **删除确认**：所有破坏性操作（供应商/MCP/提示词/技能/会话/渠道/key/备份/定价）接入 `confirm_dialog`。
6. **列表↔表单双模式**：5 套自管返回逻辑统一为 `page_header` 左侧"← 返回"槽。

## 8. 执行顺序

1. **Phase 1** Theme 运行时化（纯重构，单独 commit）
2. **Phase 2** 组件库 + 画廊（每组件一 commit）
3. **Phase 3** 外壳：titlebar/侧边栏/首页供应商列表
4. **Phase 4** 编辑供应商（试点，验证组件库）
5. **Phase 5** 铺开：网关 → 设置/应用设置 → 认证 → MCP/提示词/技能 → 会话 → 工具 → 用量
6. 每页：`just check` 通过 + 运行截图对比；只动 render 层。

## 9. 工程纪律

- 分支 `ui-overhaul`；Phase 1 独立 commit；之后每组件/每页一 commit。
- 任何一页改坏可单独回滚；全程 `cargo check -p ochub-app` 快速迭代。
