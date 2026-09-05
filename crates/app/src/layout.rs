//! OcHub uses compact headers; detailed explanations belong in the docs site.
use gpui::SharedString;
pub use ochub_ui::layout::*;

pub fn page_header(title: impl Into<SharedString>, _subtitle: Option<SharedString>) -> gpui::Div {
    ochub_ui::layout::page_header(title, None)
}

pub fn section_header(
    title: impl Into<SharedString>,
    _description: Option<SharedString>,
) -> gpui::Div {
    ochub_ui::layout::section_header(title, None)
}

pub fn switch_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    _description: Option<SharedString>,
    on: bool,
    disabled: bool,
    on_toggle: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::layout::switch_row(id, label, None, on, disabled, on_toggle)
}

#[allow(clippy::too_many_arguments)]
pub fn select_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    _description: Option<SharedString>,
    options: &[&str],
    selected: usize,
    state: SelectRowState,
    on_event: impl Fn(SelectRowEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::layout::select_row(id, label, None, options, selected, state, on_event)
}
