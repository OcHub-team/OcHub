//! Shared GPUI building blocks for the desktop shell.

use gpui::{div, prelude::*, ElementId, FontWeight, SharedString};

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
