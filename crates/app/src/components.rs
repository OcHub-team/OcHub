//! Product-wide forms keep labels and validation; reference copy lives in docs.
use gpui::{IntoElement, SharedString};
pub use ochub_ui::components::*;

pub fn field(
    label: impl Into<SharedString>,
    required: bool,
    _help: Option<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    ochub_ui::components::field(label, required, None, control)
}

pub fn field_with_error(
    label: impl Into<SharedString>,
    required: bool,
    _help: Option<SharedString>,
    error: Option<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    ochub_ui::components::field_with_error(label, required, None, error, control)
}
