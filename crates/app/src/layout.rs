//! Shared page-layout primitives — the single Surge-style base every view sits on.
//!
//! A view is a full-height column: a [`page_header`] bar (bold title, optional muted
//! subtitle, optional right-aligned actions) above a [`scroll_body`] — a vertically
//! scrolling region that horizontally centers a fixed-max-width [`content_column`].
//! Config-style views additionally use [`section_header`] + [`group`] to render
//! iOS/Surge grouped cards with inset hairline dividers, and [`row`] / [`row_label`]
//! / [`toggle`] for the rows inside those cards.
//!
//! Keeping all of this in one module means pages line up (same column width, same
//! header chrome, same card rhythm) instead of each view hand-rolling its own.

use gpui::{div, prelude::*, px, AnyElement, ElementId, FontWeight, SharedString};

use crate::theme;

/// Max width of the centered content column, shared by every view so pages align.
pub const CONTENT_MAX_WIDTH: f32 = 800.;

/// Outermost page container: a full-height flex column on the window background.
pub fn page() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(theme::bg())
}

/// A page header bar: bold title, optional muted subtitle, and a trailing slot for
/// action buttons. Chain `.child(...)` onto the returned element to add actions —
/// `justify_between` pushes them to the trailing edge.
pub fn page_header(title: impl Into<SharedString>, subtitle: Option<SharedString>) -> gpui::Div {
    let mut title_col = div().flex().flex_col().gap_1().child(
        div()
            .text_color(theme::text())
            .text_xl()
            .font_weight(FontWeight::BOLD)
            .child(title.into()),
    );
    if let Some(subtitle) = subtitle {
        title_col = title_col.child(div().text_color(theme::muted()).text_xs().child(subtitle));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .px_6()
        .py_4()
        .border_b_1()
        .border_color(theme::border())
        .child(title_col)
}

/// The scrollable body: a vertically scrolling region that horizontally centers its
/// content. Pass the column built via [`content_column`] (or any centered child).
pub fn scroll_body(
    id: impl Into<ElementId>,
    column: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .items_center()
        .p_6()
        .min_w_0()
        .overflow_y_scroll()
        .child(column)
}

/// A **virtualized** scrolling body, centered at [`CONTENT_MAX_WIDTH`] like
/// [`scroll_body`] but backed by `gpui::list` so only the visible items (plus a
/// little overdraw) are laid out and painted — the fix for long pages that drop
/// frames when every off-screen row (especially text inputs) was being relaid each
/// frame. The caller owns a `gpui::ListState` and supplies the `list` element built
/// with `gpui::list(state, cx.processor(|this, ix, window, cx| ...))`; each item
/// should carry its own bottom spacing (the list draws no inter-item gap).
pub fn virtual_body(list: gpui::List) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_h_0()
        .w_full()
        .min_w_0()
        .px_6()
        .child(
            list.with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                .flex_1()
                .min_h_0()
                .w_full()
                .max_w(px(CONTENT_MAX_WIDTH))
                .py_6(),
        )
}

/// The centered content column: left-aligned children, consistent vertical rhythm,
/// capped at [`CONTENT_MAX_WIDTH`]. Fills narrower panes, centers in wider ones.
pub fn content_column() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_3()
        .w_full()
        .max_w(px(CONTENT_MAX_WIDTH))
}

/// A section header above a [`group`]: small semibold title + muted one-line caption.
pub fn section_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .pt_4()
        .child(
            div()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(description.into()),
        )
}

/// Wrap a set of rows into a single rounded grouped card with inset hairline dividers
/// between rows (iOS / Surge settings style), rather than one bordered card per row.
pub fn group(rows: Vec<AnyElement>) -> gpui::Div {
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded_lg()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border());
    for (index, row) in rows.into_iter().enumerate() {
        if index != 0 {
            card = card.child(
                div()
                    .w_full()
                    .pl_4()
                    .child(div().h(px(1.)).w_full().bg(theme::border())),
            );
        }
        card = card.child(row);
    }
    card
}

/// A flat row container for use inside a [`group`]: flex row, standard padding, full
/// width. Add a [`row_label`] and a trailing control as children; attach interactivity
/// (`.id(...)`, `.on_click(...)`, hover) at the call site.
pub fn row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .min_w_0()
        .gap_4()
        .px_4()
        .py_3()
}

/// The left-hand label + description column shared by every grouped row: a semibold
/// label over a muted, two-line-clamped description, taking the remaining width.
pub fn row_label(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
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
                .child(label.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .line_clamp(2)
                .child(description.into()),
        )
}

/// The switch pill used by grouped toggle rows: blue (accent) when on, neutral when
/// off, with the knob sliding to the trailing edge. Matches the selected sidebar item.
pub fn toggle(on: bool) -> gpui::Div {
    div()
        .w(px(44.))
        .h(px(24.))
        .flex_shrink_0()
        .rounded_full()
        .p(px(2.))
        .flex()
        .when(on, |s| s.justify_end())
        .bg(if on {
            theme::accent()
        } else {
            theme::surface_hover()
        })
        .child(
            div()
                .w(px(20.))
                .h(px(20.))
                .rounded_full()
                .bg(theme::surface()),
        )
}
