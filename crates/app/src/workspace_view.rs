//! OpenClaw workspace editor. Mirrors cc-switch's Workspace page using the
//! already-ported `WorkspaceService` file and daily-memory helpers.

use std::process::Command;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::services::{DailyMemoryFileInfo, DailyMemorySearchResult, WorkspaceService};

use crate::components::{self, ButtonTone, ConfirmModal};
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone)]
struct WorkspaceFileRow {
    filename: &'static str,
    description: &'static str,
    exists: bool,
}

pub struct WorkspaceView {
    files: Vec<WorkspaceFileRow>,
    memory_files: Vec<DailyMemoryFileInfo>,
    memory_results: Vec<DailyMemorySearchResult>,
    workspace_file: Entity<TextInput>,
    workspace_content: Entity<TextInput>,
    memory_file: Entity<TextInput>,
    memory_content: Entity<TextInput>,
    memory_query: Entity<TextInput>,
    status: Option<SharedString>,
    /// Daily-memory file awaiting delete confirmation.
    pending_delete: Option<String>,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let workspace_file = cx.new(|cx| {
            let mut input = TextInput::new(cx, "AGENTS.md");
            input.set_content("AGENTS.md", cx);
            input
        });
        let workspace_content = cx.new(|cx| TextInput::new(cx, "工作区文件内容").multiline(true));
        let memory_file = cx.new(|cx| TextInput::new(cx, "YYYY-MM-DD.md"));
        let memory_content = cx.new(|cx| TextInput::new(cx, "每日记忆内容").multiline(true));
        let memory_query = cx.new(|cx| TextInput::new(cx, "搜索每日记忆"));

        let mut this = Self {
            files: Vec::new(),
            memory_files: Vec::new(),
            memory_results: Vec::new(),
            workspace_file,
            workspace_content,
            memory_file,
            memory_content,
            memory_query,
            status: None,
            pending_delete: None,
        };
        this.reload();
        this
    }

    pub fn reload(&mut self) {
        self.files = WorkspaceService::allowed_workspace_files()
            .iter()
            .map(|filename| WorkspaceFileRow {
                filename,
                description: workspace_file_description(filename),
                exists: WorkspaceService::read_workspace_file(filename)
                    .map(|content| content.is_some())
                    .unwrap_or(false),
            })
            .collect();
        self.memory_files = WorkspaceService::list_daily_memory_files().unwrap_or_default();
    }

    fn set_status(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(msg.into());
        cx.notify();
    }

    fn open_dir(&mut self, subdir: &'static str, cx: &mut Context<Self>) {
        match WorkspaceService::ensure_directory_for_subdir(subdir).and_then(|path| {
            open_path(&path).map_err(AppErrorString::into_app_error)?;
            Ok(path)
        }) {
            Ok(path) => self.set_status(format!("已打开 {}", path.display()), cx),
            Err(err) => self.set_status(format!("打开目录失败: {err}"), cx),
        }
    }

    fn load_workspace_file_named(&mut self, filename: String, cx: &mut Context<Self>) {
        match WorkspaceService::read_workspace_file(&filename) {
            Ok(Some(content)) => {
                self.workspace_file
                    .update(cx, |input, cx| input.set_content(filename.clone(), cx));
                self.workspace_content
                    .update(cx, |input, cx| input.set_content(content, cx));
                self.set_status(format!("已读取 {filename}"), cx);
            }
            Ok(None) => {
                self.workspace_file
                    .update(cx, |input, cx| input.set_content(filename.clone(), cx));
                self.workspace_content
                    .update(cx, |input, cx| input.set_content("", cx));
                self.set_status(format!("{filename} 尚不存在，可编辑后保存创建"), cx);
            }
            Err(err) => self.set_status(format!("读取工作区文件失败: {err}"), cx),
        }
    }

    fn load_workspace_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.workspace_file.read(cx).content().trim().to_string();
        if filename.is_empty() {
            self.set_status("请输入工作区文件名", cx);
            return;
        }
        self.load_workspace_file_named(filename, cx);
    }

    fn save_workspace_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.workspace_file.read(cx).content().trim().to_string();
        let content = self.workspace_content.read(cx).content().to_string();
        match WorkspaceService::write_workspace_file(&filename, &content) {
            Ok(()) => {
                self.reload();
                self.set_status(format!("已保存 {filename}"), cx);
            }
            Err(err) => self.set_status(format!("保存工作区文件失败: {err}"), cx),
        }
    }

    fn load_memory_file_named(&mut self, filename: String, cx: &mut Context<Self>) {
        match WorkspaceService::read_daily_memory_file(&filename) {
            Ok(Some(content)) => {
                self.memory_file
                    .update(cx, |input, cx| input.set_content(filename.clone(), cx));
                self.memory_content
                    .update(cx, |input, cx| input.set_content(content, cx));
                self.set_status(format!("已读取 {filename}"), cx);
            }
            Ok(None) => {
                self.memory_file
                    .update(cx, |input, cx| input.set_content(filename.clone(), cx));
                self.memory_content
                    .update(cx, |input, cx| input.set_content("", cx));
                self.set_status(format!("{filename} 尚不存在，可编辑后保存创建"), cx);
            }
            Err(err) => self.set_status(format!("读取每日记忆失败: {err}"), cx),
        }
    }

    fn load_memory_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.memory_file.read(cx).content().trim().to_string();
        if filename.is_empty() {
            self.set_status("请输入每日记忆文件名", cx);
            return;
        }
        self.load_memory_file_named(filename, cx);
    }

    fn save_memory_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.memory_file.read(cx).content().trim().to_string();
        let content = self.memory_content.read(cx).content().to_string();
        match WorkspaceService::write_daily_memory_file(&filename, &content) {
            Ok(()) => {
                self.reload();
                self.set_status(format!("已保存 {filename}"), cx);
            }
            Err(err) => self.set_status(format!("保存每日记忆失败: {err}"), cx),
        }
    }

    fn request_delete_memory(&mut self, filename: String, cx: &mut Context<Self>) {
        self.pending_delete = Some(filename);
        cx.notify();
    }

    fn cancel_delete_memory(&mut self, cx: &mut Context<Self>) {
        self.pending_delete = None;
        cx.notify();
    }

    fn confirm_delete_memory(&mut self, cx: &mut Context<Self>) {
        let Some(filename) = self.pending_delete.take() else {
            return;
        };
        self.delete_memory_file(filename, cx);
    }

    fn delete_memory_file(&mut self, filename: String, cx: &mut Context<Self>) {
        match WorkspaceService::delete_daily_memory_file(&filename) {
            Ok(()) => {
                self.reload();
                self.set_status(format!("已删除 {filename}"), cx);
            }
            Err(err) => self.set_status(format!("删除每日记忆失败: {err}"), cx),
        }
    }

    fn search_memory(&mut self, cx: &mut Context<Self>) {
        let query = self.memory_query.read(cx).content().trim().to_string();
        match WorkspaceService::search_daily_memory_files(&query) {
            Ok(results) => {
                let count = results.len();
                self.memory_results = results;
                self.set_status(format!("找到 {count} 个结果"), cx);
            }
            Err(err) => self.set_status(format!("搜索失败: {err}"), cx),
        }
    }

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary)
    }

    fn header(title: &str) -> impl IntoElement {
        div()
            .text_color(theme::c(theme::TEXT))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child(SharedString::from(title.to_string()))
    }

    fn render_file_card(&self, row: &WorkspaceFileRow, cx: &mut Context<Self>) -> impl IntoElement {
        let filename = row.filename.to_string();
        div()
            .id(SharedString::from(format!(
                "workspace-file-{}",
                row.filename
            )))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("打开 {}", row.filename)))
            .flex()
            .flex_col()
            .gap_2()
            .w(px(236.))
            .min_h(px(118.))
            .p_4()
            .rounded_lg()
            .cursor_pointer()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .hover(|s| s.border_color(theme::c(theme::BORDER_STRONG)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.filename),
                    )
                    .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(theme::c(
                        if row.exists {
                            theme::GREEN
                        } else {
                            theme::MUTED
                        },
                    ))),
            )
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .line_clamp(3)
                    .child(row.description),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.load_workspace_file_named(filename.clone(), cx);
            }))
    }

    fn render_memory_row(
        &self,
        file: &DailyMemoryFileInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let read_name = file.filename.clone();
        let delete_name = file.filename.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(file.filename.clone())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} 字节 · {}",
                                file.size_bytes, file.preview
                            ))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Self::action_button(
                            format!("workspace-memory-read-{}", file.filename),
                            "读取",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.load_memory_file_named(read_name.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("workspace-memory-delete-{}", file.filename),
                            "删除",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.request_delete_memory(delete_name.clone(), cx);
                            },
                        )),
                    ),
            )
    }
}

impl WorkspaceView {
    fn render_with_confirm(
        &self,
        base: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(filename) = self.pending_delete.clone() else {
            return base;
        };
        let modal = ConfirmModal::delete(
            "删除每日记忆",
            format!("确定要删除每日记忆文件“{filename}”吗？此操作无法撤销。"),
        );
        let confirm =
            components::action_button_tone("memory-delete-confirm", "删除", ButtonTone::Danger)
                .on_click(cx.listener(|this, _event, _window, cx| this.confirm_delete_memory(cx)));
        let cancel = components::action_button("memory-delete-cancel", "取消", false)
            .on_click(cx.listener(|this, _event, _window, cx| this.cancel_delete_memory(cx)));
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(base)
            .child(components::confirm_overlay(&modal, confirm, cancel))
            .into_any_element()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let file_cards: Vec<_> = self
            .files
            .iter()
            .map(|row| self.render_file_card(row, cx))
            .collect();
        let memory_rows: Vec<_> = self
            .memory_files
            .iter()
            .map(|file| self.render_memory_row(file, cx))
            .collect();
        let search_rows: Vec<_> = self
            .memory_results
            .iter()
            .map(|result| {
                div()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(theme::c(theme::SURFACE))
                    .border_1()
                    .border_color(theme::c(theme::BORDER))
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(format!(
                                "{} · {} 次",
                                result.filename, result.match_count
                            ))),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .line_clamp(2)
                            .child(SharedString::from(result.snippet.clone())),
                    )
            })
            .collect();

        let base = layout::page()
            .child(
                layout::page_header("工作区", Some("OpenClaw workspace 文件与每日记忆。".into()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Self::action_button("workspace-refresh", "刷新", false).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.reload();
                                        this.set_status("已刷新", cx);
                                    }),
                                ),
                            )
                            .child(
                                Self::action_button("workspace-open-dir", "打开目录", false)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_dir("workspace", cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::c(theme::TEAL))
                        .text_xs()
                        .child(status),
                )
            })
            .child(layout::scroll_body(
                "workspace-body",
                layout::content_column()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("工作区文件"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_3()
                                    .children(file_cards),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("编辑文件"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.workspace_file.clone())
                                    .child(
                                        Self::action_button(
                                            "workspace-load-selected",
                                            "读取",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.load_workspace_file(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "workspace-save-selected",
                                            "保存",
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.save_workspace_file(cx);
                                            }),
                                        ),
                                    ),
                            )
                            .child(self.workspace_content.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("每日记忆"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.memory_file.clone())
                                    .child(
                                        Self::action_button("workspace-memory-load", "读取", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.load_memory_file(cx);
                                            })),
                                    )
                                    .child(
                                        Self::action_button("workspace-memory-save", "保存", true)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.save_memory_file(cx);
                                            })),
                                    )
                                    .child(
                                        Self::action_button(
                                            "workspace-memory-open",
                                            "打开目录",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.open_dir("memory", cx);
                                            }),
                                        ),
                                    ),
                            )
                            .child(self.memory_content.clone())
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.memory_query.clone())
                                    .child(
                                        Self::action_button(
                                            "workspace-memory-search",
                                            "搜索",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.search_memory(cx);
                                            }),
                                        ),
                                    ),
                            )
                            .children(memory_rows)
                            .children(search_rows),
                    ),
            ))
            .into_any_element();
        self.render_with_confirm(base, cx)
    }
}

fn workspace_file_description(filename: &str) -> &'static str {
    match filename {
        "AGENTS.md" => "Agent 行为、约束和项目级操作规范。",
        "SOUL.md" => "OpenClaw 的长期风格、人格和协作偏好。",
        "USER.md" => "用户偏好、称呼和工作习惯。",
        "IDENTITY.md" => "身份、角色边界和默认立场。",
        "TOOLS.md" => "常用工具、命令和外部系统说明。",
        "MEMORY.md" => "长期记忆与跨会话上下文。",
        "HEARTBEAT.md" => "运行节奏、周期性检查和状态更新。",
        "BOOTSTRAP.md" => "首次启动或恢复上下文时的引导内容。",
        "BOOT.md" => "启动时立即读取的短指令。",
        _ => "OpenClaw workspace 文件。",
    }
}

struct AppErrorString(String);

impl AppErrorString {
    fn into_app_error(self) -> routedeck_core::AppError {
        routedeck_core::AppError::Message(self.0)
    }
}

fn open_path(path: &std::path::Path) -> Result<(), AppErrorString> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("explorer");
        cmd.arg(path);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    cmd.status()
        .map_err(|err| AppErrorString(err.to_string()))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(AppErrorString(format!("退出状态 {status}")))
            }
        })
}
