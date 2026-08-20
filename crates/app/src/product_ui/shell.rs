use gpui::{
    ElementId, FontWeight, IntoElement, ParentElement, SharedString, Styled, WindowAppearance, div,
    prelude::*, px,
};

use crate::icons::{IconName, icon};
use crate::theme;

/// The canonical width of the OcHub desktop navigation rail.
pub const SIDEBAR_WIDTH: f32 = 252.;

/// Base of the application root. Hosts add key contexts, actions, native
/// title bars, overlays, and notifications to this element.
pub fn app_root() -> gpui::Stateful<gpui::Div> {
    div()
        .id("app-root")
        .flex()
        .flex_col()
        .size_full()
        .bg(theme::window_base_background())
        .text_color(theme::text())
        .font_family("Helvetica Neue")
        .relative()
}

/// The row below any native title bar: canonical sidebar plus page content.
pub fn app_body(sidebar: impl IntoElement, content: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.))
        .child(sidebar)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h(px(0.))
                .child(content),
        )
}

/// Canonical sidebar frame. The host supplies its native drag strip and the
/// navigation body because scrolling and window movement are host concerns.
pub fn sidebar(
    appearance: WindowAppearance,
    top_chrome: impl IntoElement,
    navigation: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("sidebar")
        .relative()
        .flex()
        .flex_col()
        .h_full()
        .w(px(SIDEBAR_WIDTH))
        .flex_shrink_0()
        .bg(theme::sidebar_background())
        .text_color(theme::sidebar_glass_text(appearance))
        .border_r_1()
        .border_color(theme::border())
        .shadow_xs()
        .child(top_chrome)
        .child(navigation)
}

pub fn group_label(label: impl Into<SharedString>, appearance: WindowAppearance) -> gpui::Div {
    div()
        .mt_4()
        .mb_1()
        .px_3()
        .text_color(theme::sidebar_glass_muted(appearance))
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

/// Visual base for an application switcher row. Hosts attach identity,
/// accessibility metadata, and the click handler.
pub fn app_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    accent: u32,
    selected: bool,
    appearance: WindowAppearance,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .pl_2()
        .pr_2()
        .py_1()
        .rounded_lg()
        .cursor_pointer()
        .text_color(if selected {
            theme::sidebar_text()
        } else {
            theme::sidebar_glass_muted(appearance)
        })
        .when(selected, |row| {
            row.bg(theme::accent_soft()).font_weight(FontWeight::MEDIUM)
        })
        .when(!selected, |row| {
            row.hover(|hover| {
                hover
                    .bg(theme::surface_hover())
                    .text_color(theme::sidebar_text())
            })
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(24.))
                .h(px(24.))
                .rounded_md()
                .bg(theme::c(accent))
                .shadow_xs()
                .child(icon(icon_name, theme::accent_text(), 15.)),
        )
        .child(div().text_sm().child(label.into()))
}

/// Visual base for a tool/system navigation row. Hosts attach identity,
/// accessibility metadata, optional badges, and the click handler.
pub fn nav_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    selected: bool,
    appearance: WindowAppearance,
) -> gpui::Stateful<gpui::Div> {
    let foreground = if selected {
        theme::accent()
    } else {
        theme::sidebar_glass_muted(appearance)
    };
    div()
        .id(id)
        .role(gpui::Role::Button)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .pl_2()
        .pr_2()
        .py_1p5()
        .rounded_lg()
        .cursor_pointer()
        .text_sm()
        .text_color(foreground)
        .when(selected, |row| {
            row.bg(theme::accent_soft()).font_weight(FontWeight::MEDIUM)
        })
        .when(!selected, |row| {
            row.hover(|hover| {
                hover
                    .bg(theme::surface_hover())
                    .text_color(theme::sidebar_text())
            })
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(20.))
                .h(px(20.))
                .child(icon(icon_name, foreground, 15.)),
        )
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}
