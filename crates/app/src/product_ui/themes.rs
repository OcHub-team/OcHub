use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div, prelude::*};

use crate::{components, layout, theme};

pub struct ThemeCard {
    pub preview: AnyElement,
    pub selected: bool,
    pub name: SharedString,
    pub description: SharedString,
    pub badge: AnyElement,
    pub actions: AnyElement,
}

pub fn theme_card(spec: ThemeCard) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(if spec.selected {
            theme::accent()
        } else {
            theme::border()
        })
        .bg(theme::surface())
        .when(spec.selected, |card| card.shadow(theme::shadow_hover()))
        .child(spec.preview)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(spec.name),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(spec.description),
                                ),
                        )
                        .child(spec.badge),
                )
                .child(spec.actions),
        )
}

pub fn mode_block(
    section_title: impl Into<SharedString>,
    row_label: impl Into<SharedString>,
    mode_control: impl gpui::IntoElement,
    library_title: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .pb_3()
        .child(layout::section_header(section_title, None))
        .child(
            components::card()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_4()
                .child(layout::row_label(row_label, None))
                .child(mode_control),
        )
        .child(layout::section_header(library_title, None))
}

pub fn card_row(cards: impl IntoIterator<Item = AnyElement>, fill_second: bool) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .w_full()
        .pb_3()
        .children(cards)
        .when(fill_second, |row| row.child(div().flex_1().min_w_0()))
}
