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

use gpui::{div, prelude::*, px, AnyElement, FontWeight, SharedString};

use crate::scrollbar::{contain_vertical_scroll, VerticalScrollbar};
use crate::theme;

/// Max width of the centered content column, shared by every view so pages align.
pub const CONTENT_MAX_WIDTH: f32 = 800.;

/// Max width for data-dense pages (provider list, usage, tools, gateway):
/// same centered layout, wider column.
pub const WIDE_MAX_WIDTH: f32 = 1080.;

/// Outermost page container: a full-height flex column on the window background.
pub fn page() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(theme::content_background())
}

/// A page header bar: bold title, optional muted subtitle, and a trailing slot for
/// action buttons. Chain `.child(...)` onto the returned element to add actions —
/// `justify_between` pushes them to the trailing edge.
pub fn page_header(title: impl Into<SharedString>, subtitle: Option<SharedString>) -> gpui::Div {
    let mut title_col = div().flex().flex_col().flex_1().min_w_0().gap_1().child(
        div()
            .min_w_0()
            .truncate()
            .text_color(theme::text())
            .text_xl()
            .font_weight(FontWeight::BOLD)
            .child(title.into()),
    );
    if let Some(subtitle) = subtitle {
        title_col = title_col.child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme::muted())
                .text_xs()
                .child(subtitle),
        );
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
    id: &'static str,
    handle: &gpui::ScrollHandle,
    column: impl IntoElement,
) -> gpui::Div {
    let contained_handle = handle.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .child(
            div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .items_center()
                .p_6()
                .min_w_0()
                .overflow_y_scroll()
                .track_scroll(handle)
                .on_scroll_wheel(contain_vertical_scroll(contained_handle))
                .child(column),
        )
        .child(VerticalScrollbar::new(
            gpui::ElementId::Name(format!("{id}-scrollbar").into()),
            handle.clone(),
        ))
}

/// [`scroll_body`] with an externally held [`gpui::ScrollHandle`], so the view
/// can scroll programmatically (e.g. jump back to a top-anchored editor).
pub fn scroll_body_tracked(
    id: &'static str,
    handle: &gpui::ScrollHandle,
    column: impl IntoElement,
) -> gpui::Div {
    scroll_body(id, handle, column)
}

/// A **virtualized** scrolling body, centered at [`CONTENT_MAX_WIDTH`] like
/// [`scroll_body`] but backed by `gpui::list` so only the visible items (plus a
/// little overdraw) are laid out and painted — the fix for long pages that drop
/// frames when every off-screen row (especially text inputs) was being relaid each
/// frame. The caller owns a `gpui::ListState` and supplies the `list` element built
/// with `gpui::list(state, cx.processor(|this, ix, window, cx| ...))`; each item
/// should carry its own bottom spacing (the list draws no inter-item gap).
pub fn virtual_body(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
) -> impl IntoElement {
    virtual_body_with_width(id, list, state, CONTENT_MAX_WIDTH)
}

/// [`virtual_body`] at [`WIDE_MAX_WIDTH`], for the data-dense pages.
pub fn wide_virtual_body(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
) -> impl IntoElement {
    virtual_body_with_width(id, list, state, WIDE_MAX_WIDTH)
}

fn virtual_body_with_width(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
    max_width: f32,
) -> impl IntoElement {
    let contained_state = state.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_h_0()
        .w_full()
        .min_w_0()
        .child(
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
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .max_w(px(max_width))
                        .on_scroll_wheel(contain_vertical_scroll(contained_state))
                        .child(
                            list.with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .py_6(),
                        ),
                ),
        )
        // Keep page chrome independent from the centered content column. The
        // scrollbar belongs to the full-width viewport, so it must stay at the
        // page edge even when the list itself is capped at `max_width`.
        .child(VerticalScrollbar::new(
            gpui::ElementId::Name(format!("{id}-scrollbar").into()),
            state.clone(),
        ))
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

/// The wide centered column for data-dense pages, capped at [`WIDE_MAX_WIDTH`].
pub fn wide_column() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_3()
        .w_full()
        .max_w(px(WIDE_MAX_WIDTH))
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
