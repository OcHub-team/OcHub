use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div, prelude::*, px};

use crate::components::{self, BadgeTone};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::theme;

pub struct ProviderCard {
    pub id: SharedString,
    pub name: SharedString,
    pub endpoint: SharedString,
    pub is_current: bool,
    pub is_drag_source: bool,
    pub drag_handle: Option<AnyElement>,
    pub quota: Option<AnyElement>,
    pub current_label: SharedString,
    pub actions: Vec<AnyElement>,
    pub secondary_actions: Vec<AnyElement>,
}

/// Connection card used by the provider list.
pub fn provider_card(spec: ProviderCard) -> gpui::Stateful<gpui::Div> {
    div()
        .id(spec.id)
        .relative()
        .opacity(if spec.is_drag_source { 0. } else { 1. })
        .flex()
        .flex_col()
        .w_full()
        .overflow_hidden()
        .border_b_1()
        .border_color(theme::border())
        .child(
            div()
                .flex()
                .items_stretch()
                .w_full()
                .when_some(spec.drag_handle, |card, handle| card.child(handle))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .gap_3()
                        .px_3()
                        .py_3()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .min_w_0()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(theme::text())
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(spec.name),
                                        )
                                        .when(spec.is_current, |row| {
                                            row.child(components::badge(
                                                BadgeTone::Accent,
                                                spec.current_label,
                                            ))
                                        }),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme::muted())
                                        .text_xs()
                                        .child(spec.endpoint),
                                )
                                .children(spec.quota),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_none()
                        .gap_2()
                        .py_3()
                        .pr_2()
                        .children(spec.actions),
                ),
        )
        .when(!spec.secondary_actions.is_empty(), |row| {
            row.child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .pb_3()
                    .children(spec.secondary_actions),
            )
        })
}

pub struct ActiveProviderHero {
    pub icon: IconName,
    pub accent: u32,
    pub is_gateway: bool,
    pub current_label: SharedString,
    pub direct_label: SharedString,
    pub name: Option<SharedString>,
    pub endpoint: Option<SharedString>,
    pub quota: Option<AnyElement>,
    pub empty_title: SharedString,
    pub empty_hint: SharedString,
    pub actions: Vec<AnyElement>,
}

pub fn active_provider_hero(spec: ActiveProviderHero) -> gpui::Div {
    let has_current = spec.name.is_some();
    let icon_tile = div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(32.))
        .h(px(32.))
        .child(icon(
            if spec.is_gateway {
                IconName::Layers
            } else {
                spec.icon
            },
            if has_current {
                theme::c(spec.accent)
            } else {
                theme::muted()
            },
            23.,
        ));
    let info = if let Some(name) = spec.name {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::accent())
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(spec.current_label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(spec.direct_label),
                    ),
            )
            .child(
                div()
                    .text_color(theme::text())
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(icon(
                        if spec.is_gateway {
                            IconName::Layers
                        } else {
                            IconName::Cloud
                        },
                        theme::muted(),
                        12.,
                    ))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .truncate()
                            .child(spec.endpoint.unwrap_or_else(|| SharedString::from("—"))),
                    ),
            )
            .children(spec.quota)
    } else {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(spec.current_label),
            )
            .child(
                div()
                    .text_color(theme::text())
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(spec.empty_title),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(spec.empty_hint),
            )
    };

    components::panel()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .w_full()
        .px_4()
        .py_3()
        .child(icon_tile)
        .child(info)
        .children(spec.actions)
}

/// Product-owned page chrome for the provider list.
pub fn provider_list_page(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    actions: impl IntoElement,
    body: impl IntoElement,
) -> gpui::Div {
    layout::page()
        .child(layout::page_header(title, Some(subtitle.into())).child(actions))
        .child(body)
}
