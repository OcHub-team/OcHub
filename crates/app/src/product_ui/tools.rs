use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div, prelude::*};

use crate::theme;

pub struct McpCard {
    pub name: SharedString,
    pub endpoint: SharedString,
    pub description: Option<SharedString>,
    pub apps_label: SharedString,
    pub expanded: bool,
    pub disclosure: AnyElement,
    pub actions: Vec<AnyElement>,
    pub app_toggles: Vec<AnyElement>,
}

pub fn mcp_card(spec: McpCard) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .py_3()
        .px_3()
        .border_b_1()
        .border_color(theme::border())
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
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_color(theme::text())
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(spec.name),
                        )
                        .child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(spec.apps_label),
                        ),
                )
                .child(spec.disclosure),
        )
        .when(spec.expanded, |card| {
            card.children(spec.description.map(|description| {
                div()
                    .text_color(theme::subtext())
                    .text_sm()
                    .child(description)
            }))
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(spec.endpoint),
            )
            .child(div().flex().flex_wrap().gap_3().children(spec.app_toggles))
            .child(div().flex().justify_end().gap_2().children(spec.actions))
        })
}
