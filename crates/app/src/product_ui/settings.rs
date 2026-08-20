use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div};

use crate::layout;

/// A settings section exactly as it appears in the native virtualized list.
pub fn group_block(title: impl Into<SharedString>, rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pb_3()
        .w_full()
        .child(layout::section_header(title, None))
        .child(layout::group(rows))
        .into_any_element()
}
