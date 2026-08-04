//! Shared GPUI building blocks for the desktop shell.
//!
//! One component per repeated pattern (see docs/ui-overhaul.md §6): buttons,
//! fields, segmented control, badges/dots, cards, empty states, modal chrome,
//! disclosure, stat tiles, tables, pagination, status footer. Views compose
//! these instead of hand-rolling styling so every page stays consistent.

use std::rc::Rc;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike};
use gpui::{
    Anchor, AnyElement, App, ElementId, Entity, FontWeight, MouseButton, Rgba, ScrollHandle,
    SharedString, Window, WindowAppearance, anchored, deferred, div, point, prelude::*, px,
};

use crate::i18n::{Key, k, raw, t};
use crate::icons::{IconName, icon};
use crate::scrollbar::VerticalScrollbar;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

// ── Buttons ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonTone {
    Primary,
    Neutral,
    Danger,
    /// Borderless, transparent until hovered — for quiet in-row actions.
    Ghost,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonSize {
    /// Compact default (px_3 py_1).
    Sm,
    /// Roomier variant (px_4 py_1.5) for page-level primary actions.
    Md,
}

impl ButtonTone {
    /// (bg, hover_bg, fg)
    fn colors(self) -> (Rgba, Rgba, Rgba) {
        match self {
            Self::Primary => (
                theme::accent_fill(),
                theme::accent_hover(),
                theme::accent_text(),
            ),
            Self::Neutral => (theme::inset(), theme::surface_hover(), theme::text()),
            Self::Danger => (theme::red_soft(), theme::red_hover(), theme::red()),
            Self::Ghost => (
                theme::surface().alpha(0.),
                theme::surface_hover(),
                theme::text(),
            ),
        }
    }

    fn is_emphasis(self) -> bool {
        matches!(self, Self::Primary)
    }
}

/// The one button builder. Returns a `Stateful<Div>` so call sites can attach
/// `.on_click(...)` and hover already wired to the tone.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: ButtonTone,
    size: ButtonSize,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    button_base(id, label.clone(), tone, size, true).child(label)
}

/// [`button`] rendered inert: no hover response and no pointer cursor, so it
/// reads as unavailable before it is clicked.
///
/// Attach no `on_click` to the result — a disabled button must not act. Use it
/// wherever a button would otherwise be swapped for empty space, so the control
/// keeps its position and the layout does not jump when it becomes available.
///
/// `muted` fades it to [`DISABLED_OPACITY`]. Pass `false` when the button sits
/// inside a container that is already faded: gpui multiplies nested opacity, and
/// 0.6 × 0.6 is illegible. Exactly one element per subtree should carry it.
pub fn disabled_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: ButtonTone,
    size: ButtonSize,
    muted: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let button = button_base(id, label.clone(), tone, size, false).child(label);
    if muted {
        button.opacity(DISABLED_OPACITY)
    } else {
        button
    }
}

/// Button with a leading icon.
pub fn icon_button_tone(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: IconName,
    tone: ButtonTone,
    size: ButtonSize,
) -> gpui::Stateful<gpui::Div> {
    let (_, _, fg) = tone.colors();
    let label = label.into();
    button_base(id, label.clone(), tone, size, true).child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(icon(name, fg, 14.))
            .child(label),
    )
}

/// 按钮只有 padding、没有宽度约束：放进纵向 flex 列（如 `card()`）会被交叉轴
/// stretch 拉满整行——调用点须套一层 `div().flex().flex_row()` 保持内容宽。
fn button_base(
    id: impl Into<ElementId>,
    label: SharedString,
    tone: ButtonTone,
    size: ButtonSize,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (bg, hover_bg, fg) = tone.colors();
    let mut base = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .font_weight(if tone.is_emphasis() {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        });
    if enabled {
        base = base.cursor_pointer().hover(|s| s.bg(hover_bg));
    }
    match size {
        ButtonSize::Sm => base.px_3().py_1(),
        ButtonSize::Md => base.px_4().py(px(6.)),
    }
}

// Back-compat shims (existing call sites) — new code should use `button` /
// `icon_button_tone` with an explicit tone+size.
pub fn action_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    button(
        id,
        label,
        if primary {
            ButtonTone::Primary
        } else {
            ButtonTone::Neutral
        },
        ButtonSize::Sm,
    )
}

pub fn action_button_tone(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: ButtonTone,
) -> gpui::Stateful<gpui::Div> {
    button(id, label, tone, ButtonSize::Sm)
}

pub fn icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: IconName,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    icon_button_tone(
        id,
        label,
        name,
        if primary {
            ButtonTone::Primary
        } else {
            ButtonTone::Neutral
        },
        ButtonSize::Sm,
    )
}

// ── Form fields ─────────────────────────────────────────────────────────────

/// Vertical field: caption label (optionally marked required), optional muted
/// help line, then the control. The single way to label form controls.
pub fn field(
    label: impl Into<SharedString>,
    required: bool,
    help: Option<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    field_with_error(label, required, help, None, control)
}

/// [`field`] with an inline validation error rendered in red below the control.
pub fn field_with_error(
    label: impl Into<SharedString>,
    required: bool,
    help: Option<SharedString>,
    error: Option<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    let mut label_row = div()
        .flex()
        .flex_row()
        .gap_1()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::subtext())
        .child(label.into());
    if required {
        label_row = label_row.child(div().text_color(theme::red()).child("*"));
    }
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap(px(6.))
        .child(label_row);
    if let Some(help) = help {
        col = col.child(div().text_color(theme::muted()).text_xs().child(help));
    }
    col = col.child(control);
    if let Some(error) = error {
        col = col.child(field_error(error));
    }
    col
}

/// The one inline validation message: red caption text, no box, no icon.
///
/// [`field_with_error`] renders exactly this below its control, so an error
/// raised somewhere a `field` cannot reach — under a grouped row, beside a
/// [`commit_bar`] — still looks like every other error in the app. Show it only
/// while the input is actually invalid; a permanently visible red line reads as
/// decoration and stops being noticed.
pub fn field_error(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_color(theme::red())
        .text_xs()
        .child(message.into())
}

/// Horizontal field for grouped settings rows: semibold label + muted
/// description on the left (flex_1), control pinned right.
pub fn field_row(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .min_w_0()
        .gap_4()
        .px_4()
        .py_3()
        .child(crate::layout::row_label(label, description))
        .child(control)
}

// ── Commit bar ──────────────────────────────────────────────────────────────

/// The footer of a form that **batches** its edits: a dirty indicator on the
/// leading edge, `Discard` and `Save` on the trailing one.
///
/// Use it when a page collects several changes and writes them in one go, so
/// the user can see at a glance whether anything is outstanding and can back
/// out of the whole batch. Do **not** pair it with rows that apply immediately
/// (`layout::switch_row`, `layout::select_row`) — a Save button next to
/// controls that have already taken effect is the exact ambiguity these
/// primitives exist to remove.
///
/// Both buttons go inert (via [`disabled_button`]) whenever there is nothing to
/// commit or a save is already in flight, so a double submit is impossible and
/// the bar keeps its size in every state.
///
/// `id` seeds the two button ids; pass something unique to the form.
pub fn commit_bar(
    id: &'static str,
    dirty: bool,
    saving: bool,
    on_discard: impl Fn(&mut Window, &mut App) + 'static,
    on_save: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    let live = dirty && !saving;
    let (tone, status) = if saving {
        (theme::accent(), t(k::COMMON_COMMIT_BAR_SAVING))
    } else if dirty {
        (theme::yellow(), t(k::COMMON_COMMIT_BAR_DIRTY))
    } else {
        (theme::green(), t(k::COMMON_COMMIT_BAR_CLEAN))
    };

    let discard_id = ElementId::Name(SharedString::from(format!("{id}-discard")));
    let discard = if live {
        button(
            discard_id,
            t(k::COMMON_COMMIT_BAR_DISCARD),
            ButtonTone::Neutral,
            ButtonSize::Md,
        )
        .on_click(move |_event, window, cx| on_discard(window, cx))
        .into_any_element()
    } else {
        disabled_button(
            discard_id,
            t(k::COMMON_COMMIT_BAR_DISCARD),
            ButtonTone::Neutral,
            ButtonSize::Md,
            true,
        )
        .into_any_element()
    };

    let save_id = ElementId::Name(SharedString::from(format!("{id}-save")));
    let save = if live {
        button(
            save_id,
            t(k::COMMON_COMMIT_BAR_SAVE),
            ButtonTone::Primary,
            ButtonSize::Md,
        )
        .on_click(move |_event, window, cx| on_save(window, cx))
        .into_any_element()
    } else {
        disabled_button(
            save_id,
            t(k::COMMON_COMMIT_BAR_SAVE),
            ButtonTone::Primary,
            ButtonSize::Md,
            true,
        )
        .into_any_element()
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .w_full()
        .min_w_0()
        .px_4()
        .py_3()
        .bg(theme::surface())
        .border_t_1()
        .border_color(theme::border())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .gap_2()
                .child(status_dot_sized(tone, 6.))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::subtext())
                        .child(status),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .gap_2()
                .child(discard)
                .child(save),
        )
}

// ── Segmented control ───────────────────────────────────────────────────────

/// Single-select pill row (the one replacement for every hand-rolled
/// pill/chip selector): inset track, selected item raised with a hairline
/// shadow. `on_select(index, window, cx)` fires on click. `id` accepts
/// `&'static str` or an owned `SharedString` for schema-driven ids.
pub fn segmented(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let on_select: SelectHandler = Rc::new(on_select);
    segmented_track(id.into(), options, selected, Some(on_select))
}

/// [`segmented`] with the interactivity removed: same track, same raised
/// selected pill, but no click handlers, no hover response and no pointer
/// cursor. For a disabled row that must still show which option is in force —
/// hiding the option set would make "why can't I change this?" harder to answer.
pub fn segmented_readonly(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
) -> gpui::Stateful<gpui::Div> {
    segmented_track(id.into(), options, selected, None)
}

type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

fn segmented_track(
    id: SharedString,
    options: &[&str],
    selected: usize,
    on_select: Option<SelectHandler>,
) -> gpui::Stateful<gpui::Div> {
    let mut track = div()
        .id(id.clone())
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .flex_none()
        .max_w_full()
        .min_w_0()
        .gap(px(2.))
        .p(px(2.))
        .rounded_lg()
        .bg(theme::inset());
    for (ix, option) in options.iter().enumerate() {
        let is_selected = ix == selected;
        let mut item = div()
            .id(SharedString::from(format!("{id}-{ix}")))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(tf!(
                k::COMMON_SEGMENTED_OPTION_ARIA,
                id = id,
                option = option
            )))
            .aria_selected(is_selected)
            .flex_none()
            .max_w_full()
            .overflow_hidden()
            .px_3()
            .py_1()
            .rounded_md()
            .text_sm()
            .child(SharedString::from(option.to_string()));
        if on_select.is_some() {
            item = item.cursor_pointer();
        }
        if is_selected {
            item = item
                .bg(theme::surface())
                .shadow_xs()
                .text_color(theme::text())
                .font_weight(FontWeight::MEDIUM);
        } else {
            item = item.text_color(theme::muted());
            if on_select.is_some() {
                item = item.hover(|s| s.text_color(theme::subtext()));
            }
        }
        if let Some(on_select) = on_select.clone() {
            item = item.on_click(move |_event, window, cx| on_select(ix, window, cx));
        }
        track = track.child(item);
    }
    track
}

// ── Select dropdown ────────────────────────────────────────────────────────

/// A selection control should stop laying every option out horizontally once
/// either the list itself is long or the labels would make a compact list wrap.
///
/// The weighted character count treats non-ASCII glyphs as roughly two Latin
/// characters, which is close enough for deciding between the two controls
/// without coupling schema data to a particular font or window width.
pub fn select_prefers_dropdown(options: &[&str]) -> bool {
    if options.len() >= 5 {
        return true;
    }
    if options.len() < 3 {
        return false;
    }
    let label_units: usize = options
        .iter()
        .map(|label| {
            label
                .chars()
                .map(|character| if character.is_ascii() { 1 } else { 2 })
                .sum::<usize>()
        })
        .sum();
    label_units >= 32
}

/// The two state changes emitted by [`select_dropdown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectDropdownEvent {
    Open(bool),
    Select(usize),
}

type SelectDropdownHandler = Rc<dyn Fn(SelectDropdownEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy)]
enum SelectDropdownChrome {
    Field,
    Sidebar(WindowAppearance),
}

/// A compact single-select trigger with a popover list. The owner keeps the
/// open state so virtualized forms can unmount/remount the control without
/// hiding mutable state inside a short-lived element.
pub fn select_dropdown(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    open: bool,
    on_event: impl Fn(SelectDropdownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    select_dropdown_control(
        id.into(),
        options,
        selected,
        open,
        None,
        SelectDropdownChrome::Field,
        Some(Rc::new(on_event)),
    )
}

/// Sidebar counterpart of [`select_dropdown`].
///
/// The trigger deliberately has no field chrome: it rests directly on the
/// sidebar backdrop and only gains the same hover/open highlight as navigation
/// rows. The popover menu remains identical to the regular selector.
pub fn select_dropdown_sidebar(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    open: bool,
    appearance: WindowAppearance,
    on_event: impl Fn(SelectDropdownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    select_dropdown_control(
        id.into(),
        options,
        selected,
        open,
        None,
        SelectDropdownChrome::Sidebar(appearance),
        Some(Rc::new(on_event)),
    )
}

/// [`select_dropdown`] with guidance text for an unselected value.
///
/// The placeholder is shown only in the trigger and is never inserted into the
/// option list, so required selectors cannot accidentally persist a fake
/// "empty" choice.
pub fn select_dropdown_with_placeholder(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    open: bool,
    placeholder: impl Into<SharedString>,
    on_event: impl Fn(SelectDropdownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    select_dropdown_control(
        id.into(),
        options,
        selected,
        open,
        Some(placeholder.into()),
        SelectDropdownChrome::Field,
        Some(Rc::new(on_event)),
    )
}

/// Read-only counterpart used by disabled settings rows.
pub fn select_dropdown_readonly(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
) -> gpui::Stateful<gpui::Div> {
    select_dropdown_control(
        id.into(),
        options,
        selected,
        false,
        None,
        SelectDropdownChrome::Field,
        None,
    )
}

fn select_dropdown_control(
    id: SharedString,
    options: &[&str],
    selected: usize,
    open: bool,
    placeholder: Option<SharedString>,
    chrome: SelectDropdownChrome,
    on_event: Option<SelectDropdownHandler>,
) -> gpui::Stateful<gpui::Div> {
    let has_selection = options.get(selected).is_some();
    let current = options
        .get(selected)
        .map(|option| SharedString::from((*option).to_string()))
        .or(placeholder)
        .unwrap_or_default();
    let mut trigger = div()
        .id(ElementId::Name(format!("{id}-trigger").into()))
        .role(gpui::Role::ComboBox)
        .aria_label(current.clone())
        .aria_expanded(open)
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded_lg()
        .text_sm()
        .child(div().min_w_0().flex_1().truncate().child(current))
        .child(icon(IconName::ChevronDown, theme::muted(), 13.));
    trigger = match chrome {
        SelectDropdownChrome::Field => trigger
            .h(px(38.))
            .px_3()
            .border_1()
            .border_color(if open {
                theme::accent()
            } else {
                theme::border_strong()
            })
            .bg(theme::surface())
            .text_color(if has_selection {
                theme::text()
            } else {
                theme::muted()
            }),
        SelectDropdownChrome::Sidebar(appearance) => trigger
            .h(px(34.))
            .px_2()
            .text_color(if has_selection {
                theme::sidebar_glass_text(appearance)
            } else {
                theme::sidebar_glass_muted(appearance)
            })
            .when(open, |style| {
                style
                    .bg(theme::accent_soft())
                    .font_weight(FontWeight::MEDIUM)
            }),
    };
    if let Some(handler) = on_event.clone() {
        trigger = trigger.cursor_pointer();
        trigger = match chrome {
            SelectDropdownChrome::Field => {
                trigger.hover(|style| style.border_color(theme::accent()).bg(theme::panel()))
            }
            SelectDropdownChrome::Sidebar(_) => trigger.hover(|style| {
                style
                    .bg(theme::surface_hover())
                    .text_color(theme::sidebar_text())
            }),
        };
        trigger = trigger.on_mouse_down(MouseButton::Left, move |_event, window, cx| {
            handler(SelectDropdownEvent::Open(!open), window, cx)
        });
    }

    let mut control = div()
        .id(ElementId::Name(format!("{id}-control").into()))
        .relative()
        .w_full()
        .min_w_0()
        .child(trigger);
    let Some(handler) = on_event else {
        return control;
    };
    if !open {
        return control;
    }

    let mut menu = div()
        .id(ElementId::Name(format!("{id}-menu").into()))
        .role(gpui::Role::List)
        .w_full()
        .min_w(px(220.))
        .max_h(px(280.))
        .overflow_y_scroll()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(theme::border())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude();
    for (index, option) in options.iter().enumerate() {
        let is_selected = index == selected;
        let callback = handler.clone();
        let mut item = div()
            .id(ElementId::Name(format!("{id}-option-{index}").into()))
            .role(gpui::Role::ListBoxOption)
            .aria_label(SharedString::from((*option).to_string()))
            .aria_selected(is_selected)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .min_h(px(34.))
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(SharedString::from((*option).to_string())),
            );
        if is_selected {
            item = item
                .bg(theme::accent_soft())
                .text_color(theme::accent())
                .font_weight(FontWeight::MEDIUM)
                .child(icon(IconName::Check, theme::accent(), 13.));
        } else {
            item = item
                .text_color(theme::subtext())
                .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()));
        }
        menu = menu.child(item.on_click(move |_event, window, cx| {
            callback(SelectDropdownEvent::Select(index), window, cx)
        }));
    }
    let dismiss = handler;
    menu = menu.on_mouse_down_out(move |_event, window, cx| {
        dismiss(SelectDropdownEvent::Open(false), window, cx)
    });

    control = control.child(
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .offset(point(px(0.), px(4.)))
                .snap_to_window_with_margin(px(8.))
                .child(menu),
        )
        .priority(30),
    );
    control
}

// ── Time ────────────────────────────────────────────────────────────────────

/// A Unix timestamp as local wall-clock text.
///
/// Lives here rather than in one page because a raw `last_sync_at` integer is
/// a machine value, and every page that has one needs the same answer. Pass
/// `with_seconds` when the exact second matters (a log line, a range bound);
/// leave it off for a status readout.
pub fn format_local_timestamp(timestamp: i64, with_seconds: bool) -> String {
    let pattern = if with_seconds {
        "%Y/%m/%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format(pattern).to_string())
        .unwrap_or_else(|| crate::i18n::raw(k::COMMON_TIME_INVALID).to_string())
}

// ── Badges & status dots ────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
    Teal,
    Mauve,
    Peach,
}

impl BadgeTone {
    /// (soft bg, fg)
    fn colors(self) -> (Rgba, Rgba) {
        match self {
            Self::Neutral => (theme::inset(), theme::subtext()),
            Self::Accent => (theme::accent_soft(), theme::accent()),
            Self::Success => (theme::green_soft(), theme::green()),
            Self::Warning => (theme::yellow_soft(), theme::yellow()),
            Self::Danger => (theme::red_soft(), theme::red()),
            Self::Teal => (theme::teal().alpha(0.12), theme::teal()),
            Self::Mauve => (theme::mauve().alpha(0.12), theme::mauve()),
            Self::Peach => (theme::peach().alpha(0.14), theme::peach()),
        }
    }
}

/// Pill badge: soft tinted background, tone text, caption size.
pub fn badge(tone: BadgeTone, label: impl Into<SharedString>) -> gpui::Div {
    let (bg, fg) = tone.colors();
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(label.into())
}

/// 8px round status dot.
pub fn status_dot(color: Rgba) -> gpui::Div {
    div()
        .w(px(8.))
        .h(px(8.))
        .flex_none()
        .rounded_full()
        .bg(color)
}

/// Custom-size round status dot.
pub fn status_dot_sized(color: Rgba, diameter: f32) -> gpui::Div {
    div()
        .w(px(diameter))
        .h(px(diameter))
        .flex_none()
        .rounded_full()
        .bg(color)
}

// ── Sponsor marks ───────────────────────────────────────────────────────────

/// A sponsor's mark, on a fixed light tile of `tile` px.
///
/// Unlike the OcHub wordmark — solid ink, so [`crate::about_view`] can swap two
/// files by palette polarity — these are full-colour third-party marks we must
/// not recolour, and their transparent backgrounds contain both near-black and
/// near-white pixels: on a themed surface the black vanishes in dark mode and
/// the white vanishes in light mode. A constant near-white tile, the same
/// treatment an OS gives an app icon, keeps one file per sponsor correct in
/// both palettes.
///
/// `asset` is a path under `crates/app/assets/`, as carried by
/// `provider_config::Sponsor::logo`.
pub fn sponsor_logo(asset: &'static str, tile: f32) -> gpui::Div {
    div()
        .size(px(tile))
        .flex_none()
        .rounded_md()
        .bg(theme::c(0xFFFFFF))
        .border_1()
        .border_color(theme::border())
        .flex()
        .items_center()
        .justify_center()
        .child(gpui::img(asset).size(px((tile * 0.74).round())))
}

// ── Cards ───────────────────────────────────────────────────────────────────

/// The one card: white surface, hairline border, 8px radius, 16px padding.
/// For grouped row layouts use `layout::group` instead.
pub fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .rounded_lg()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .p_4()
}

/// Opacity applied to disabled interactive elements. Keep call sites on this
/// constant so the disabled look stays uniform across pages.
pub const DISABLED_OPACITY: f32 = 0.6;

/// Legacy thin panel (kept for the provider list hero/card; new code: `card`).
pub fn panel() -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
}

// ── Empty states ────────────────────────────────────────────────────────────

/// Centered empty placeholder: muted icon, title, hint, optional CTA.
pub fn empty_state(
    icon_name: IconName,
    title: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cta: Option<AnyElement>,
) -> gpui::Div {
    let mut col = div()
        .flex()
        .flex_col()
        .items_center()
        .w_full()
        .gap_2()
        .py_12()
        .child(icon(icon_name, theme::muted(), 26.))
        .child(
            div()
                .text_color(theme::subtext())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(hint.into()),
        );
    if let Some(cta) = cta {
        col = col.child(div().mt_2().child(cta));
    }
    col
}

// ── Modal chrome ────────────────────────────────────────────────────────────

/// Dimmed full-window overlay centering its child. Attach dismissal on the
/// overlay; the child card is `.occlude()`d so clicks don't pass through.
pub fn modal_overlay(child: impl IntoElement) -> gpui::Div {
    div()
        .absolute()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::scrim().alpha(if theme::is_dark() { 0.68 } else { 0.45 }))
        .occlude()
        .child(child)
}

/// The centered dialog card. Pair with [`modal_overlay`].
pub fn modal_card() -> gpui::Div {
    div()
        .w(px(520.))
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(theme::overlay())
        .border_1()
        .border_color(theme::border())
        .shadow(theme::shadow_popover())
        .occlude()
}

/// Modal title bar (title + trailing slot via `.child(...)`).
pub fn modal_header(title: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_5()
        .py_4()
        .border_b_1()
        .border_color(theme::border())
        .child(
            div()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// Padded modal content region.
pub fn modal_body() -> gpui::Div {
    div().flex().flex_col().gap_3().px_5().py_4()
}

/// Modal action row (right-aligned buttons).
pub fn modal_footer(actions: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap_2()
        .px_5()
        .py_3()
        .border_t_1()
        .border_color(theme::border())
        .children(actions)
}

// ── Disclosure ──────────────────────────────────────────────────────────────

/// Expandable section header: chevron + title + muted detail. Attach
/// `.on_click(...)` to toggle; render the body below when `expanded`.
pub fn disclosure(
    id: &'static str,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    expanded: bool,
) -> gpui::Stateful<gpui::Div> {
    let title = title.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(title.clone())
        .aria_expanded(expanded)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .cursor_pointer()
        .child(icon(
            if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
            theme::muted(),
            14.,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w_0()
                .child(
                    div()
                        .text_color(theme::text())
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(detail.into()),
                ),
        )
}

// ── Stat tile ───────────────────────────────────────────────────────────────

/// Metric card: dot or icon + caption label, big value, muted detail line.
/// The single tile for gateway/tools/usage dashboards.
pub fn stat_tile(
    icon_name: Option<IconName>,
    tone: Rgba,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> gpui::Div {
    let marker = match icon_name {
        Some(name) => div()
            .flex()
            .items_center()
            .justify_center()
            .w(px(20.))
            .h(px(20.))
            .rounded_md()
            .bg(tone.alpha(0.12))
            .child(icon(name, tone, 12.))
            .into_any_element(),
        None => status_dot(tone).into_any_element(),
    };
    card()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(marker)
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(label.into()),
                ),
        )
        .child(
            div()
                .text_color(theme::text())
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .child(value.into()),
        )
        .child(
            div()
                .text_color(theme::subtext())
                .text_xs()
                .child(detail.into()),
        )
}

// ── Table ───────────────────────────────────────────────────────────────────

/// Table header row: caption labels over a hairline. Pair with [`table_row`];
/// wrap both in a `card().p_0()` (or `layout::group`-style container).
pub fn table_header(cols: &[&str]) -> gpui::Div {
    let mut header = div()
        .grid()
        .grid_cols(cols.len() as u16)
        .gap_2()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(theme::border());
    for col in cols {
        header = header.child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(col.to_string())),
        );
    }
    header
}

/// Table body row: grid-aligned cells, hairline separator unless `last`.
pub fn table_row(cells: Vec<AnyElement>, n_cols: usize, last: bool) -> gpui::Div {
    let row = div()
        .grid()
        .grid_cols(n_cols as u16)
        .gap_2()
        .px_4()
        .py_2()
        .items_center()
        .when(!last, |s| s.border_b_1().border_color(theme::border()));
    row.children(cells)
}

// ── Pagination ──────────────────────────────────────────────────────────────

pub fn datetime_filter_field(
    id: impl Into<ElementId>,
    label: &'static str,
    input: Entity<TextInput>,
    expanded: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .aria_label(label)
        .aria_expanded(expanded)
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .cursor_pointer()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::subtext())
                .child(label),
        )
        .child(
            div().relative().w_full().child(input).child(
                div()
                    .absolute()
                    .right(px(10.))
                    .top(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::surface())
                    .child(icon(
                        IconName::Calendar,
                        if expanded {
                            theme::accent()
                        } else {
                            theme::muted()
                        },
                        15.,
                    )),
            ),
        )
}

#[allow(clippy::too_many_arguments)]
pub fn datetime_picker(
    id: &'static str,
    selected: DateTime<Local>,
    picker_year: i32,
    picker_month: u32,
    hour_scroll: &ScrollHandle,
    minute_scroll: &ScrollHandle,
    on_shift_month: impl Fn(i32, &mut Window, &mut App) + 'static,
    on_select_date: impl Fn(NaiveDate, &mut Window, &mut App) + 'static,
    on_select_hour: impl Fn(u32, &mut Window, &mut App) + 'static,
    on_select_minute: impl Fn(u32, &mut Window, &mut App) + 'static,
    on_today: impl Fn(&mut Window, &mut App) + 'static,
    on_clear: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let on_shift_month = std::rc::Rc::new(on_shift_month);
    let on_select_date = std::rc::Rc::new(on_select_date);
    let on_select_hour = std::rc::Rc::new(on_select_hour);
    let on_select_minute = std::rc::Rc::new(on_select_minute);
    let on_today = std::rc::Rc::new(on_today);
    let on_clear = std::rc::Rc::new(on_clear);
    let selected_date = selected.date_naive();
    let selected_hour = selected.hour();
    let selected_minute = selected.minute();
    let today = Local::now().date_naive();
    let first_of_month = NaiveDate::from_ymd_opt(picker_year, picker_month, 1).unwrap_or(today);
    let calendar_start =
        first_of_month - Duration::days(first_of_month.weekday().num_days_from_sunday() as i64);

    let mut weekday_header = div().grid().grid_cols(7).gap(px(2.)).w_full();
    for weekday in [
        t(k::COMMON_CALENDAR_WEEKDAY_SUN),
        t(k::COMMON_CALENDAR_WEEKDAY_MON),
        t(k::COMMON_CALENDAR_WEEKDAY_TUE),
        t(k::COMMON_CALENDAR_WEEKDAY_WED),
        t(k::COMMON_CALENDAR_WEEKDAY_THU),
        t(k::COMMON_CALENDAR_WEEKDAY_FRI),
        t(k::COMMON_CALENDAR_WEEKDAY_SAT),
    ] {
        weekday_header = weekday_header.child(
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::muted())
                .child(weekday),
        );
    }

    let mut day_grid = div().grid().grid_cols(7).gap(px(2.)).w_full();
    for index in 0..42 {
        let date = calendar_start + Duration::days(index);
        let callback = on_select_date.clone();
        day_grid = day_grid.child(
            calendar_day_button(
                ElementId::Name(
                    format!("{id}-day-{}-{}-{}", date.year(), date.month(), date.day()).into(),
                ),
                date,
                date.month() == picker_month,
                date == selected_date,
                date == today,
            )
            .on_click(move |_event, window, cx| callback(date, window, cx)),
        );
    }

    let previous_month = on_shift_month.clone();
    let next_month = on_shift_month.clone();
    let clear = on_clear.clone();
    let today_callback = on_today.clone();
    let calendar = div()
        .w(px(236.))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(42.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(SharedString::from(tf!(
                            k::COMMON_CALENDAR_MONTH_TITLE,
                            year = picker_year,
                            month = month_label(picker_month)
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                            calendar_nav_button(
                                ElementId::Name(format!("{id}-previous-month").into()),
                                raw(k::COMMON_CALENDAR_PREVIOUS_MONTH_ARIA),
                                IconName::ChevronLeft,
                            )
                            .on_click(move |_event, window, cx| previous_month(-1, window, cx)),
                        )
                        .child(
                            calendar_nav_button(
                                ElementId::Name(format!("{id}-next-month").into()),
                                raw(k::COMMON_CALENDAR_NEXT_MONTH_ARIA),
                                IconName::ChevronRight,
                            )
                            .on_click(move |_event, window, cx| next_month(1, window, cx)),
                        ),
                ),
        )
        .child(div().px_3().child(weekday_header).child(day_grid))
        .child(
            div()
                .mt_auto()
                .h(px(42.))
                .px_3()
                .border_t_1()
                .border_color(theme::border())
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    calendar_footer_button(
                        ElementId::Name(format!("{id}-clear").into()),
                        t(k::COMMON_CALENDAR_CLEAR_LABEL),
                    )
                    .on_click(move |_event, window, cx| clear(window, cx)),
                )
                .child(
                    calendar_footer_button(
                        ElementId::Name(format!("{id}-today").into()),
                        t(k::COMMON_CALENDAR_TODAY_LABEL),
                    )
                    .on_click(move |_event, window, cx| today_callback(window, cx)),
                ),
        );

    let hour_scroll_id = SharedString::from(format!("{id}-hours"));
    let mut hour_options = div()
        .id(hour_scroll_id.clone())
        .w(px(54.))
        .h(px(252.))
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(hour_scroll)
        .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
            hour_scroll.clone(),
        ));
    for hour in 0..24u32 {
        let callback = on_select_hour.clone();
        hour_options = hour_options.child(
            time_value_button(
                ElementId::Name(format!("{hour_scroll_id}-{hour}").into()),
                hour,
                hour == selected_hour,
            )
            .on_click(move |_event, window, cx| callback(hour, window, cx)),
        );
    }

    let minute_scroll_id = SharedString::from(format!("{id}-minutes"));
    let mut minute_options = div()
        .id(minute_scroll_id.clone())
        .w(px(54.))
        .h(px(252.))
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(minute_scroll)
        .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
            minute_scroll.clone(),
        ));
    for minute in 0..60u32 {
        let callback = on_select_minute.clone();
        minute_options = minute_options.child(
            time_value_button(
                ElementId::Name(format!("{minute_scroll_id}-{minute}").into()),
                minute,
                minute == selected_minute,
            )
            .on_click(move |_event, window, cx| callback(minute, window, cx)),
        );
    }

    let time_selector = div()
        .w(px(111.))
        .h_full()
        .flex_none()
        .flex()
        .flex_row()
        .child(
            div()
                .w(px(55.))
                .flex()
                .flex_col()
                .items_center()
                .child(time_column_label(raw(k::COMMON_CALENDAR_HOUR_LABEL)))
                .child(
                    div()
                        .relative()
                        .w(px(54.))
                        .h(px(252.))
                        .child(hour_options)
                        .child(VerticalScrollbar::new(
                            ElementId::Name(format!("{id}-hours-scrollbar").into()),
                            hour_scroll.clone(),
                        )),
                ),
        )
        .child(
            div()
                .w(px(56.))
                .border_l_1()
                .border_color(theme::border())
                .flex()
                .flex_col()
                .items_center()
                .child(time_column_label(raw(k::COMMON_CALENDAR_MINUTE_LABEL)))
                .child(
                    div()
                        .relative()
                        .w(px(54.))
                        .h(px(252.))
                        .child(minute_options)
                        .child(VerticalScrollbar::new(
                            ElementId::Name(format!("{id}-minutes-scrollbar").into()),
                            minute_scroll.clone(),
                        )),
                ),
        );

    div()
        .id(ElementId::Name(format!("{id}-popover").into()))
        .flex()
        .w(px(348.))
        .h(px(296.))
        .rounded_lg()
        .border_1()
        .border_color(theme::border())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude()
        .flex_row()
        .child(calendar)
        .child(div().w(px(1.)).h_full().bg(theme::border()))
        .child(time_selector)
}

/// The month as a calendar header spells it: a name in English, digits in
/// Chinese and Japanese. It is the `{month}` of `common.calendar.month.title`,
/// so the label is translated separately from the order it sits in.
fn month_label(month: u32) -> String {
    const NAMES: [Key; 12] = [
        k::COMMON_CALENDAR_MONTH_NAME_01,
        k::COMMON_CALENDAR_MONTH_NAME_02,
        k::COMMON_CALENDAR_MONTH_NAME_03,
        k::COMMON_CALENDAR_MONTH_NAME_04,
        k::COMMON_CALENDAR_MONTH_NAME_05,
        k::COMMON_CALENDAR_MONTH_NAME_06,
        k::COMMON_CALENDAR_MONTH_NAME_07,
        k::COMMON_CALENDAR_MONTH_NAME_08,
        k::COMMON_CALENDAR_MONTH_NAME_09,
        k::COMMON_CALENDAR_MONTH_NAME_10,
        k::COMMON_CALENDAR_MONTH_NAME_11,
        k::COMMON_CALENDAR_MONTH_NAME_12,
    ];
    match NAMES.get(month.wrapping_sub(1) as usize) {
        Some(key) => raw(*key).to_string(),
        // Unreachable for a real date; keeps the pre-catalog `{month:02}`.
        None => format!("{month:02}"),
    }
}

fn calendar_nav_button(
    id: impl Into<ElementId>,
    label: &'static str,
    icon_name: IconName,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(theme::surface_hover()))
        .child(icon(icon_name, theme::subtext(), 14.))
}

fn calendar_day_button(
    id: impl Into<ElementId>,
    date: NaiveDate,
    in_current_month: bool,
    selected: bool,
    today: bool,
) -> gpui::Stateful<gpui::Div> {
    let day = date.day();
    let mut button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(tf!(
            k::COMMON_CALENDAR_DAY_ARIA,
            year = date.year(),
            month = date.month(),
            day = day
        )))
        .aria_selected(selected)
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(theme::surface().alpha(0.))
        .cursor_pointer()
        .text_sm()
        .child(SharedString::from(day.to_string()));
    if selected {
        button = button
            .bg(theme::accent_fill())
            .border_color(theme::accent())
            .text_color(theme::accent_text())
            .font_weight(FontWeight::SEMIBOLD);
    } else if today {
        button = button
            .border_color(theme::accent())
            .text_color(theme::accent())
            .font_weight(FontWeight::MEDIUM)
            .hover(|style| style.bg(theme::accent_soft()));
    } else if in_current_month {
        button = button
            .text_color(theme::text())
            .hover(|style| style.bg(theme::surface_hover()));
    } else {
        button = button.text_color(theme::muted()).hover(|style| {
            style
                .bg(theme::surface_hover())
                .text_color(theme::subtext())
        });
    }
    button
}

fn calendar_footer_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .px_1()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::accent())
        .hover(|style| style.bg(theme::accent_soft()))
        .child(label)
}

fn time_column_label(label: &'static str) -> gpui::Div {
    div()
        .h(px(42.))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::muted())
        .child(label)
}

fn time_value_button(
    id: impl Into<ElementId>,
    value: u32,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(format!("{value:02}")))
        .aria_selected(selected)
        .w_full()
        .h(px(34.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_sm()
        .child(SharedString::from(format!("{value:02}")));
    if selected {
        button
            .bg(theme::accent_fill())
            .text_color(theme::accent_text())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        button
            .text_color(theme::text())
            .hover(|style| style.bg(theme::surface_hover()))
    }
}

/// Footer pagination bar: prev button, "3 / 12" label, next button. Buttons
/// are built (and wired) at the call site with `components::button`.
/// 解析日期输入：支持 `YYYY-MM-DD`、`YYYY/MM/DD`、`MM-DD`（按当前年补全）。
pub fn parse_jump_date(text: &str) -> Option<NaiveDate> {
    let t = text.trim().replace('/', "-");
    if t.is_empty() {
        return None;
    }
    if let Ok(date) = NaiveDate::parse_from_str(&t, "%Y-%m-%d") {
        return Some(date);
    }
    NaiveDate::parse_from_str(&format!("{}-{t}", Local::now().year()), "%Y-%m-%d").ok()
}

/// 翻页条（antd 风格）：`[1] 2 … N   跳至 [x] 页          共 N 条`。
/// 页码按钮直接点击切页，当前页高亮为 accent 实底；间断处补省略号；
/// “跳至”输入框由调用方持有并在构造时通过 [`TextInput::set_on_enter`]
/// 接回车提交（本组件只负责渲染布局）。`on_select_page` 收 0-based 页码。
/// 旧的 [`pagination`]（仅上/下页）保留给不需要跳转的简单场合。
#[allow(clippy::too_many_arguments)]
pub fn pagination_bar(
    id: &'static str,
    page: u32,
    total_pages: u32,
    total_items: Option<u64>,
    page_size: u32,
    page_sizes: &'static [u32],
    page_size_open: bool,
    page_input: &Entity<TextInput>,
    on_select_page: impl Fn(u32, &mut Window, &mut App) + 'static,
    on_toggle_page_size: impl Fn(&mut Window, &mut App) + 'static,
    on_select_page_size: impl Fn(u32, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let total_pages = total_pages.max(1);
    let last = total_pages - 1;
    let page = page.min(last);
    let on_select_page = std::rc::Rc::new(on_select_page);
    let on_toggle_page_size = std::rc::Rc::new(on_toggle_page_size);
    let on_select_page_size = std::rc::Rc::new(on_select_page_size);

    // 页码集合：首页、末页、当前页 ±1；端点附近再补一格，间断处渲染省略号。
    let mut numbers = std::collections::BTreeSet::new();
    numbers.insert(0);
    numbers.insert(last);
    for delta in -1i64..=1 {
        let candidate = page as i64 + delta;
        if (0..=last as i64).contains(&candidate) {
            numbers.insert(candidate as u32);
        }
    }
    if page <= 1 && last >= 1 {
        numbers.insert(1);
    }
    if page + 2 >= last && last >= 1 {
        numbers.insert(last - 1);
    }

    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap_1()
        .w_full()
        .py_2()
        .child(pagination_nav_button(
            ElementId::Name(format!("{id}-previous").into()),
            raw(k::COMMON_PAGINATION_PREVIOUS_ARIA),
            IconName::ChevronLeft,
            page == 0,
            {
                let callback = on_select_page.clone();
                move |window, cx| callback(page.saturating_sub(1), window, cx)
            },
        ));

    let mut previous: Option<u32> = None;
    for number in numbers {
        if let Some(prev) = previous
            && number > prev + 1
        {
            bar = bar.child(div().px_1().text_color(theme::muted()).text_sm().child("…"));
        }
        previous = Some(number);

        let is_current = number == page;
        let mut cell = div()
            .id(ElementId::Name(format!("{id}-p{number}").into()))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(tf!(
                k::COMMON_PAGINATION_PAGE_ARIA,
                page = number + 1
            )))
            .aria_selected(is_current)
            .min_w(px(28.))
            .h(px(28.))
            .px_1p5()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_sm();
        if is_current {
            cell = cell
                .bg(theme::accent_fill())
                .text_color(theme::accent_text())
                .font_weight(FontWeight::SEMIBOLD);
        } else {
            let cb = on_select_page.clone();
            cell = cell
                .text_color(theme::subtext())
                .cursor_pointer()
                .hover(|s| s.bg(theme::surface_hover()).text_color(theme::text()))
                .on_click(move |_event, window, cx| cb(number, window, cx));
        }
        bar = bar.child(cell.child(SharedString::from((number + 1).to_string())));
    }

    bar = bar.child(pagination_nav_button(
        ElementId::Name(format!("{id}-next").into()),
        raw(k::COMMON_PAGINATION_NEXT_ARIA),
        IconName::ChevronRight,
        page == last,
        {
            let callback = on_select_page.clone();
            move |window, cx| callback((page + 1).min(last), window, cx)
        },
    ));

    let mut size_popover = div()
        .id(ElementId::Name(format!("{id}-page-size-popover").into()))
        .w(px(108.))
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(theme::border())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude();
    for size in page_sizes.iter().copied() {
        let selected = size == page_size;
        let callback = on_select_page_size.clone();
        let option = div()
            .id(ElementId::Name(format!("{id}-page-size-{size}").into()))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(tf!(
                k::COMMON_PAGINATION_PAGE_SIZE_OPTION_ARIA,
                size = size
            )))
            .aria_selected(selected)
            .h(px(30.))
            .px_2()
            .rounded_md()
            .flex()
            .items_center()
            .justify_between()
            .cursor_pointer()
            .text_sm()
            .text_color(if selected {
                theme::accent()
            } else {
                theme::subtext()
            })
            .when(selected, |option| option.bg(theme::accent_soft()))
            .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()))
            .child(SharedString::from(tf!(
                k::COMMON_PAGINATION_PAGE_SIZE_OPTION,
                size = size
            )))
            .when(selected, |option| {
                option.child(icon(IconName::Check, theme::accent(), 12.))
            })
            .on_click(move |_event, window, cx| callback(size, window, cx));
        size_popover = size_popover.child(option);
    }
    let close_page_size = on_toggle_page_size.clone();
    size_popover =
        size_popover.on_mouse_down_out(move |_event, window, cx| close_page_size(window, cx));
    let toggle_page_size = on_toggle_page_size.clone();
    let size_control = div()
        .relative()
        .flex_none()
        .child(
            div()
                .id(ElementId::Name(format!("{id}-page-size").into()))
                .role(gpui::Role::Button)
                .aria_label(t(k::COMMON_PAGINATION_PAGE_SIZE_TRIGGER_ARIA))
                .aria_expanded(page_size_open)
                .h(px(28.))
                .px_2()
                .flex()
                .items_center()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_color(if page_size_open {
                    theme::accent()
                } else {
                    theme::border()
                })
                .bg(theme::surface())
                .cursor_pointer()
                .text_sm()
                .text_color(theme::subtext())
                .hover(|style| {
                    style
                        .border_color(theme::accent())
                        .text_color(theme::text())
                })
                .child(SharedString::from(tf!(
                    k::COMMON_PAGINATION_PAGE_SIZE_OPTION,
                    size = page_size
                )))
                .child(icon(IconName::ChevronDown, theme::muted(), 11.))
                .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                    toggle_page_size(window, cx)
                }),
        )
        .when(page_size_open, |control| {
            control.child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(4.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(size_popover),
                )
                .priority(20),
            )
        });

    // The jump row brackets the input: prefix + [x] + suffix. English wants
    // nothing after the box, and a catalog entry can never be empty, so a blank
    // suffix means "render no trailing label" rather than a stray gap.
    let jump_suffix = raw(k::COMMON_PAGINATION_JUMP_SUFFIX);
    bar.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .ml_2()
            .child(
                div()
                    .text_color(theme::subtext())
                    .text_sm()
                    .child(t(k::COMMON_PAGINATION_JUMP_PREFIX)),
            )
            .child(div().w(px(48.)).flex_none().child(page_input.clone()))
            .when(!jump_suffix.trim().is_empty(), |row| {
                row.child(
                    div()
                        .text_color(theme::subtext())
                        .text_sm()
                        .child(t(k::COMMON_PAGINATION_JUMP_SUFFIX)),
                )
            }),
    )
    .child(size_control)
    .child(div().flex_1())
    .when_some(total_items, |bar, total| {
        bar.child(
            div()
                .text_color(theme::muted())
                .text_sm()
                .child(SharedString::from(tf!(
                    k::COMMON_PAGINATION_TOTAL_LABEL,
                    total = total
                ))),
        )
    })
}

fn pagination_nav_button(
    id: impl Into<ElementId>,
    label: &'static str,
    icon_name: IconName,
    disabled: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .child(icon(
            icon_name,
            if disabled {
                theme::muted().alpha(0.45)
            } else {
                theme::subtext()
            },
            12.,
        ));
    if disabled {
        button
    } else {
        button
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_event, window, cx| on_click(window, cx))
    }
}

pub fn pagination(prev: AnyElement, label: impl Into<SharedString>, next: AnyElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .py_3()
        .child(prev)
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(label.into()),
        )
        .child(next)
}

/// Byte counts for humans. Binary units, one decimal place, so a size that
/// grows steadily reads as growing rather than flickering between roundings.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::select_prefers_dropdown;

    #[test]
    fn short_compact_selects_stay_segmented() {
        assert!(!select_prefers_dropdown(&["关闭", "WebDAV", "S3"]));
        assert!(!select_prefers_dropdown(&["Bearer", "API Key"]));
    }

    #[test]
    fn long_or_numerous_selects_use_a_dropdown() {
        assert!(select_prefers_dropdown(&[
            "仅第三方 API",
            "仅 ChatGPT 账号登录",
            "ChatGPT 登录 + 第三方 API",
        ]));
        assert!(select_prefers_dropdown(&[
            "自动", "Terminal", "iTerm2", "Ghostty", "Warp",
        ]));
    }
}
