//! App-level notification center and toast host.
//!
//! This is intentionally UI-owned: core services return success/warnings/errors,
//! while the shell decides whether a result should be silent, inline, or global.

use std::collections::VecDeque;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, FontWeight, SharedString, Window};

use crate::icons::{icon, IconName};
use crate::theme;

const MAX_VISIBLE: usize = 4;
const MAX_HISTORY: usize = 64;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TIMEOUT: Duration = Duration::from_secs(9);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationLevel {
    fn colors(self) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba, IconName) {
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
}

impl NotificationHost {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            visible: VecDeque::new(),
            history: VecDeque::new(),
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

    pub fn notify(&mut self, request: NotificationRequest, cx: &mut Context<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let timeout = request
            .timeout
            .unwrap_or_else(|| request.level.default_timeout());
        let notification = Notification {
            id,
            level: request.level,
            title: request.title,
            message: request.message,
            source: request.source,
        };

        self.visible.push_front(notification.clone());
        self.history.push_front(notification);
        while self.visible.len() > MAX_VISIBLE {
            self.visible.pop_back();
        }
        while self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }

        if !request.persistent {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(timeout).await;
                this.update(cx, |this, cx| {
                    this.dismiss(id);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        id
    }

    pub fn dismiss(&mut self, id: u64) {
        self.visible.retain(|item| item.id != id);
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.visible.clear();
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

        div()
            .id(element_id)
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .w(px(380.))
            .px_3()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(accent.alpha(0.32))
            .bg(bg.alpha(0.96))
            .shadow(theme::shadow_popover())
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
            )
    }
}

impl Render for NotificationHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications = self.visible.iter().cloned().collect::<Vec<_>>();

        div()
            .id("notification-host")
            .absolute()
            .top(px(56.))
            .right_4()
            .flex()
            .flex_col()
            .gap_2()
            .children(
                notifications
                    .into_iter()
                    .map(|notification| self.render_notification(notification, cx)),
            )
    }
}
