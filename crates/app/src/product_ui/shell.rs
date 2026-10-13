use gpui::{
    ElementId, IntoElement, ParentElement, SharedString, Styled, WindowAppearance, div, prelude::*,
    px,
};

use crate::icons::IconName;
use crate::theme;

/// The canonical width of the OcHub desktop navigation rail.
pub const SIDEBAR_WIDTH: f32 = 220.;

pub fn content_viewport(window: &gpui::Window) -> gpui::Size<gpui::Pixels> {
    let mut available = window.viewport_size();
    available.width = (available.width - px(SIDEBAR_WIDTH)).max(px(0.));
    available.height =
        (available.height - ochub_ui::window_chrome::title_bar_height(window)).max(px(0.));
    available
}

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
        .font_family(if cfg!(target_os = "macos") {
            "Helvetica Neue"
        } else {
            ".SystemUIFont"
        })
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
        .bg(theme::panel())
        .text_color(theme::sidebar_glass_text(appearance))
        .border_r_1()
        .border_color(theme::border())
        .child(top_chrome)
        .child(navigation)
}

pub fn group_label(label: impl Into<SharedString>, _appearance: WindowAppearance) -> gpui::Div {
    ochub_ui::navigation::section_label(label)
}

pub fn app_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    _accent: u32,
    selected: bool,
    _appearance: WindowAppearance,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::navigation::item(id, label, icon_name, selected).h(px(32.))
}

pub fn nav_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    selected: bool,
    _appearance: WindowAppearance,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::navigation::item(id, label, icon_name, selected).h(px(34.))
}
