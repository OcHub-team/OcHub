use gpui::{IntoElement, ParentElement, SharedString, Styled};

use crate::layout;

/// Product-page frame owned by the desktop application.
pub fn page(
    title: impl Into<SharedString>,
    actions: impl IntoElement,
    body: impl IntoElement,
) -> gpui::Div {
    layout::page()
        .relative()
        .child(layout::page_header(title, None).child(actions))
        .child(body)
}
