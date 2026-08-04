//! Dev-only component gallery (`MS_GALLERY=1`): renders every shared
//! component in every state so visual tuning and regression checks don't
//! require clicking through the whole app (GPUI has no hot reload).

use gpui::{Context, FontWeight, ScrollHandle, SharedString, Window, div, prelude::*, px};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

pub struct GalleryView {
    demo_input: gpui::Entity<TextInput>,
    secret_input: gpui::Entity<TextInput>,
    scroll_handle: ScrollHandle,
}

impl GalleryView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            demo_input: cx.new(|cx| TextInput::new(cx, "https://example.com")),
            secret_input: cx.new(|cx| TextInput::new(cx, "sk-ant-…")),
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn demo_row(children: Vec<gpui::AnyElement>) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap_2()
            .children(children)
    }

    fn section(title: &'static str, description: &'static str, body: gpui::Div) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(layout::section_header(
                title,
                Some(SharedString::from(description)),
            ))
            .child(body)
    }
}

impl gpui::Render for GalleryView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = components::card()
            .gap_3()
            .child(Self::demo_row(vec![
                components::button("g-b1", "主要操作", ButtonTone::Primary, ButtonSize::Sm)
                    .into_any_element(),
                components::button("g-b2", "次要操作", ButtonTone::Neutral, ButtonSize::Sm)
                    .into_any_element(),
                components::button("g-b3", "危险操作", ButtonTone::Danger, ButtonSize::Sm)
                    .into_any_element(),
                components::button("g-b4", "幽灵按钮", ButtonTone::Ghost, ButtonSize::Sm)
                    .into_any_element(),
            ]))
            .child(Self::demo_row(vec![
                components::button("g-b5", "中号主要", ButtonTone::Primary, ButtonSize::Md)
                    .into_any_element(),
                components::button("g-b6", "中号次要", ButtonTone::Neutral, ButtonSize::Md)
                    .into_any_element(),
                components::icon_button_tone(
                    "g-b7",
                    "刷新",
                    IconName::Refresh,
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .into_any_element(),
                components::icon_button_tone(
                    "g-b8",
                    "新建",
                    IconName::Add,
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .into_any_element(),
            ]));

        let fields = components::card()
            .gap_4()
            .child(components::field(
                "网站 URL",
                false,
                Some("仅用于展示，不会写入任何配置。".into()),
                self.demo_input.clone(),
            ))
            .child(components::field(
                "API Key",
                true,
                Some("写入所选鉴权变量；留空则保持不变。".into()),
                self.secret_input.clone(),
            ))
            .child(components::field_row(
                "随应用自动启动",
                Some("启动 OcHub 时自动拉起网关（端点地址保持稳定）。".into()),
                layout::toggle(true),
            ));

        let segmented_demo = components::card()
            .gap_3()
            .child(Self::demo_row(vec![
                components::segmented(
                    "g-seg-1",
                    &["AUTH_TOKEN (Bearer)", "API_KEY (x-api-key)"],
                    0,
                    |_, _, _| {},
                )
                .into_any_element(),
            ]))
            .child(Self::demo_row(vec![
                components::segmented(
                    "g-seg-2",
                    &["messages", "chat", "responses"],
                    1,
                    |_, _, _| {},
                )
                .into_any_element(),
            ]));

        let badges = components::card()
            .gap_3()
            .child(Self::demo_row(vec![
                components::badge(BadgeTone::Neutral, "默认").into_any_element(),
                components::badge(BadgeTone::Accent, "当前").into_any_element(),
                components::badge(BadgeTone::Success, "已启用").into_any_element(),
                components::badge(BadgeTone::Warning, "可更新").into_any_element(),
                components::badge(BadgeTone::Danger, "异常").into_any_element(),
                components::badge(BadgeTone::Teal, "claude").into_any_element(),
                components::badge(BadgeTone::Mauve, "opencode").into_any_element(),
                components::badge(BadgeTone::Peach, "gemini").into_any_element(),
            ]))
            .child(Self::demo_row(vec![
                components::status_dot(theme::green()).into_any_element(),
                components::status_dot(theme::yellow()).into_any_element(),
                components::status_dot(theme::red()).into_any_element(),
                components::status_dot(theme::muted()).into_any_element(),
                components::status_dot_sized(theme::accent(), 6.).into_any_element(),
            ]));

        let cards = layout::group(vec![
            layout::row()
                .child(layout::row_label(
                    "要求 API key",
                    Some("推理端点要求本地 key，用量按 key 归因。".into()),
                ))
                .child(layout::toggle(true))
                .into_any_element(),
            layout::row()
                .child(layout::row_label(
                    "随应用自动启动",
                    Some("启动 OcHub 时自动拉起网关（端点地址保持稳定）。".into()),
                ))
                .child(layout::toggle(false))
                .into_any_element(),
        ]);

        let empty = components::card().p_0().child(components::empty_state(
            IconName::Folder,
            "尚无上游",
            "新建上游并选择接口格式（messages / chat / responses）。",
            Some(
                components::button(
                    "g-empty-cta",
                    "新建上游",
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .into_any_element(),
            ),
        ));

        let modal_demo = components::modal_overlay(
            components::modal_card()
                .child(components::modal_header("删除供应商"))
                .child(
                    components::modal_body().child(
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("确定删除 Fox-aws 吗？该操作不可撤销。"),
                    ),
                )
                .child(components::modal_footer(vec![
                    components::button("g-m1", "取消", ButtonTone::Neutral, ButtonSize::Sm)
                        .into_any_element(),
                    components::button("g-m2", "删除", ButtonTone::Danger, ButtonSize::Sm)
                        .into_any_element(),
                ])),
        );
        // Inline (non-overlay) preview of the modal card so the gallery page
        // stays scrollable; the overlay wrapper is demonstrated by the frame.
        let modal_frame = div()
            .relative()
            .h(px(280.))
            .rounded_lg()
            .overflow_hidden()
            .child(modal_demo);

        let disclosures = components::card().gap_2().p_0().py_2().child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .child(components::disclosure(
                    "g-d1",
                    "网络设置",
                    Some("监听地址、超时与重试策略".into()),
                    true,
                ))
                .child(
                    div()
                        .pl_6()
                        .text_color(theme::muted())
                        .text_xs()
                        .child("展开状态的正文区域。"),
                )
                .child(components::disclosure(
                    "g-d2",
                    "流式健康检测",
                    Some("对SSE 流做周期性探测与自愈".into()),
                    false,
                )),
        );

        let stats = div()
            .grid()
            .grid_cols(4)
            .gap_3()
            .child(components::stat_tile(
                None,
                theme::green(),
                "网关状态",
                "运行中",
                "http://127.0.0.1:4180",
            ))
            .child(components::stat_tile(
                Some(IconName::Message),
                theme::accent(),
                "请求",
                "1,248",
                "成功率 99.1%",
            ))
            .child(components::stat_tile(
                Some(IconName::Diamond),
                theme::peach(),
                "总成本",
                "$3.42",
                "输入 1.2M / 输出 80K",
            ))
            .child(components::stat_tile(
                Some(IconName::Cloud),
                theme::teal(),
                "缓存命中",
                "86.4%",
                "重复输入复用比例",
            ));

        let table = components::card().p_0().child(
            div()
                .flex()
                .flex_col()
                .child(components::table_header(&[
                    "角色",
                    "模型 ID",
                    "显示名",
                    "1M 上下文",
                ]))
                .child(components::table_row(
                    vec![
                        div().text_sm().child("sonnet").into_any_element(),
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("claude-sonnet-4-6")
                            .into_any_element(),
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("Sonnet 4.6")
                            .into_any_element(),
                        layout::toggle(false).into_any_element(),
                    ],
                    4,
                    false,
                ))
                .child(components::table_row(
                    vec![
                        div().text_sm().child("opus").into_any_element(),
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("claude-opus-4-5")
                            .into_any_element(),
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("Opus 4.5")
                            .into_any_element(),
                        layout::toggle(true).into_any_element(),
                    ],
                    4,
                    true,
                )),
        );

        let pagination = components::card().child(components::pagination(
            components::button("g-p1", "‹ 上一页", ButtonTone::Neutral, ButtonSize::Sm)
                .into_any_element(),
            "第 3 / 12 页",
            components::button("g-p2", "下一页 ›", ButtonTone::Neutral, ButtonSize::Sm)
                .into_any_element(),
        ));

        let type_ramp = components::card()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text())
                    .child("Display 页面大标题 (xl)"),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text())
                    .child("Title 页面标题 (lg)"),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child("Heading 区块标题 (sm/semibold)"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text())
                    .child("Body 正文与行标签 (sm) — 用于大多数内容。"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Caption 说明文字 (xs) — 用于帮助文本、表头与徽章。"),
            );

        layout::page()
            .child(layout::page_header(
                "组件画廊",
                Some(SharedString::from("MS_GALLERY=1 · 共享组件全状态预览")),
            ))
            .child(layout::scroll_body(
                "gallery-scroll",
                &self.scroll_handle,
                layout::content_column()
                    .child(Self::section("按钮", "tone × size,统一悬停与字重", buttons))
                    .child(Self::section(
                        "表单字段",
                        "竖排 field / 必填 / 横排 field_row",
                        fields,
                    ))
                    .child(Self::section(
                        "分段选择器",
                        "替代所有手搓 pill/chip 选择器",
                        segmented_demo,
                    ))
                    .child(Self::section("徽章与状态点", "八种 tone + 圆点", badges))
                    .child(Self::section(
                        "分组卡片",
                        "layout::group + row + toggle",
                        div().child(cards),
                    ))
                    .child(Self::section(
                        "空状态",
                        "图标 + 标题 + 提示 + 可选 CTA",
                        empty,
                    ))
                    .child(Self::section(
                        "模态",
                        "modal_overlay + modal_card + header/body/footer",
                        modal_frame,
                    ))
                    .child(Self::section(
                        "折叠区块",
                        "disclosure 展开/收起",
                        disclosures,
                    ))
                    .child(Self::section(
                        "指标卡",
                        "stat_tile:圆点或图标两种标记",
                        div().child(stats),
                    ))
                    .child(Self::section("表格", "table_header + table_row", table))
                    .child(Self::section("分页", "pagination 页脚条", pagination))
                    .child(Self::section(
                        "字体阶梯",
                        "display / title / heading / body / caption",
                        type_ramp,
                    )),
            ))
    }
}
