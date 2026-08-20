use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div, prelude::*, px};

use crate::{components, theme};

pub struct StationCard {
    pub editing: bool,
    pub identity_icon: AnyElement,
    pub name: SharedString,
    pub badges: Vec<AnyElement>,
    pub base_url: SharedString,
    pub endpoint_summary: Option<SharedString>,
    pub website: Option<SharedString>,
    pub model_summary: SharedString,
    pub quota: Option<AnyElement>,
    pub controls: Vec<AnyElement>,
}

/// Gateway station card. Endpoint discovery, quota loading, and all mutations
/// remain in the surrounding product view.
pub fn station_card(spec: StationCard) -> gpui::Div {
    components::card()
        .gap_3()
        .when(spec.editing, |panel| {
            panel.opacity(components::DISABLED_OPACITY)
        })
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(260.))
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(spec.identity_icon)
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_base()
                                        .font_weight(FontWeight::BOLD)
                                        .child(spec.name),
                                )
                                .children(spec.badges),
                        )
                        .child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .truncate()
                                .child(spec.base_url),
                        )
                        .children(spec.endpoint_summary.map(|summary| {
                            div().text_color(theme::muted()).text_xs().child(summary)
                        }))
                        .children(spec.website.map(|website| {
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .truncate()
                                .child(website)
                        }))
                        .child(
                            div()
                                .text_color(theme::subtext())
                                .text_xs()
                                .child(spec.model_summary),
                        )
                        .children(spec.quota),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap_2()
                        .children(spec.controls),
                ),
        )
}
