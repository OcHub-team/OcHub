//! Shared GPUI building blocks for the desktop shell.

use gpui::{div, prelude::*, px, ElementId, FontWeight, SharedString};

use crate::icons::{icon, IconName};
use crate::theme;

#[derive(Clone, Copy)]
pub enum ButtonTone {
    Primary,
    Neutral,
    Danger,
}

impl ButtonTone {
    fn colors(self) -> (u32, u32, u32, u32) {
        match self {
            Self::Primary => (
                theme::ACCENT,
                theme::ACCENT_HOVER,
                theme::ACCENT_TEXT,
                theme::ACCENT,
            ),
            // Subtle filled neutral with a ghost hover (no border, no shadow).
            Self::Neutral => (
                theme::INSET,
                theme::SURFACE_HOVER,
                theme::TEXT,
                theme::BORDER,
            ),
            Self::Danger => (theme::RED_SOFT, 0xf0d2cc, theme::RED, theme::RED_SOFT),
        }
    }

    fn is_emphasis(self) -> bool {
        matches!(self, Self::Primary)
    }
}

pub fn action_button(
    id: impl Into<ElementId>,
    label: &'static str,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let tone = if primary {
        ButtonTone::Primary
    } else {
        ButtonTone::Neutral
    };
    action_button_tone(id, label, tone)
}

pub fn action_button_tone(
    id: impl Into<ElementId>,
    label: &'static str,
    tone: ButtonTone,
) -> gpui::Stateful<gpui::Div> {
    button_base(id, label, tone).child(label)
}

fn button_base(
    id: impl Into<ElementId>,
    label: &'static str,
    tone: ButtonTone,
) -> gpui::Stateful<gpui::Div> {
    let (bg, hover_bg, fg, _border) = tone.colors();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(theme::c(bg))
        .text_color(theme::c(fg))
        .text_sm()
        .font_weight(if tone.is_emphasis() {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        })
        .hover(|s| s.bg(theme::c(hover_bg)))
}

pub fn icon_button(
    id: impl Into<ElementId>,
    label: &'static str,
    name: IconName,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let tone = if primary {
        ButtonTone::Primary
    } else {
        ButtonTone::Neutral
    };
    let (_, _, fg, _) = tone.colors();
    button_base(id, label, tone).child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(icon(name, fg, 14.))
            .child(label),
    )
}

pub fn panel() -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(theme::c(theme::BORDER))
        .bg(theme::c(theme::SURFACE))
}

#[derive(Clone, Copy)]
enum StatusTone {
    Info,
    Success,
    Warning,
    Error,
}

impl StatusTone {
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

    fn colors(self) -> (u32, u32, u32, IconName) {
        match self {
            Self::Info => (
                theme::ACCENT_SOFT,
                theme::ACCENT,
                theme::TEXT,
                IconName::Proxy,
            ),
            Self::Success => (
                theme::GREEN_SOFT,
                theme::GREEN,
                theme::TEXT,
                IconName::Check,
            ),
            Self::Warning => (
                theme::YELLOW_SOFT,
                theme::YELLOW,
                theme::TEXT,
                IconName::Settings,
            ),
            Self::Error => (theme::RED_SOFT, theme::RED, theme::TEXT, IconName::Wrench),
        }
    }
}

/// Descriptor for the shared confirm dialog. `danger` selects destructive
/// (red) styling per theme.rs; otherwise the neutral accent is used.
#[derive(Clone)]
pub struct ConfirmModal {
    pub title: SharedString,
    pub message: SharedString,
    pub danger: bool,
}

impl ConfirmModal {
    /// A destructive-delete confirm dialog descriptor.
    pub fn delete(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            danger: true,
        }
    }
}

/// Render the shared confirm dialog as a full-view modal overlay. `confirm` and
/// `cancel` are the footer buttons — the caller wires their `.on_click` with
/// `cx.listener`, so this stays view-agnostic. Host it inside a `relative()`
/// container sized to the view (or the app root) so the backdrop covers it.
pub fn confirm_overlay(
    modal: &ConfirmModal,
    confirm: impl IntoElement,
    cancel: impl IntoElement,
) -> impl IntoElement {
    let (accent, accent_soft) = if modal.danger {
        (theme::RED, theme::RED_SOFT)
    } else {
        (theme::ACCENT, theme::ACCENT_SOFT)
    };
    div()
        .id("confirm-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .bg(theme::translucent(0x000000, 0.38))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_4()
                .w(px(400.))
                .p_5()
                .rounded_lg()
                .bg(theme::c(theme::SURFACE))
                .border_1()
                .border_color(theme::c(theme::BORDER))
                .shadow(theme::shadow_popover())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .flex_shrink_0()
                                .w(px(24.))
                                .h(px(24.))
                                .rounded_full()
                                .bg(theme::c(accent_soft))
                                .text_color(theme::c(accent))
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .child("!"),
                        )
                        .child(
                            div()
                                .text_color(theme::c(theme::TEXT))
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(modal.title.clone()),
                        ),
                )
                .child(
                    div()
                        .text_color(theme::c(theme::SUBTEXT))
                        .text_sm()
                        .line_height(px(20.))
                        .child(modal.message.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(cancel)
                        .child(confirm),
                ),
        )
}

pub fn status_banner(message: impl Into<SharedString>) -> impl IntoElement {
    let message = message.into();
    let tone = StatusTone::from_text(&message.to_string());
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
        .border_color(theme::translucent(accent, 0.32))
        .bg(theme::translucent(bg, 0.8))
        .child(
            div()
                .mt_0p5()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::c(accent))
                .child(icon(icon_name, accent, 14.)),
        )
        .child(
            div()
                .text_color(theme::c(fg))
                .text_sm()
                .line_height(gpui::px(18.))
                .child(message),
        )
}
