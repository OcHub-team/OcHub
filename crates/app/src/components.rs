//! Shared GPUI building blocks for the desktop shell.
//!
//! One component per repeated pattern (see docs/ui-overhaul.md §6): buttons,
//! fields, segmented control, badges/dots, cards, empty states, modal chrome,
//! disclosure, stat tiles, tables, pagination, status footer. Views compose
//! these instead of hand-rolling styling so every page stays consistent.

use gpui::{
    div, prelude::*, px, AnyElement, App, ElementId, FontWeight, Rgba, SharedString, Window,
};

use crate::icons::{icon, IconName};
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
            Self::Primary => (theme::accent(), theme::accent_hover(), theme::accent_text()),
            Self::Neutral => (theme::inset(), theme::surface_hover(), theme::text()),
            Self::Danger => (theme::red_soft(), theme::c(0xf0d2cc), theme::red()),
            Self::Ghost => (
                theme::c(0xffffff).alpha(0.),
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
    let mut col = div().flex().flex_col().gap(px(6.)).child(label_row);
    if let Some(help) = help {
        col = col.child(div().text_color(theme::muted()).text_xs().child(help));
    }
    col.child(control)
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
/// shadow. `on_select(index, window, cx)` fires on click.
pub fn segmented(
    id: &'static str,
    options: &[&str],
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let on_select = std::rc::Rc::new(on_select);
    let mut track = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
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
        .bg(theme::c(0x000000).alpha(0.45))
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
        .bg(theme::surface())
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
/// The single tile for gateway/proxy/tools/usage dashboards.
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

// ── Status banner / footer ──────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum BannerTone {
    Info,
    Success,
    Warning,
    Error,
}

impl BannerTone {
    fn from_text(text: &str) -> Self {
        if text.contains("失败") || text.contains("错误") || text.contains("不可用") {
            Self::Error
        } else if text.contains("警告") || text.contains("跳过") || text.contains("冲突") {
            Self::Warning
        } else if text.contains("成功") || text.contains("已") {
            Self::Success
        } else {
            Self::Info
        }
    }

    /// (soft bg, accent, fg, icon)
    fn colors(self) -> (Rgba, Rgba, Rgba, IconName) {
        match self {
            Self::Info => (
                theme::accent_soft(),
                theme::accent(),
                theme::text(),
                IconName::Proxy,
            ),
            Self::Success => (
                theme::green_soft(),
                theme::green(),
                theme::text(),
                IconName::Check,
            ),
            Self::Warning => (
                theme::yellow_soft(),
                theme::yellow(),
                theme::text(),
                IconName::Settings,
            ),
            Self::Error => (
                theme::red_soft(),
                theme::red(),
                theme::text(),
                IconName::Wrench,
            ),
        }
    }
}

/// Tinted banner with tone icon; tone auto-detected from the message text.
pub fn status_banner(message: impl Into<SharedString>) -> impl IntoElement {
    let message = message.into();
    status_banner_tone(BannerTone::from_text(&message.to_string()), message)
}

/// Tinted banner with an explicit tone.
pub fn status_banner_tone(tone: BannerTone, message: impl Into<SharedString>) -> impl IntoElement {
    let (bg, accent, fg, icon_name) = tone.colors();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(accent.alpha(0.32))
        .bg(bg.alpha(0.8))
        .child(
            div()
                .mt(px(2.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(accent)
                .child(icon(icon_name, accent, 14.)),
        )
        .child(
            div()
                .text_color(fg)
                .text_sm()
                .line_height(px(18.))
                .child(message.into()),
        )
}

/// The standard status strip at the bottom of a page: renders the banner when
/// `status` is `Some`, collapses otherwise. Replaces the ~14 hand-rolled
/// copies scattered across views.
pub fn status_footer(status: Option<SharedString>) -> gpui::Div {
    div()
        .px_6()
        .py_2()
        .when_some(status, |s, message| s.child(status_banner(message)))
}
