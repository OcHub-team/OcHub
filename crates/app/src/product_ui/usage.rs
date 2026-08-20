use gpui::{AnyElement, ParentElement, Styled, div};

/// The production usage-summary grid is two columns.
pub fn summary_grid(cards: impl IntoIterator<Item = AnyElement>) -> gpui::Div {
    div().grid().grid_cols(2).gap_3().children(cards)
}
