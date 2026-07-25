//! Shared page-layout primitives — the single Surge-style base every view sits on.
//!
//! A view is a full-height column: a [`page_header`] bar (bold title, optional muted
//! subtitle, optional right-aligned actions) above a [`scroll_body`] — a vertically
//! scrolling region that horizontally centers a fixed-max-width [`content_column`].
//! Config-style views additionally use [`section_header`] + [`group`] to render
//! iOS/Surge grouped cards with inset hairline dividers, and [`row`] / [`row_label`]
//! / [`toggle`] for the rows inside those cards.
//!
//! Keeping all of this in one module means pages line up (same column width, same
//! header chrome, same card rhythm) instead of each view hand-rolling its own.
//!
//! # Which row do I use?
//!
//! [`row`] is the bare container: it draws a row and nothing else, so a page that
//! reaches for it has to invent its own control, its own keyboard handling and
//! its own answer to "what does clicking here do?". Prefer one of the four
//! purpose-built rows, each of which is focusable, activates on Space/Enter, and
//! carries a trailing control you can identify without reading the label:
//!
//! | Row               | Trailing control    | Activating it…                     |
//! |-------------------|---------------------|------------------------------------|
//! | [`switch_row`]    | toggle pill         | flips a boolean, **immediately**   |
//! | [`select_row`]    | segmented control   | picks one of a visible option set  |
//! | [`navigate_row`]  | chevron             | opens a sub-page; changes nothing  |
//! | [`action_row`]    | button              | runs a command (save, sync, reset) |
//!
//! The distinctions are the point. A switch that needed a separate Save button
//! would be a lie; an action dressed as a settings row makes a destructive
//! command look like a preference. If a row would *write* something, it gets a
//! button — [`action_row`] with [`crate::components::ButtonTone::Danger`] when
//! the write is destructive. If a page batches its edits instead of applying
//! them per row, end it with [`crate::components::commit_bar`].
//!
//! Rows that only *display* a value are still plain [`row`] + [`row_label`]:
//! they are not interactive, so making them focusable would put a tab stop on
//! something that cannot be operated.

use std::rc::Rc;

use gpui::{
    actions, div, prelude::*, px, AnyElement, App, ElementId, FontWeight, KeyBinding, KeyDownEvent,
    SharedString, Window,
};

use crate::components::{self, ButtonSize, ButtonTone, DISABLED_OPACITY};
use crate::i18n::k;
use crate::icons::{icon, IconName};
use crate::scrollbar::{contain_vertical_scroll, VerticalScrollbar};
use crate::tf;
use crate::theme;

type RowActivation = Rc<dyn Fn(&mut Window, &mut App)>;
type RowSelectEventHandler = Rc<dyn Fn(SelectRowEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectRowState {
    pub disabled: bool,
    pub dropdown_open: bool,
}

impl SelectRowState {
    pub fn new(disabled: bool, dropdown_open: bool) -> Self {
        Self {
            disabled,
            dropdown_open,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectRowEvent {
    Open(bool),
    Select(usize),
}

/// Max width of the centered content column, shared by every view so pages align.
pub const CONTENT_MAX_WIDTH: f32 = 800.;

/// Max width for data-dense pages (provider list, usage, tools, gateway):
/// same centered layout, wider column.
pub const WIDE_MAX_WIDTH: f32 = 1080.;

/// Outermost page container: a full-height flex column on the window background.
pub fn page() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(theme::content_background())
}

/// A page header bar: bold title, optional muted subtitle, and a trailing slot for
/// action buttons. Chain `.child(...)` onto the returned element to add actions —
/// `justify_between` pushes them to the trailing edge.
pub fn page_header(title: impl Into<SharedString>, subtitle: Option<SharedString>) -> gpui::Div {
    let mut title_col = div().flex().flex_col().flex_1().min_w_0().gap_1().child(
        div()
            .min_w_0()
            .truncate()
            .text_color(theme::text())
            .text_xl()
            .font_weight(FontWeight::BOLD)
            .child(title.into()),
    );
    if let Some(subtitle) = subtitle {
        title_col = title_col.child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme::muted())
                .text_xs()
                .child(subtitle),
        );
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .px_6()
        .py_4()
        .border_b_1()
        .border_color(theme::border())
        .child(title_col)
}

/// The scrollable body: a vertically scrolling region that horizontally centers its
/// content. Pass the column built via [`content_column`] (or any centered child).
pub fn scroll_body(
    id: &'static str,
    handle: &gpui::ScrollHandle,
    column: impl IntoElement,
) -> gpui::Div {
    let contained_handle = handle.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .child(
            div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .items_center()
                .p_6()
                .min_w_0()
                .overflow_y_scroll()
                .track_scroll(handle)
                .on_scroll_wheel(contain_vertical_scroll(contained_handle))
                .child(column),
        )
        .child(VerticalScrollbar::new(
            gpui::ElementId::Name(format!("{id}-scrollbar").into()),
            handle.clone(),
        ))
}

/// A **virtualized** scrolling body, centered at [`CONTENT_MAX_WIDTH`] like
/// [`scroll_body`] but backed by `gpui::list` so only the visible items (plus a
/// little overdraw) are laid out and painted — the fix for long pages that drop
/// frames when every off-screen row (especially text inputs) was being relaid each
/// frame. The caller owns a `gpui::ListState` and supplies the `list` element built
/// with `gpui::list(state, cx.processor(|this, ix, window, cx| ...))`; each item
/// should carry its own bottom spacing (the list draws no inter-item gap).
pub fn virtual_body(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
) -> impl IntoElement {
    virtual_body_with_width(id, list, state, CONTENT_MAX_WIDTH)
}

/// [`virtual_body`] at [`WIDE_MAX_WIDTH`], for the data-dense pages.
pub fn wide_virtual_body(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
) -> impl IntoElement {
    virtual_body_with_width(id, list, state, WIDE_MAX_WIDTH)
}

fn virtual_body_with_width(
    id: &'static str,
    list: gpui::List,
    state: &gpui::ListState,
    max_width: f32,
) -> impl IntoElement {
    let contained_state = state.clone();
    div()
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_h_0()
        .w_full()
        .min_w_0()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .flex_1()
                .min_h_0()
                .w_full()
                .min_w_0()
                .px_6()
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .max_w(px(max_width))
                        .on_scroll_wheel(contain_vertical_scroll(contained_state))
                        .child(
                            list.with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .py_6(),
                        ),
                ),
        )
        // Keep page chrome independent from the centered content column. The
        // scrollbar belongs to the full-width viewport, so it must stay at the
        // page edge even when the list itself is capped at `max_width`.
        .child(VerticalScrollbar::new(
            gpui::ElementId::Name(format!("{id}-scrollbar").into()),
            state.clone(),
        ))
}

/// The centered content column: left-aligned children, consistent vertical rhythm,
/// capped at [`CONTENT_MAX_WIDTH`]. Fills narrower panes, centers in wider ones.
pub fn content_column() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_3()
        .w_full()
        .max_w(px(CONTENT_MAX_WIDTH))
}

/// The wide centered column for data-dense pages, capped at [`WIDE_MAX_WIDTH`].
pub fn wide_column() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_3()
        .w_full()
        .max_w(px(WIDE_MAX_WIDTH))
}

/// A section header above a [`group`]: small semibold title + muted one-line caption.
pub fn section_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .pt_4()
        .child(
            div()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(description.into()),
        )
}

/// Wrap a set of rows into a single rounded grouped card with inset hairline dividers
/// between rows (iOS / Surge settings style), rather than one bordered card per row.
pub fn group(rows: Vec<AnyElement>) -> gpui::Div {
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded_lg()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border());
    for (index, row) in rows.into_iter().enumerate() {
        if index != 0 {
            card = card.child(
                div()
                    .w_full()
                    .pl_4()
                    .child(div().h(px(1.)).w_full().bg(theme::border())),
            );
        }
        card = card.child(row);
    }
    card
}

/// A flat row container for use inside a [`group`]: flex row, standard padding, full
/// width. Add a [`row_label`] and a trailing control as children; attach interactivity
/// (`.id(...)`, `.on_click(...)`, hover) at the call site.
pub fn row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .min_w_0()
        .gap_4()
        .px_4()
        .py_3()
}

/// The left-hand label + description column shared by every grouped row: a semibold
/// label over a muted, two-line-clamped description, taking the remaining width.
pub fn row_label(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_color(theme::text())
                .font_weight(FontWeight::SEMIBOLD)
                .child(label.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .line_clamp(2)
                .child(description.into()),
        )
}

/// The switch pill used by grouped toggle rows: blue (accent) when on, neutral when
/// off, with the knob sliding to the trailing edge. Matches the selected sidebar item.
pub fn toggle(on: bool) -> gpui::Div {
    div()
        .w(px(44.))
        .h(px(24.))
        .flex_shrink_0()
        .rounded_full()
        .p(px(2.))
        .flex()
        .when(on, |s| s.justify_end())
        .bg(if on {
            theme::accent()
        } else {
            theme::surface_hover()
        })
        .child(
            div()
                .w(px(20.))
                .h(px(20.))
                .rounded_full()
                .bg(theme::surface()),
        )
}

// ── Keyboard-operable rows ──────────────────────────────────────────────────
//
// `settings_view` is the first consumer of all four rows and of `bind_keys`.
// A page should be able to reach for the right row without first having to
// build it, so these landed ahead of that — the `#[allow(dead_code)]` that
// bought them the time is gone now that they are used.

actions!(ochub_row, [Activate]);

/// The keymap context every interactive row publishes. Scoping the Space/Enter
/// bindings to it keeps them off text inputs, which need those keys themselves.
const ROW_KEY_CONTEXT: &str = "SettingsRow";

/// Register Space/Enter as "activate the focused row".
///
/// Call once from `main.rs`, **after** `text_input::bind_keys`: an unscoped
/// `enter` binding is already registered there, and ties in the keymap are
/// broken by registration order.
///
/// Wiring this up is optional. The rows also handle a bare Space/Enter key press
/// directly, and that fallback only runs when no binding consumed the keystroke,
/// so keyboard activation works either way and never fires twice.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", Activate, Some(ROW_KEY_CONTEXT)),
        KeyBinding::new("enter", Activate, Some(ROW_KEY_CONTEXT)),
    ]);
}

/// A bare Space/Enter press — no modifiers, not a key repeat.
///
/// Holding a key down would otherwise toggle a switch dozens of times, and
/// `cmd-enter` belongs to whichever page-level command claimed it.
fn is_activation_key(event: &KeyDownEvent) -> bool {
    !event.is_held
        && !event.keystroke.modifiers.modified()
        && matches!(event.keystroke.key.as_str(), "space" | "enter")
}

/// The shared frame behind every interactive row: same geometry as [`row`],
/// plus focus, hover and disabled handling.
///
/// The focus ring is a 1px border and the padding is 1px short of [`row`]'s
/// `px_4`/`py_3` to pay for it, so a focused row occupies exactly the same box
/// as an unfocused one and the card's rhythm never shifts under the keyboard.
///
/// A disabled row is inert by construction rather than by suppression: it is
/// muted to [`DISABLED_OPACITY`], it never calls `focusable`, so it is not a tab
/// stop and cannot receive a key press, and each row builder returns before
/// attaching any click, action or key handler. Its trailing control is likewise
/// built in a handler-free variant, so there is nothing left to fire.
///
/// `id` is the caller's row id, which the four builders treat as a namespace:
/// the row itself is `{id}-row`, its trailing control `{id}-options` or
/// `{id}-button`. gpui keys the row's focus handle off that id, so it must be
/// unique within the window and stable across frames — a focused row whose id
/// changes loses focus mid-interaction.
fn row_frame(id: &SharedString, disabled: bool) -> gpui::Stateful<gpui::Div> {
    let frame = div()
        .id(ElementId::Name(format!("{id}-row").into()))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .min_w_0()
        .gap_4()
        .px(px(15.))
        .py(px(11.))
        .rounded_md()
        .border_1()
        .border_color(theme::surface().alpha(0.));
    if disabled {
        frame.opacity(DISABLED_OPACITY)
    } else {
        frame
            .focusable()
            .tab_stop(true)
            .key_context(ROW_KEY_CONTEXT)
            .hover(|style| style.bg(theme::inset()))
            .focus(|style| style.bg(theme::inset()).border_color(theme::accent()))
    }
}

/// Wire a row's keyboard activation.
///
/// Two paths on purpose. The [`Activate`] action is the idiomatic gpui route —
/// it is what a keymap, a command palette or an assistive technology can see —
/// and it fires when [`bind_keys`] has been called. The raw key handler is the
/// fallback for a build that never registered those bindings. gpui stops
/// propagation as soon as an action listener runs, and key-down listeners are
/// dispatched only after every matching binding has been offered the keystroke,
/// so exactly one of the two ever fires.
fn with_activation(
    row: gpui::Stateful<gpui::Div>,
    activate: RowActivation,
) -> gpui::Stateful<gpui::Div> {
    let from_action = activate.clone();
    row.on_action(move |_: &Activate, window, cx| from_action(window, cx))
        .on_key_down(move |event, window, cx| {
            if is_activation_key(event) {
                cx.stop_propagation();
                activate(window, cx);
            }
        })
}

/// A row whose control is a **switch**: activating it flips `on` and the change
/// takes effect immediately. Reach for it whenever a setting is a boolean and
/// there is nothing to confirm.
///
/// Anywhere in the row activates — click, Space or Enter — and the whole row is
/// announced as a `Switch` carrying its on/off state, so the pill is a readout
/// rather than the only target. If the change needs confirming, or costs
/// something to undo, it is an [`action_row`], not a switch.
///
/// ```ignore
/// let toggle = cx.listener(|this, _: &(), _window, cx| this.toggle_tray(cx));
/// layout::switch_row(
///     "tray",
///     t(k::SETTINGS_TRAY_LABEL),
///     t(k::SETTINGS_TRAY_DESCRIPTION),
///     self.tray_enabled,
///     false,
///     move |window, cx| toggle(&(), window, cx),
/// )
/// ```
pub fn switch_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    on: bool,
    disabled: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let row = row_frame(&id, disabled)
        .role(gpui::Role::Switch)
        .aria_label(label.clone())
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .child(row_label(label, description))
        .child(toggle(on));
    if disabled {
        return row;
    }
    let activate: RowActivation = Rc::new(on_toggle);
    let clicked = activate.clone();
    with_activation(
        row.cursor_pointer()
            .on_click(move |_event, window, cx| clicked(window, cx)),
        activate,
    )
}

/// A row whose control adapts to the option set: compact choices stay in a
/// segmented control, while long or numerous choices use a dropdown.
///
/// This replaces click-to-cycle rows. Cycling hides the option set — you cannot
/// tell how many choices exist, what the next one will be, or how to go back —
/// and it costs `n - 1` clicks to reach the option before the current one.
///
/// Mouse users click the option they want. Keyboard users tab to the row and
/// press Space or Enter: segmented controls advance to the next value, while a
/// dropdown opens its option list. Use a [`navigate_row`] instead when each
/// option needs a full explanation or its own configuration.
pub fn select_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    state: SelectRowState,
    on_event: impl Fn(SelectRowEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let current = options.get(selected).copied().unwrap_or_default();
    let event: RowSelectEventHandler = Rc::new(on_event);
    let use_dropdown = components::select_prefers_dropdown(options);

    let options_id = SharedString::from(format!("{id}-options"));
    let control = if use_dropdown {
        if state.disabled {
            components::select_dropdown_readonly(options_id, options, selected).into_any_element()
        } else {
            let event = event.clone();
            components::select_dropdown(
                options_id,
                options,
                selected,
                state.dropdown_open,
                move |dropdown_event, window, cx| match dropdown_event {
                    components::SelectDropdownEvent::Open(open) => {
                        event(SelectRowEvent::Open(open), window, cx)
                    }
                    components::SelectDropdownEvent::Select(index) => {
                        event(SelectRowEvent::Select(index), window, cx)
                    }
                },
            )
            .into_any_element()
        }
    } else if state.disabled {
        components::segmented_readonly(options_id, options, selected).into_any_element()
    } else {
        let event = event.clone();
        components::segmented(options_id, options, selected, move |index, window, cx| {
            event(SelectRowEvent::Select(index), window, cx)
        })
        .into_any_element()
    };

    let mut row = row_frame(&id, state.disabled)
        .aria_label(SharedString::from(tf!(
            k::COMMON_ROW_SELECT_ARIA,
            label = label,
            value = current
        )))
        .child(row_label(label, description))
        .child(
            div()
                .flex_none()
                .when(use_dropdown, |slot| slot.w(px(240.)).max_w_full())
                .child(control),
        );
    if !use_dropdown {
        row = row.role(gpui::Role::RadioGroup);
    }
    if state.disabled {
        return row;
    }

    let activate = if use_dropdown {
        let event = event.clone();
        Rc::new(move |window: &mut Window, cx: &mut App| {
            event(SelectRowEvent::Open(!state.dropdown_open), window, cx)
        }) as RowActivation
    } else {
        // Keyboard activation advances one step so a single key can walk a
        // compact segmented set whose options all remain visible.
        let next = if options.is_empty() {
            0
        } else {
            (selected + 1) % options.len()
        };
        Rc::new(move |window: &mut Window, cx: &mut App| {
            event(SelectRowEvent::Select(next), window, cx)
        })
    };
    with_activation(row, activate)
}

/// A row that **drills in**: activating it opens a sub-page. It changes nothing
/// by itself, and the trailing chevron says so.
///
/// The chevron is what separates this from every other row: it points onward
/// rather than presenting a control, so nobody has to guess whether clicking
/// will read or write. `value` optionally previews what is configured inside
/// ("3 rules", "Custom"), muted and ahead of the chevron; leave it `None` when
/// the sub-page has no one-line summary. Use it whenever a setting needs more
/// room than a row — a list to edit, options that need explaining, anything
/// with its own validation.
pub fn navigate_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    value: Option<SharedString>,
    disabled: bool,
    on_open: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let row = row_frame(&id, disabled)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(tf!(
            k::COMMON_ROW_NAVIGATE_ARIA,
            label = label
        )))
        .child(row_label(label, description))
        .when_some(value, |row, value| {
            row.child(
                div()
                    .flex_none()
                    .max_w(px(200.))
                    .truncate()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(value),
            )
        })
        .child(div().flex_none().flex().items_center().child(icon(
            IconName::ChevronRight,
            theme::muted(),
            14.,
        )));
    if disabled {
        return row;
    }
    let activate: RowActivation = Rc::new(on_open);
    let clicked = activate.clone();
    with_activation(
        row.cursor_pointer()
            .on_click(move |_event, window, cx| clicked(window, cx)),
        activate,
    )
}

/// A row that **runs a command**: activating it writes, syncs, resets or
/// deletes. The trailing control is a real [`crate::components::button`],
/// because a button is the one shape users already read as "this does
/// something".
///
/// Never disguise a command as a settings row. A save that looks like a
/// preference is the worst case: the row reads as state, so a user expects a
/// click to change what it displays, and instead it commits. Pass
/// [`ButtonTone::Danger`] for anything destructive and
/// [`ButtonTone::Primary`] for the one obvious action on the page; everything
/// else is [`ButtonTone::Neutral`].
///
/// Only the button responds to the mouse — clicking the label does nothing, so
/// there is no invisible hit area that fires a command. From the keyboard the
/// row itself is the tab stop, and Space/Enter runs it once. Set `disabled`
/// while the command is already running: the row leaves the tab order, the
/// button goes inert, and a second submit becomes impossible.
pub fn action_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    action: impl Into<SharedString>,
    tone: ButtonTone,
    disabled: bool,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let label = label.into();
    let action = action.into();
    let activate: RowActivation = Rc::new(on_activate);

    let button_id = ElementId::Name(format!("{id}-button").into());
    let control = if disabled {
        // `row_frame` already fades the whole row, and gpui multiplies nested
        // opacity — fading the button again would leave it barely legible.
        components::disabled_button(button_id, action.clone(), tone, ButtonSize::Sm, false)
            .into_any_element()
    } else {
        let clicked = activate.clone();
        components::button(button_id, action.clone(), tone, ButtonSize::Sm)
            .on_click(move |_event, window, cx| clicked(window, cx))
            .into_any_element()
    };

    let row = row_frame(&id, disabled)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(tf!(
            k::COMMON_ROW_ACTION_ARIA,
            label = label,
            action = action
        )))
        .child(row_label(label, description))
        .child(div().flex_none().child(control));
    if disabled {
        return row;
    }
    with_activation(row, activate)
}
