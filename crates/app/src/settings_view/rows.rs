//! The adapter between `cx.listener` and the four `layout` rows.
//!
//! Each helper takes a [`RowId`] and pulls the element id and the label key out
//! of [`search::entry`], so no page ever writes an id literal or repeats the
//! `move |window, cx| listener(&(), window, cx)` boilerplate.
//!
//! A row renders a second line only when `desc_override` carries a live readout
//! (the resolved data directory, the sync summary). The static `desc` key is
//! never drawn — it stays in [`search::RowEntry`] purely so search keeps
//! matching against the wording a user would think to type.

use gpui::{AnyElement, Context, IntoElement, SharedString, Window};

use crate::components::ButtonTone;
use crate::i18n::t;
use crate::layout;
use crate::product_ui::settings as settings_screen;

use super::SettingsView;
use super::search::{RowId, entry};

/// A switch that flips a boolean and writes immediately.
pub(super) fn switch(
    cx: &mut Context<SettingsView>,
    row: RowId,
    on: bool,
    disabled: bool,
    desc_override: Option<SharedString>,
    handler: impl Fn(&mut SettingsView, &mut Context<SettingsView>) + 'static,
) -> AnyElement {
    let entry = entry(row);
    let activate = cx.listener(
        move |this: &mut SettingsView, _event: &(), _window: &mut Window, cx| handler(this, cx),
    );
    layout::switch_row(
        entry.id,
        t(entry.label),
        desc_override,
        on,
        disabled,
        move |window, cx| activate(&(), window, cx),
    )
    .into_any_element()
}

/// An adaptive select: short lists stay segmented, long lists open a dropdown.
pub(super) fn select(
    cx: &mut Context<SettingsView>,
    row: RowId,
    options: &[&str],
    selected: usize,
    state: layout::SelectRowState,
    desc_override: Option<SharedString>,
    handler: impl Fn(&mut SettingsView, usize, &mut Context<SettingsView>) + 'static,
) -> AnyElement {
    let entry = entry(row);
    let on_event = cx.listener(
        move |this: &mut SettingsView, event: &layout::SelectRowEvent, _window: &mut Window, cx| {
            match *event {
                layout::SelectRowEvent::Open(open) => {
                    this.open_select_row = if open { Some(row) } else { None };
                    cx.notify();
                }
                layout::SelectRowEvent::Select(index) => {
                    this.open_select_row = None;
                    handler(this, index, cx);
                }
            }
        },
    );
    layout::select_row(
        entry.id,
        t(entry.label),
        desc_override,
        options,
        selected,
        state,
        move |event, window, cx| on_event(&event, window, cx),
    )
    .into_any_element()
}

/// A drill-in row: it changes nothing by itself, and the chevron says so.
pub(super) fn nav(
    cx: &mut Context<SettingsView>,
    row: RowId,
    value: Option<SharedString>,
    disabled: bool,
    desc_override: Option<SharedString>,
    handler: impl Fn(&mut SettingsView, &mut Context<SettingsView>) + 'static,
) -> AnyElement {
    let entry = entry(row);
    let activate = cx.listener(
        move |this: &mut SettingsView, _event: &(), _window: &mut Window, cx| handler(this, cx),
    );
    layout::navigate_row(
        entry.id,
        t(entry.label),
        desc_override,
        value,
        disabled,
        move |window, cx| activate(&(), window, cx),
    )
    .into_any_element()
}

/// A row that runs a command. The trailing control is a real button, because a
/// button is the one shape users already read as "this does something".
pub(super) fn act(
    cx: &mut Context<SettingsView>,
    row: RowId,
    action: SharedString,
    tone: ButtonTone,
    disabled: bool,
    desc_override: Option<SharedString>,
    handler: impl Fn(&mut SettingsView, &mut Context<SettingsView>) + 'static,
) -> AnyElement {
    let entry = entry(row);
    let activate = cx.listener(
        move |this: &mut SettingsView, _event: &(), _window: &mut Window, cx| handler(this, cx),
    );
    layout::action_row(
        entry.id,
        t(entry.label),
        desc_override,
        action,
        tone,
        disabled,
        move |window, cx| activate(&(), window, cx),
    )
    .into_any_element()
}

/// One section as a virtualized list item: header above a grouped card, with
/// its own bottom spacing (the list draws no inter-item gap).
pub(super) fn group_block(title: impl Into<SharedString>, rows: Vec<AnyElement>) -> AnyElement {
    settings_screen::group_block(title, rows)
}
