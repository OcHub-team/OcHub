//! Shared GPUI building blocks for the desktop shell.
//!
//! One component per repeated pattern (see docs/ui-overhaul.md §6): buttons,
//! fields, segmented control, badges/dots, cards, empty states, modal chrome,
//! disclosure, stat tiles, tables, pagination, status footer. Views compose
//! these instead of hand-rolling styling so every page stays consistent.

use chrono::{Datelike, Local, NaiveDate};
use gpui::{
    div, prelude::*, px, AnyElement, App, ElementId, Entity, FontWeight, Rgba, SharedString, Window,
};

use crate::icons::{icon, IconName};
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

/// A [`card`] highlighted as the active editing surface (accent border).
pub fn card_emphasis() -> gpui::Div {
    card().border_color(theme::accent())
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
pub fn pagination_bar(
    id: &'static str,
    page: u32,
    total_pages: u32,
    total_items: Option<u64>,
    page_input: &Entity<TextInput>,
    on_select_page: impl Fn(u32, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let total_pages = total_pages.max(1);
    let last = total_pages - 1;
    let page = page.min(last);
    let on_select_page = std::rc::Rc::new(on_select_page);

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
        .py_3();

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

    bar.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .ml_3()
            .child(div().text_color(theme::subtext()).text_sm().child("跳至"))
            .child(div().w(px(56.)).flex_none().child(page_input.clone()))
            .child(div().text_color(theme::subtext()).text_sm().child("页")),
    )
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
