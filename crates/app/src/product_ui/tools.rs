use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div};

use crate::{components, theme};

pub struct McpCard {
    pub name: SharedString,
    pub endpoint: SharedString,
    pub description: Option<SharedString>,
    pub apps_label: SharedString,
    pub actions: Vec<AnyElement>,
    pub app_toggles: Vec<AnyElement>,
}

pub fn mcp_card(spec: McpCard) -> gpui::Div {
    components::card()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_color(theme::text())
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(spec.name),
                        )
                        .child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(spec.endpoint),
                        )
                        .children(spec.description.map(|description| {
                            div()
                                .text_color(theme::subtext())
                                .text_xs()
                                .child(description)
                        }))
                        .child(
                            div()
                                .text_color(theme::teal())
                                .text_xs()
                                .child(spec.apps_label),
                        ),
                )
                .child(div().flex().flex_row().gap_2().children(spec.actions)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_3()
                .children(spec.app_toggles),
        )
}
