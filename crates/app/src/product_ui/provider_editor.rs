use gpui::{AnyElement, FontWeight, ParentElement, SharedString, Styled, div, prelude::*, px};

use crate::components::{self, BadgeTone};
use crate::layout;
use crate::theme;

pub const EDITOR_MAX_WIDTH: f32 = 1320.;
pub const EDITOR_SPLIT_MIN_WINDOW_WIDTH: f32 = 1200.;
pub const EDITOR_STACK_GRID_MAX_WINDOW_WIDTH: f32 = 1050.;
pub const PREVIEW_SPLIT_FRACTION: f32 = 0.38;
pub const PREVIEW_SPLIT_MIN_WIDTH: f32 = 400.;
pub const PREVIEW_SPLIT_MAX_WIDTH: f32 = 560.;

pub fn is_compact(window_width: f32) -> bool {
    window_width < EDITOR_SPLIT_MIN_WINDOW_WIDTH
}

pub fn stacks_field_grid(window_width: f32) -> bool {
    window_width < EDITOR_STACK_GRID_MAX_WINDOW_WIDTH
}

pub struct ProviderEditorPage {
    pub title: SharedString,
    pub subtitle: SharedString,
    pub actions: AnyElement,
    pub form_scroll: AnyElement,
    pub preview: Option<AnyElement>,
    pub form_scrollbar: Option<AnyElement>,
    pub modal: Option<AnyElement>,
    pub convert_modal: Option<AnyElement>,
    pub compact_layout: bool,
    pub stack_grid: bool,
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
        .when(spec.stack_grid, |editor| editor.p_4())
        .when(!spec.stack_grid, |editor| editor.p_6())
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
        .child(layout::page_header(spec.title, Some(spec.subtitle)).child(spec.actions))
        .child(editor_body)
        .children(spec.modal)
        .children(spec.convert_modal)
}

pub struct PreviewSummary {
    pub title: SharedString,
    pub files: SharedString,
    pub errors: Option<SharedString>,
    pub warnings: Option<SharedString>,
}

/// Collapsed file-preview row used by the real editor whenever its preview is
/// folded in the single-column layout. Hosts attach the action and ARIA label.
pub fn preview_summary(spec: PreviewSummary) -> gpui::Stateful<gpui::Div> {
    div()
        .id("preview-summary-expand")
        .role(gpui::Role::Button)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .cursor_pointer()
        .hover(|style| style.bg(theme::surface_hover()))
        .child(
            div()
                .flex_none()
                .text_color(theme::muted())
                .text_xs()
                .child("▸"),
        )
        .child(
            div()
                .flex_none()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(spec.title),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::muted())
                .text_xs()
                .font_family("Menlo")
                .child(spec.files),
        )
        .when_some(spec.errors, |row, errors| {
            row.child(components::badge(BadgeTone::Danger, errors))
        })
        .when_some(spec.warnings, |row, warnings| {
            row.child(components::badge(BadgeTone::Warning, warnings))
        })
}

pub fn preview_summary_card(summary: impl gpui::IntoElement) -> gpui::Div {
    components::card()
        .p_0()
        .flex_none()
        .overflow_hidden()
        .child(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_breakpoints_match_the_desktop_layout() {
        assert!(is_compact(1199.));
        assert!(!is_compact(1200.));
        assert!(stacks_field_grid(1049.));
        assert!(!stacks_field_grid(1050.));
    }
}
