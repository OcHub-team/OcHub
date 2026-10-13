use gpui::{AnyElement, ParentElement, SharedString, Styled, div, prelude::*, px};

use crate::layout;
use crate::theme;

pub const EDITOR_MAX_WIDTH: f32 = 1320.;
pub const EDITOR_SPLIT_MIN_WINDOW_WIDTH: f32 = 1200.;
pub const PREVIEW_SPLIT_FRACTION: f32 = 0.38;
pub const PREVIEW_SPLIT_MIN_WIDTH: f32 = 400.;
pub const PREVIEW_SPLIT_MAX_WIDTH: f32 = 560.;

pub fn is_compact(window_width: f32) -> bool {
    window_width < EDITOR_SPLIT_MIN_WINDOW_WIDTH
}

/// Use the space the form actually receives, including the preview's minimum
/// width. A larger window can still contain a narrow form when split opens.
pub fn form_width(window_width: f32, preview_visible: bool) -> f32 {
    let content = (window_width - super::shell::SIDEBAR_WIDTH - 48.).clamp(0., EDITOR_MAX_WIDTH);
    if preview_visible && !is_compact(window_width) {
        let preview = (content * PREVIEW_SPLIT_FRACTION)
            .clamp(PREVIEW_SPLIT_MIN_WIDTH, PREVIEW_SPLIT_MAX_WIDTH);
        (content - preview - 16. - 8.).max(0.)
    } else {
        (content - 8.).max(0.)
    }
}

pub fn stacks_field_grid(form_width: f32, text_columns: usize, toggle_columns: usize) -> bool {
    // Allow a useful model identifier in every text field, plus shared padding,
    // gaps and a fixed 32px icon action slot.
    let columns = text_columns + toggle_columns + 1;
    let required = text_columns as f32 * 140.
        + toggle_columns as f32 * 64.
        + 32.
        + 26.
        + columns.saturating_sub(1) as f32 * 8.;
    form_width < required
}

pub struct ProviderEditorPage {
    pub title: SharedString,
    pub subtitle: SharedString,
    pub actions: AnyElement,
    pub tabs: AnyElement,
    pub footer: AnyElement,
    pub form_scroll: AnyElement,
    pub preview: Option<AnyElement>,
    pub form_scrollbar: Option<AnyElement>,
    pub modal: Option<AnyElement>,
    pub convert_modal: Option<AnyElement>,
    pub compact_layout: bool,
}

/// The canonical provider-editor page composition. Data loading, validation,
/// save operations, and the concrete form controls remain host-owned.
pub fn provider_editor_page(spec: ProviderEditorPage) -> gpui::Div {
    let body = div()
        .flex()
        .items_stretch()
        .flex_1()
        .min_h_0()
        .gap_4()
        .w_full()
        .when(spec.compact_layout, |body| body.flex_col())
        .when(!spec.compact_layout, |body| body.flex_row())
        .child(spec.form_scroll)
        .children(spec.preview);

    let editor_body = div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .items_center()
        .p_6()
        .child(
            layout::wide_column()
                .max_w(px(EDITOR_MAX_WIDTH))
                .h_full()
                .min_h_0()
                .child(body),
        )
        .children(spec.form_scrollbar);

    layout::page()
        .relative()
        .child(
            layout::page_header(spec.title, Some(spec.subtitle))
                .when(spec.compact_layout, |header| {
                    header.flex_col().items_stretch().gap_3()
                })
                .child(spec.actions),
        )
        .child(
            div()
                .flex_none()
                .px_6()
                .pb_3()
                .border_b_1()
                .border_color(theme::border())
                .child(spec.tabs),
        )
        .child(editor_body)
        .child(
            div()
                .flex_none()
                .px_6()
                .py_3()
                .border_t_1()
                .border_color(theme::border())
                .bg(theme::surface())
                .child(spec.footer),
        )
        .children(spec.modal)
        .children(spec.convert_modal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_breakpoints_match_the_desktop_layout() {
        assert!(is_compact(1199.));
        assert!(!is_compact(1200.));
        assert!(form_width(1200., true) < form_width(1199., true));
        assert!(stacks_field_grid(form_width(1200., true), 6, 0));
        assert!(stacks_field_grid(form_width(960., true), 6, 0));
        assert!(!stacks_field_grid(form_width(1500., false), 6, 0));
        assert!(!stacks_field_grid(form_width(1200., true), 2, 0));
    }
}
