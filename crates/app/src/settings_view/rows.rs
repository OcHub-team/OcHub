//! The adapter between `cx.listener` and the four `layout` rows.
//!
//! Each helper takes a [`RowId`] and pulls the element id, the label key and
//! the description key out of [`search::entry`], so no page ever writes an id
//! literal or repeats the `move |window, cx| listener(&(), window, cx)`
//! boilerplate. `desc_override` is for the rows whose description is a live
//! readout (the resolved data directory, the update state) rather than static
//! copy; search still matches against the static key.

use gpui::{div, prelude::*, AnyElement, Context, SharedString, Window};

use crate::components::ButtonTone;
use crate::i18n::t;
use crate::layout;

use super::search::{entry, RowId};
use super::SettingsView;

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
        desc_override.unwrap_or_else(|| t(entry.desc)),
        on,
        disabled,
        move |window, cx| activate(&(), window, cx),
    )
    .into_any_element()
}

/// A segmented control: every option visible, any option one click away.
pub(super) fn select(
    cx: &mut Context<SettingsView>,
    row: RowId,
    options: &[&str],
    selected: usize,
    disabled: bool,
    desc_override: Option<SharedString>,
    handler: impl Fn(&mut SettingsView, usize, &mut Context<SettingsView>) + 'static,
) -> AnyElement {
    let entry = entry(row);
    let activate = cx.listener(
        move |this: &mut SettingsView, index: &usize, _window: &mut Window, cx| {
            handler(this, *index, cx)
        },
    );
    layout::select_row(
        entry.id,
        t(entry.label),
        desc_override.unwrap_or_else(|| t(entry.desc)),
        options,
        selected,
        disabled,
        move |index, window, cx| activate(&index, window, cx),
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
        desc_override.unwrap_or_else(|| t(entry.desc)),
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
        desc_override.unwrap_or_else(|| t(entry.desc)),
        action,
        tone,
        disabled,
        move |window, cx| activate(&(), window, cx),
    )
    .into_any_element()
}

/// One section as a virtualized list item: header above a grouped card, with
/// its own bottom spacing (the list draws no inter-item gap).
pub(super) fn group_block(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    rows: Vec<AnyElement>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pb_3()
        .w_full()
        .child(layout::section_header(title, description))
        .child(layout::group(rows))
        .into_any_element()
}
