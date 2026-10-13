//! Callers pass live metadata and state descriptions explicitly. Static help
//! remains in the documentation and is not added by these adapters.
use gpui::SharedString;
pub use ochub_ui::layout::*;

pub fn page_header(title: impl Into<SharedString>, subtitle: Option<SharedString>) -> gpui::Div {
    ochub_ui::layout::page_header(title, subtitle)
}

pub fn section_header(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
) -> gpui::Div {
    ochub_ui::layout::section_header(title, description)
}

pub fn switch_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    on: bool,
    disabled: bool,
    on_toggle: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::layout::switch_row(id, label, description, on, disabled, on_toggle)
}

#[allow(clippy::too_many_arguments)]
pub fn select_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    options: &[&str],
    selected: usize,
    state: SelectRowState,
    on_event: impl Fn(SelectRowEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    ochub_ui::layout::select_row(id, label, description, options, selected, state, on_event)
}
