//! Shared GPUI building blocks for the desktop shell.
//!
//! One component per repeated pattern (see docs/ui-overhaul.md §6): buttons,
//! fields, segmented control, badges/dots, cards, empty states, modal chrome,
//! disclosure, stat tiles, tables, pagination, status footer. Views compose
//! these instead of hand-rolling styling so every page stays consistent.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike};
use gpui::{
    anchored, deferred, div, point, prelude::*, px, Anchor, AnyElement, App, ElementId, Entity,
    FontWeight, MouseButton, Rgba, ScrollHandle, SharedString, Window,
};

use crate::icons::{icon, IconName};
use crate::scrollbar::VerticalScrollbar;
use crate::text_input::TextInput;
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
    button_base(id, label.clone(), tone, size).child(label)
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
    button_base(id, label.clone(), tone, size).child(
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
) -> gpui::Stateful<gpui::Div> {
    let (bg, hover_bg, fg) = tone.colors();
    let base = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .rounded_md()
        .cursor_pointer()
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .font_weight(if tone.is_emphasis() {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        })
        .hover(|s| s.bg(hover_bg));
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
        col = col.child(div().text_color(theme::red()).text_xs().child(error));
    }
    col
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
    let id = id.into();
    let on_select = std::rc::Rc::new(on_select);
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
            .aria_label(SharedString::from(format!("{id} 选项 {option}")))
            .aria_selected(is_selected)
            .flex_none()
            .max_w_full()
            .overflow_hidden()
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .child(SharedString::from(option.to_string()));
        if is_selected {
            item = item
                .bg(theme::surface())
                .shadow_xs()
                .text_color(theme::text())
                .font_weight(FontWeight::MEDIUM);
        } else {
            item = item
                .text_color(theme::muted())
                .hover(|s| s.text_color(theme::subtext()));
        }
        let on_select = on_select.clone();
        item = item.on_click(move |_event, window, cx| on_select(ix, window, cx));
        track = track.child(item);
    }
    track
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
    div()
        .id(id)
        .role(gpui::Role::Button)
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
                        .child(title.into()),
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
    for weekday in ["日", "一", "二", "三", "四", "五", "六"] {
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
                        .child(SharedString::from(format!(
                            "{picker_year}年{picker_month:02}月"
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
                                "上个月",
                                IconName::ChevronLeft,
                            )
                            .on_click(move |_event, window, cx| previous_month(-1, window, cx)),
                        )
                        .child(
                            calendar_nav_button(
                                ElementId::Name(format!("{id}-next-month").into()),
                                "下个月",
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
                    calendar_footer_button(ElementId::Name(format!("{id}-clear").into()), "清除")
                        .on_click(move |_event, window, cx| clear(window, cx)),
                )
                .child(
                    calendar_footer_button(ElementId::Name(format!("{id}-today").into()), "今天")
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
                .child(time_column_label("时"))
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
                .child(time_column_label("分"))
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
        .aria_label(SharedString::from(format!(
            "{}年{}月{}日",
            date.year(),
            date.month(),
            day
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
            "上一页",
            IconName::ChevronLeft,
            page == 0,
            {
                let callback = on_select_page.clone();
                move |window, cx| callback(page.saturating_sub(1), window, cx)
            },
        ));

    let mut previous: Option<u32> = None;
    for number in numbers {
        if let Some(prev) = previous {
            if number > prev + 1 {
                bar = bar.child(div().px_1().text_color(theme::muted()).text_sm().child("…"));
            }
        }
        previous = Some(number);

        let is_current = number == page;
        let mut cell = div()
            .id(ElementId::Name(format!("{id}-p{number}").into()))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("第 {} 页", number + 1)))
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
        "下一页",
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
            .aria_label(SharedString::from(format!("每页 {size} 条")))
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
            .child(SharedString::from(format!("{size} 条/页")))
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
                .aria_label("选择每页条数")
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
                .child(SharedString::from(format!("{page_size} 条/页")))
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

    bar.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .ml_2()
            .child(div().text_color(theme::subtext()).text_sm().child("跳至"))
            .child(div().w(px(48.)).flex_none().child(page_input.clone()))
            .child(div().text_color(theme::subtext()).text_sm().child("页")),
    )
    .child(size_control)
    .child(div().flex_1())
    .when_some(total_items, |bar, total| {
        bar.child(
            div()
                .text_color(theme::muted())
                .text_sm()
                .child(SharedString::from(format!("共 {total} 条"))),
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
