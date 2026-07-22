//! Sessions panel. Lists recent CLI sessions discovered on disk via
//! `session_manager::scan_sessions()` and supports deleting one. Both calls are
//! synchronous filesystem operations.

use std::sync::Arc;

use gpui::{div, prelude::*, px, Context, FontWeight, SharedString, Window};
use ochub_core::session_manager::{self, SessionMessage, SessionMeta};
use ochub_core::AppState;

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
}

impl SessionsView {
    pub fn new(app: Arc<AppState>, _cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            app,
            sessions: Vec::new(),
            status: None,
            page: 0,
            detail: None,
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
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(accent),
                    )
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

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(theme::bg())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(theme::border())
                    .bg(theme::header())
                    .child(
                        div()
                            .id("session-back")
                            .role(gpui::Role::Button)
                            .aria_label("返回会话列表")
                            .flex_shrink_0()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_color(theme::subtext())
                            .text_sm()
                            .hover(|s| s.bg(theme::surface_hover()))
                            .child("← 返回")
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.close_detail(cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .px_2()
                                            .rounded_md()
                                            .flex_shrink_0()
                                            .bg(theme::surface_hover())
                                            .text_color(theme::teal())
                                            .text_xs()
                                            .child(SharedString::from(provider)),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .text_color(theme::text())
                                            .text_lg()
                                            .font_weight(FontWeight::BOLD)
                                            .truncate()
                                            .child(SharedString::from(title)),
                                    ),
                            )
                            .child(div().text_color(theme::muted()).text_xs().truncate().child(
                                SharedString::from(match project {
                                    Some(p) => format!("{count} 条消息 · {p}"),
                                    None => format!("{count} 条消息"),
                                }),
                            )),
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
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .child("这条会话没有可显示的消息。"),
                        )
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

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
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
                            .child(
                                div()
                                    .px_2()
                                    .rounded_md()
                                    .flex_shrink_0()
                                    .bg(theme::surface_hover())
                                    .text_color(theme::teal())
                                    .text_xs()
                                    .child(SharedString::from(provider)),
                            )
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
                        div()
                            .id(SharedString::from(format!("session-view-{idx}")))
                            .role(gpui::Role::Button)
                            .aria_label("查看完整对话")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::accent_soft())
                            .text_color(theme::accent())
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .hover(|s| s.bg(theme::surface_hover()))
                            .child("查看")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.open_detail(idx, cx);
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("session-delete-{idx}")))
                            .role(gpui::Role::Button)
                            .aria_label("删除会话")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::surface_hover())
                            .text_color(theme::red())
                            .text_sm()
                            .hover(|s| s.bg(theme::red_soft()))
                            .child("删除")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.do_delete(idx, cx);
                            })),
                    ),
            )
    }

    fn page_button(
        cx: &mut Context<Self>,
        id: &'static str,
        label: &'static str,
        enabled: bool,
        target: usize,
    ) -> gpui::Stateful<gpui::Div> {
        let base = div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(label)
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme::border())
            .text_sm()
            .child(label);
        if enabled {
            base.cursor_pointer()
                .bg(theme::surface())
                .text_color(theme::text())
                .hover(|s| s.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _event, _window, cx| this.set_page(target, cx)))
        } else {
            base.bg(theme::inset()).text_color(theme::muted())
        }
    }

    fn render_pagination(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.sessions.len();
        let total_pages = self.total_pages();
        let page = self.page;
        let start = page * PAGE_SIZE + 1;
        let end = ((page + 1) * PAGE_SIZE).min(total);
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_6()
            .py_3()
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::header())
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(SharedString::from(format!(
                        "第 {start}–{end} 条 · 共 {total} 条会话"
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(Self::page_button(
                        cx,
                        "sessions-prev",
                        "上一页",
                        page > 0,
                        page.saturating_sub(1),
                    ))
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_xs()
                            .min_w(gpui::px(60.))
                            .child(SharedString::from(format!(
                                "第 {} / {} 页",
                                page + 1,
                                total_pages
                            ))),
                    )
                    .child(Self::page_button(
                        cx,
                        "sessions-next",
                        "下一页",
                        page + 1 < total_pages,
                        page + 1,
                    )),
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

        layout::page()
            .child(
                layout::page_header("会话", None).child(
                    div()
                        .id("sessions-refresh")
                        .role(gpui::Role::Button)
                        .aria_label("刷新会话")
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme::surface())
                        .text_color(theme::subtext())
                        .text_sm()
                        .child("刷新")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.reload();
                            cx.notify();
                        })),
                ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::teal())
                        .text_xs()
                        .child(status),
                )
            })
            .child(layout::scroll_body(
                "session-list",
                layout::content_column()
                    .when(is_empty, |s| {
                        s.child(div().text_color(theme::muted()).child("没有找到会话。"))
                    })
                    .children(cards),
            ))
            .when(show_pagination, |s| s.child(self.render_pagination(cx)))
            .into_any_element()
    }
}
