//! Sessions panel. Lists recent CLI sessions discovered on disk via
//! `session_manager::scan_sessions()` and supports deleting one. Both calls are
//! synchronous filesystem operations.

use std::sync::Arc;

use gpui::{div, prelude::*, px, Context, FontWeight, SharedString, Window};
use ochub_core::session_manager::{self, SessionMessage, SessionMeta};
use ochub_core::AppState;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::theme;

/// Sessions per page in the list (avoids rendering hundreds of rows at once).
const PAGE_SIZE: usize = 20;

/// An opened session: its metadata plus the loaded conversation transcript.
struct SessionDetail {
    meta: SessionMeta,
    messages: Vec<SessionMessage>,
    error: Option<SharedString>,
}

pub struct SessionsView {
    #[allow(dead_code)]
    app: Arc<AppState>,
    sessions: Vec<SessionMeta>,
    status: Option<SharedString>,
    /// Zero-based current page into `sessions`.
    page: usize,
    /// When `Some`, the transcript viewer replaces the list.
    detail: Option<SessionDetail>,
    /// Session index pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<usize>,
}

impl SessionsView {
    pub fn new(app: Arc<AppState>, _cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            app,
            sessions: Vec::new(),
            status: None,
            page: 0,
            detail: None,
            confirm_delete: None,
        };
        this.reload();
        this
    }

    pub fn reload(&mut self) {
        self.sessions = session_manager::scan_sessions();
        // Returning to the list (refresh / re-entering the section) closes any
        // open transcript.
        self.detail = None;
        // Keep the current page in range after the list size changes.
        let max_page = self.total_pages().saturating_sub(1);
        if self.page > max_page {
            self.page = max_page;
        }
    }

    fn total_pages(&self) -> usize {
        self.sessions.len().div_ceil(PAGE_SIZE).max(1)
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let max_page = self.total_pages().saturating_sub(1);
        let page = page.min(max_page);
        if page != self.page {
            self.page = page;
            cx.notify();
        }
    }

    fn title_for(session: &SessionMeta) -> String {
        session
            .title
            .clone()
            .or_else(|| session.summary.clone())
            .unwrap_or_else(|| session.session_id.clone())
    }

    fn do_delete(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(idx) else {
            return;
        };
        let source_path = session.source_path.clone().unwrap_or_default();
        match session_manager::delete_session(
            &session.provider_id,
            &session.session_id,
            &source_path,
        ) {
            Ok(true) => self.status = Some(SharedString::from("会话已删除")),
            Ok(false) => self.status = Some(SharedString::from("未找到会话")),
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    /// Load a session's full transcript and switch to the detail viewer.
    fn open_detail(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(idx).cloned() else {
            return;
        };
        let source_path = session.source_path.clone().unwrap_or_default();
        let detail = match session_manager::load_messages(&session.provider_id, &source_path) {
            Ok(messages) => SessionDetail {
                meta: session,
                messages,
                error: None,
            },
            Err(err) => SessionDetail {
                meta: session,
                messages: Vec::new(),
                error: Some(SharedString::from(format!("加载对话失败: {err}"))),
            },
        };
        self.detail = Some(detail);
        cx.notify();
    }

    fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        cx.notify();
    }

    /// Per-role accent color + soft background for a transcript bubble.
    fn role_colors(role: &str) -> (gpui::Rgba, gpui::Rgba) {
        match role {
            "user" => (theme::accent(), theme::accent_soft()),
            "assistant" => (theme::green(), theme::green_soft()),
            "system" => (theme::muted(), theme::inset()),
            _ => (theme::mauve(), theme::surface_hover()),
        }
    }

    fn role_label(role: &str) -> SharedString {
        match role {
            "user" => SharedString::from("用户"),
            "assistant" => SharedString::from("助手"),
            "system" => SharedString::from("系统"),
            "tool" => SharedString::from("工具"),
            other => SharedString::from(other.to_string()),
        }
    }

    fn render_message(message: &SessionMessage) -> impl IntoElement {
        let (accent, soft) = Self::role_colors(&message.role);
        let label = Self::role_label(&message.role);
        let content = if message.content.trim().is_empty() {
            SharedString::from("（空消息）")
        } else {
            SharedString::from(message.content.clone())
        };
        components::card()
            .flex_shrink_0()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(components::status_dot_sized(accent, 6.))
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(soft)
                            .text_color(accent)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .text_color(theme::text())
                    .text_sm()
                    .line_height(px(20.))
                    .child(content),
            )
    }

    fn render_detail(&self, detail: &SessionDetail, cx: &mut Context<Self>) -> impl IntoElement {
        let title = Self::title_for(&detail.meta);
        let provider = detail.meta.provider_id.clone();
        let project = detail.meta.project_dir.clone();
        let count = detail.messages.len();
        let messages: Vec<_> = detail.messages.iter().map(Self::render_message).collect();
        let error = detail.error.clone();
        let subtitle = SharedString::from(match project {
            Some(p) => format!("{count} 条消息 · {p}"),
            None => format!("{count} 条消息"),
        });

        layout::page()
            .child(
                layout::page_header(title, Some(subtitle)).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(components::badge(BadgeTone::Teal, provider))
                        .child(
                            components::button(
                                "session-back",
                                "← 返回",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.close_detail(cx)),
                            ),
                        ),
                ),
            )
            .child(
                div()
                    .id("session-transcript")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .p_6()
                    .w_full()
                    .overflow_y_scroll()
                    .when_some(error, |s, err| {
                        s.child(div().text_color(theme::red()).text_sm().child(err))
                    })
                    .when(messages.is_empty() && detail.error.is_none(), |s| {
                        s.child(components::empty_state(
                            IconName::Message,
                            "没有可显示的消息",
                            "这条会话没有可显示的消息。",
                            None,
                        ))
                    })
                    .children(messages),
            )
    }

    fn render_card(
        &self,
        idx: usize,
        session: &SessionMeta,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = Self::title_for(session);
        let provider = session.provider_id.clone();
        let project = session.project_dir.clone();

        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .id(SharedString::from(format!("session-open-{idx}")))
                    .role(gpui::Role::Button)
                    .aria_label("查看完整对话")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_detail(idx, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(components::badge(BadgeTone::Teal, provider))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(title)),
                            ),
                    )
                    .when_some(project, |s, p| {
                        s.child(
                            div()
                                .min_w_0()
                                .text_color(theme::muted())
                                .text_xs()
                                .truncate()
                                .child(SharedString::from(p)),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            SharedString::from(format!("session-view-{idx}")),
                            "查看",
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_detail(idx, cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("session-delete-{idx}")),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(idx);
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_pagination(&self, cx: &mut Context<Self>) -> gpui::Div {
        let total_pages = self.total_pages();
        let page = self.page;
        let prev = components::button(
            "sessions-prev",
            "上一页",
            ButtonTone::Neutral,
            ButtonSize::Sm,
        );
        let prev = if page > 0 {
            let target = page.saturating_sub(1);
            prev.on_click(cx.listener(move |this, _event, _window, cx| this.set_page(target, cx)))
        } else {
            prev.text_color(theme::muted())
        };
        let next = components::button(
            "sessions-next",
            "下一页",
            ButtonTone::Neutral,
            ButtonSize::Sm,
        );
        let next = if page + 1 < total_pages {
            let target = page + 1;
            next.on_click(cx.listener(move |this, _event, _window, cx| this.set_page(target, cx)))
        } else {
            next.text_color(theme::muted())
        };
        components::pagination(
            prev.into_any_element(),
            format!("第 {} / {} 页", page + 1, total_pages),
            next.into_any_element(),
        )
    }
}

impl Render for SessionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(detail) = self.detail.as_ref() {
            return self.render_detail(detail, cx).into_any_element();
        }
        let total = self.sessions.len();
        let start = self.page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(total);
        let cards: Vec<_> = self.sessions[start..end]
            .iter()
            .enumerate()
            .map(|(i, s)| self.render_card(start + i, s, cx))
            .collect();
        let is_empty = total == 0;
        let show_pagination = total > PAGE_SIZE;
        let confirm = self.confirm_delete.and_then(|idx| {
            self.sessions
                .get(idx)
                .map(Self::title_for)
                .map(|t| (idx, t))
        });

        layout::page()
            .relative()
            .child(
                layout::page_header("会话", None).child(
                    components::button(
                        "sessions-refresh",
                        "刷新",
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.reload();
                        cx.notify();
                    })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "session-list",
                layout::content_column()
                    .when(is_empty, |s| {
                        s.child(components::empty_state(
                            IconName::Clock,
                            "没有找到会话",
                            "扫描到的 CLI 会话会显示在这里。",
                            None,
                        ))
                    })
                    .children(cards),
            ))
            .when(show_pagination, |s| s.child(self.render_pagination(cx)))
            .when_some(confirm, |root, (idx, title)| {
                let message =
                    SharedString::from(format!("确定删除会话「{title}」吗？此操作不可撤销。"));
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除会话"))
                        .child(
                            components::modal_body()
                                .child(div().text_color(theme::subtext()).text_sm().child(message)),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "session-confirm-delete-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "session-confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(idx, cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }
}
