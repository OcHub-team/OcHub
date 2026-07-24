//! App-level notification center and toast host.
//!
//! This is intentionally UI-owned: core services return success/warnings/errors,
//! while the shell decides whether a result should be silent, inline, or global.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context,
    FontWeight, IntoElement, RenderOnce, Rgba, SharedString, Window,
};

use crate::icons::{icon, IconName};
use crate::theme;

const MAX_VISIBLE: usize = 3;
const MAX_HISTORY: usize = 64;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const ERROR_TIMEOUT: Duration = Duration::from_secs(5);
const STACK_LAYER_OFFSET: f32 = 6.;
const STACK_LAYER_INSET: f32 = 5.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationLevel {
    pub fn from_text(text: &str) -> Self {
        if text.contains("失败")
            || text.contains("错误")
            || text.contains("无效")
            || text.contains("不能为空")
            || text.contains("不可用")
        {
            Self::Error
        } else if text.contains("警告")
            || text.contains("跳过")
            || text.contains("冲突")
            || text.contains("建议")
            || text.contains("尚未")
            || text.contains("不存在")
        {
            Self::Warning
        } else if text.contains("成功") || text.contains("已") || text.contains("完成") {
            Self::Success
        } else {
            Self::Info
        }
    }

    fn colors(self) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba, IconName) {
        match self {
            Self::Info => (
                theme::accent_soft(),
                theme::accent(),
                theme::text(),
                IconName::Message,
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
                IconName::Diamond,
            ),
            Self::Error => (
                theme::red_soft(),
                theme::red(),
                theme::text(),
                IconName::Close,
            ),
        }
    }

    fn default_timeout(self) -> Duration {
        match self {
            Self::Error => ERROR_TIMEOUT,
            _ => DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub struct Notification {
    id: u64,
    level: NotificationLevel,
    title: SharedString,
    message: Option<SharedString>,
    source: Option<SharedString>,
    auto_dismiss_after: Option<Duration>,
    remaining: Option<Duration>,
    countdown_started_at: Option<Instant>,
    timer_epoch: u64,
    hovered: bool,
}

pub struct NotificationRequest {
    level: NotificationLevel,
    title: SharedString,
    message: Option<SharedString>,
    source: Option<SharedString>,
    persistent: bool,
    timeout: Option<Duration>,
}

impl NotificationRequest {
    pub fn new(level: NotificationLevel, title: impl Into<SharedString>) -> Self {
        Self {
            level,
            title: title.into(),
            message: None,
            source: None,
            persistent: false,
            timeout: None,
        }
    }

    pub fn message(mut self, message: impl Into<SharedString>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[allow(dead_code)]
    pub fn source(mut self, source: impl Into<SharedString>) -> Self {
        self.source = Some(source.into());
        self
    }

    #[allow(dead_code)]
    pub fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    #[allow(dead_code)]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

pub struct NotificationHost {
    next_id: u64,
    visible: VecDeque<Notification>,
    history: VecDeque<Notification>,
    stack_expanded: bool,
}

/// A fixed-size canvas keeps the countdown animation out of layout. The wider,
/// translucent layer is the glow; the one-pixel layer is the crisp progress line.
#[derive(IntoElement)]
struct ToastProgress {
    accent: Rgba,
    duration: Duration,
    remaining: Duration,
    started_at: Option<Instant>,
}

impl ToastProgress {
    fn new(
        accent: Rgba,
        duration: Duration,
        remaining: Duration,
        started_at: Option<Instant>,
    ) -> Self {
        Self {
            accent,
            duration,
            remaining,
            started_at,
        }
    }
}

impl RenderOnce for ToastProgress {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let accent = self.accent;
        let duration = self.duration;
        let remaining = self.remaining;
        let started_at = self.started_at;

        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _prepaint, window, cx| {
                let x = f32::from(bounds.origin.x);
                let y = f32::from(bounds.origin.y);
                let width = f32::from(bounds.size.width).max(0.);
                let progress = if cx.reduce_motion() {
                    1.
                } else {
                    remaining_duration_fraction(duration, remaining, started_at)
                };

                // A faint full-width rail remains visible when reduced motion is enabled.
                window.paint_quad(fill(
                    Bounds::new(point(px(x), px(y + 1.)), size(px(width), px(1.))),
                    accent.alpha(0.13),
                ));

                let active_width = width * progress;
                if active_width <= 0. {
                    return;
                }

                window.paint_quad(
                    fill(
                        Bounds::new(point(px(x), px(y)), size(px(active_width), px(3.))),
                        accent.alpha(0.22),
                    )
                    .corner_radii(px(1.5)),
                );
                window.paint_quad(
                    fill(
                        Bounds::new(point(px(x), px(y + 1.)), size(px(active_width), px(1.))),
                        accent.alpha(0.96),
                    )
                    .corner_radii(px(0.5)),
                );

                if progress > 0. && started_at.is_some() && !cx.reduce_motion() {
                    window.request_animation_frame();
                }
            },
        )
        .w_full()
        .h_full()
    }
}

fn remaining_fraction(duration: Duration, elapsed: Duration) -> f32 {
    if duration.is_zero() {
        return 0.;
    }

    (duration.saturating_sub(elapsed).as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
}

fn remaining_duration_fraction(
    duration: Duration,
    remaining: Duration,
    started_at: Option<Instant>,
) -> f32 {
    let elapsed = started_at.map_or(Duration::ZERO, |started_at| started_at.elapsed());
    remaining_fraction(
        duration,
        duration.saturating_sub(remaining.saturating_sub(elapsed)),
    )
}

fn auto_dismiss_timeout(
    level: NotificationLevel,
    persistent: bool,
    timeout: Option<Duration>,
) -> Option<Duration> {
    (!persistent).then(|| timeout.unwrap_or_else(|| level.default_timeout()))
}

impl NotificationHost {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            visible: VecDeque::new(),
            history: VecDeque::new(),
            stack_expanded: false,
        }
    }

    pub fn info(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) -> u64 {
        self.notify(NotificationRequest::new(NotificationLevel::Info, title), cx)
    }

    pub fn success(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Success, title),
            cx,
        )
    }

    pub fn warning(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Warning, title).message(message),
            cx,
        )
    }

    pub fn error(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Error, title)
                .message(message)
                .persistent(false),
            cx,
        )
    }

    /// Status toast with an explicit level. `None` falls back to keyword
    /// inference — blocking/refusal messages should always pass an explicit
    /// level so their wording never gets mis-classified.
    pub fn status_leveled(
        &mut self,
        level: Option<NotificationLevel>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let message = message.into();
        let level = level.unwrap_or_else(|| NotificationLevel::from_text(message.as_ref()));
        self.notify(NotificationRequest::new(level, message), cx)
    }

    pub fn notify(&mut self, request: NotificationRequest, cx: &mut Context<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let timeout = auto_dismiss_timeout(request.level, request.persistent, request.timeout);
        let countdown_started_at = timeout.map(|_| Instant::now());
        let notification = Notification {
            id,
            level: request.level,
            title: request.title,
            message: request.message,
            source: request.source,
            auto_dismiss_after: timeout,
            remaining: timeout,
            countdown_started_at,
            timer_epoch: 0,
            hovered: false,
        };

        self.visible.push_front(notification.clone());
        self.history.push_front(notification);
        while self.visible.len() > MAX_VISIBLE {
            self.visible.pop_back();
        }
        while self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }

        if let Some(timeout) = timeout {
            Self::schedule_dismiss(id, timeout, 0, cx);
        }

        id
    }

    fn schedule_dismiss(id: u64, timeout: Duration, timer_epoch: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            this.update(cx, |this, cx| {
                let should_dismiss = this.visible.iter().any(|notification| {
                    notification.id == id
                        && notification.timer_epoch == timer_epoch
                        && !notification.hovered
                });
                if should_dismiss {
                    this.dismiss(id);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn set_hovered(&mut self, id: u64, hovered: bool, cx: &mut Context<Self>) {
        let mut resume = None;
        let mut dismiss_now = false;
        if let Some(notification) = self.visible.iter_mut().find(|item| item.id == id) {
            if notification.hovered == hovered {
                return;
            }

            notification.hovered = hovered;
            notification.timer_epoch = notification.timer_epoch.wrapping_add(1);
            if hovered {
                if let (Some(remaining), Some(started_at)) = (
                    notification.remaining.as_mut(),
                    notification.countdown_started_at.take(),
                ) {
                    *remaining = remaining.saturating_sub(started_at.elapsed());
                }
            } else if let Some(remaining) = notification.remaining {
                if remaining.is_zero() {
                    dismiss_now = true;
                } else {
                    notification.countdown_started_at = Some(Instant::now());
                    resume = Some((remaining, notification.timer_epoch));
                }
            }
        }

        if dismiss_now {
            self.dismiss(id);
        } else if let Some((remaining, timer_epoch)) = resume {
            Self::schedule_dismiss(id, remaining, timer_epoch, cx);
        }
        cx.notify();
    }

    pub fn dismiss(&mut self, id: u64) {
        self.visible.retain(|item| item.id != id);
        if self.visible.len() <= 1 {
            self.stack_expanded = false;
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.visible.clear();
        self.stack_expanded = false;
    }

    #[allow(dead_code)]
    pub fn history(&self) -> impl Iterator<Item = &Notification> {
        self.history.iter()
    }

    fn render_notification(
        &self,
        notification: Notification,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = notification.id;
        let (bg, accent, fg, icon_name) = notification.level.colors();
        let element_id = SharedString::from(format!("notification-{id}"));
        let auto_dismiss_after = notification.auto_dismiss_after;
        let remaining = notification.remaining;
        let countdown_started_at = notification.countdown_started_at;
        let copy_text = [
            Some(notification.title.as_ref()),
            notification.message.as_deref(),
            notification.source.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");

        div()
            .id(element_id)
            .relative()
            .flex()
            .flex_col()
            .w(px(380.))
            .rounded_lg()
            .overflow_hidden()
            .border_1()
            .border_color(accent.alpha(0.32))
            .bg(bg.alpha(0.96))
            .shadow(theme::shadow_popover())
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                this.set_hovered(id, *hovered, cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_3()
                    .px_3()
                    .py_3()
                    .child(
                        div()
                            .mt_0p5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(24.))
                            .h(px(24.))
                            .rounded_md()
                            .bg(accent.alpha(0.12))
                            .child(icon(icon_name, accent, 15.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_color(fg)
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .line_height(px(18.))
                                    .child(notification.title),
                            )
                            .when_some(notification.message, |s, message| {
                                s.child(
                                    div()
                                        .text_color(theme::subtext())
                                        .text_xs()
                                        .line_height(px(17.))
                                        .child(message),
                                )
                            })
                            .when_some(notification.source, |s, source| {
                                s.child(
                                    div()
                                        .text_color(theme::muted())
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(source),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(("notification-copy", id))
                                    .role(gpui::Role::Button)
                                    .aria_label("复制通知内容")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(22.))
                                    .h(px(22.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|s| s.bg(accent.alpha(0.12)).text_color(theme::text()))
                                    .child(icon(IconName::Copy, theme::muted(), 13.))
                                    .on_click(move |_event, _window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    }),
                            )
                            .child(
                                div()
                                    .id(("notification-close", id))
                                    .role(gpui::Role::Button)
                                    .aria_label("关闭通知")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(22.))
                                    .h(px(22.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|s| s.bg(accent.alpha(0.12)).text_color(theme::text()))
                                    .child(icon(IconName::Close, theme::muted(), 13.))
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.dismiss(id);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(auto_dismiss_after, |toast, timeout| {
                toast.child(
                    div()
                        .absolute()
                        .bottom(px(0.))
                        .left(px(0.))
                        .right(px(0.))
                        .h(px(3.))
                        .child(ToastProgress::new(
                            accent,
                            timeout,
                            remaining.unwrap_or(timeout),
                            countdown_started_at,
                        )),
                )
            })
    }

    fn render_stack_backplate(
        &self,
        notification: &Notification,
        layer: usize,
        layer_count: usize,
    ) -> impl IntoElement {
        let (bg, accent, _, _) = notification.level.colors();
        let inset = (layer_count - layer) as f32 * STACK_LAYER_INSET;
        let top = layer as f32 * STACK_LAYER_OFFSET;

        div()
            .absolute()
            .top(px(top))
            .left(px(inset))
            .right(px(inset))
            .h(px(24.))
            .rounded_lg()
            .border_1()
            .border_color(accent.alpha(0.24))
            .bg(bg.alpha(0.98))
    }

    fn render_collapsed_stack(
        &self,
        notifications: Vec<Notification>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layer_count = notifications.len().saturating_sub(1);
        let mut stack = div()
            .id("notification-stack-collapsed")
            .relative()
            .w(px(380.))
            .pt(px(layer_count as f32 * STACK_LAYER_OFFSET));

        // Paint oldest to newest so each successive layer naturally covers the one behind it.
        for (layer, notification) in notifications.iter().skip(1).rev().enumerate() {
            stack = stack.child(self.render_stack_backplate(notification, layer, layer_count));
        }

        stack.child(self.render_notification(notifications[0].clone(), cx))
    }
}

/// A child view with legacy status state that is forwarded into the single
/// app-level toast host. Taking the value prevents an inline banner and avoids
/// emitting the same state again on unrelated redraws.
pub trait ToastSource {
    fn take_toast(&mut self) -> Option<SharedString>;
    /// Explicit level for the toast just taken. Default `None` keeps keyword
    /// inference; views with blocking/refusal toasts should set it explicitly.
    fn take_toast_level(&mut self) -> Option<NotificationLevel> {
        None
    }
}

macro_rules! impl_status_toasts {
    ($view:ty) => {
        impl $crate::notifications::ToastSource for $view {
            fn take_toast(&mut self) -> Option<gpui::SharedString> {
                self.status.take()
            }
        }
    };
}

/// Like [`impl_status_toasts!`] but also forwards an explicit
/// `self.status_level` set alongside `self.status`.
macro_rules! impl_status_toasts_leveled {
    ($view:ty) => {
        impl $crate::notifications::ToastSource for $view {
            fn take_toast(&mut self) -> Option<gpui::SharedString> {
                self.status.take()
            }
            fn take_toast_level(&mut self) -> Option<$crate::notifications::NotificationLevel> {
                self.status_level.take()
            }
        }
    };
}

pub(crate) use impl_status_toasts;
pub(crate) use impl_status_toasts_leveled;

impl Render for NotificationHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications = self.visible.iter().cloned().collect::<Vec<_>>();
        let is_stack = notifications.len() > 1;
        let is_expanded = is_stack && self.stack_expanded;

        let host = div()
            .id("notification-host")
            .absolute()
            .top(px(56.))
            .right_4()
            .flex()
            .flex_col()
            .gap_2()
            .when(is_stack, |host| {
                host.on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                    if this.stack_expanded != *hovered {
                        this.stack_expanded = *hovered;
                        cx.notify();
                    }
                }))
            });

        if is_expanded || !is_stack {
            host.children(
                notifications
                    .into_iter()
                    .map(|notification| self.render_notification(notification, cx)),
            )
        } else {
            host.child(self.render_collapsed_stack(notifications, cx))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        auto_dismiss_timeout, remaining_fraction, NotificationLevel, DEFAULT_TIMEOUT,
        ERROR_TIMEOUT, MAX_VISIBLE,
    };

    #[test]
    fn infers_toast_level_from_legacy_status_copy() {
        assert_eq!(
            NotificationLevel::from_text("保存失败: permission denied"),
            NotificationLevel::Error
        );
        assert_eq!(
            NotificationLevel::from_text("配置尚未创建"),
            NotificationLevel::Warning
        );
        assert_eq!(
            NotificationLevel::from_text("已应用 OcHub"),
            NotificationLevel::Success
        );
        assert_eq!(
            NotificationLevel::from_text("正在检查更新..."),
            NotificationLevel::Info
        );
    }

    #[test]
    fn resolves_auto_dismiss_duration_for_progress_bar() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(3));
        assert_eq!(ERROR_TIMEOUT, Duration::from_secs(5));
        assert_eq!(MAX_VISIBLE, 3);
        assert_eq!(
            auto_dismiss_timeout(NotificationLevel::Info, false, None),
            Some(DEFAULT_TIMEOUT)
        );
        assert_eq!(
            auto_dismiss_timeout(NotificationLevel::Error, false, None),
            Some(ERROR_TIMEOUT)
        );
        assert_eq!(
            auto_dismiss_timeout(
                NotificationLevel::Success,
                false,
                Some(Duration::from_millis(750))
            ),
            Some(Duration::from_millis(750))
        );
        assert_eq!(
            auto_dismiss_timeout(
                NotificationLevel::Warning,
                true,
                Some(Duration::from_secs(1))
            ),
            None
        );
    }

    #[test]
    fn calculates_progress_from_absolute_elapsed_time() {
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::ZERO),
            1.
        );
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::from_millis(1500)),
            0.5
        );
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::from_secs(4)),
            0.
        );
        assert_eq!(remaining_fraction(Duration::ZERO, Duration::ZERO), 0.);
    }
}
