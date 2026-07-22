//! Prompts panel. Prompts are maintained per-app, so this view carries its own
//! app switcher and exposes the core prompt workflow: create, edit, import from
//! the live prompt file, enable, and delete.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::db::legacy_json::Prompt;
use ochub_core::services::PromptService;
use ochub_core::{AppState, AppType};

use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormMode {
    List,
    Add,
    Edit,
}

pub struct PromptsView {
    app: Arc<AppState>,
    selected_app: AppType,
    prompts: Vec<Prompt>,
    status: Option<SharedString>,
    form_mode: FormMode,
    editing_id: Option<String>,
    editing_enabled: bool,
    editing_created_at: Option<i64>,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    content: Entity<TextInput>,
}

impl PromptsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "提示词名称"));
        let description = cx.new(|cx| TextInput::new(cx, "描述（可选）"));
        let content = cx.new(|cx| TextInput::new(cx, "提示词正文").multiline(true));
        let mut this = Self {
            app,
            selected_app: AppType::Claude,
            prompts: Vec::new(),
            status: None,
            form_mode: FormMode::List,
            editing_id: None,
            editing_enabled: false,
            editing_created_at: None,
            name,
            description,
            content,
        };
        this.reload();
        this
    }

    fn apps() -> Vec<AppType> {
        crate::app_meta::enabled_prompt_apps()
    }

    fn app_label(app: AppType) -> SharedString {
        crate::app_meta::label(app)
    }

    pub fn reload(&mut self) {
        let apps = Self::apps();
        if !apps.contains(&self.selected_app) {
            if let Some(first) = apps.first() {
                self.selected_app = *first;
            }
        }
        match PromptService::get_prompts(&self.app, self.selected_app) {
            Ok(map) => self.prompts = map.into_values().collect(),
            Err(err) => {
                self.prompts = Vec::new();
                self.status = Some(SharedString::from(format!("加载提示词失败: {err}")));
            }
        }
    }

    fn select_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        if self.selected_app != app {
            self.selected_app = app;
            self.status = None;
            self.form_mode = FormMode::List;
            self.clear_form(cx);
            self.reload();
            cx.notify();
        }
    }

    fn clear_form(&mut self, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.editing_enabled = false;
        self.editing_created_at = None;
        self.name.update(cx, |input, cx| input.set_content("", cx));
        self.description
            .update(cx, |input, cx| input.set_content("", cx));
        self.content
            .update(cx, |input, cx| input.set_content("", cx));
    }

    fn start_add(&mut self, cx: &mut Context<Self>) {
        self.clear_form(cx);
        self.form_mode = FormMode::Add;
        self.status = None;
        cx.notify();
    }

    fn start_edit(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        self.form_mode = FormMode::Edit;
        self.editing_id = Some(prompt.id.clone());
        self.editing_enabled = prompt.enabled;
        self.editing_created_at = prompt.created_at;
        self.name
            .update(cx, |input, cx| input.set_content(prompt.name, cx));
        self.description.update(cx, |input, cx| {
            input.set_content(prompt.description.unwrap_or_default(), cx)
        });
        self.content
            .update(cx, |input, cx| input.set_content(prompt.content, cx));
        self.status = None;
        cx.notify();
    }

    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.form_mode = FormMode::List;
        self.clear_form(cx);
        cx.notify();
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn do_save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.status = Some(SharedString::from("名称不能为空"));
            cx.notify();
            return;
        }

        let description = self.description.read(cx).content().trim().to_string();
        let content = self.content.read(cx).content().to_string();
        let now = Self::now_secs();
        let id = self
            .editing_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let prompt = Prompt {
            id: id.clone(),
            name,
            content,
            description: (!description.is_empty()).then_some(description),
            enabled: self.editing_enabled,
            created_at: self.editing_created_at.or(Some(now)),
            updated_at: Some(now),
        };

        match PromptService::upsert_prompt(&self.app, self.selected_app, &id, prompt) {
            Ok(()) => {
                self.status = Some(SharedString::from(if self.form_mode == FormMode::Edit {
                    "提示词已保存"
                } else {
                    "提示词已创建"
                }));
                self.form_mode = FormMode::List;
                self.clear_form(cx);
                self.reload();
            }
            Err(err) => self.status = Some(SharedString::from(format!("保存失败: {err}"))),
        }
        cx.notify();
    }

    fn do_import_file(&mut self, cx: &mut Context<Self>) {
        match PromptService::import_from_file(&self.app, self.selected_app) {
            Ok(id) => {
                self.status = Some(SharedString::from(format!("已从当前文件导入提示词: {id}")))
            }
            Err(err) => self.status = Some(SharedString::from(format!("导入失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    fn do_enable(&mut self, id: String, cx: &mut Context<Self>) {
        match PromptService::enable_prompt(&self.app, self.selected_app, &id) {
            Ok(()) => self.status = Some(SharedString::from("提示词已启用")),
            Err(err) => self.status = Some(SharedString::from(format!("启用失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    fn prompt_preview(prompt: &Prompt) -> String {
        let normalized = prompt
            .content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        let preview = if normalized.is_empty() {
            "空正文".to_string()
        } else {
            normalized
        };
        let max_chars = 160;
        if preview.chars().count() > max_chars {
            format!("{}...", preview.chars().take(max_chars).collect::<String>())
        } else {
            preview
        }
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match PromptService::delete_prompt(&self.app, self.selected_app, &id) {
            Ok(()) => self.status = Some(SharedString::from("提示词已删除")),
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    fn render_app_pill(&self, app: AppType, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_app == app;
        div()
            .id(SharedString::from(format!("prompt-app-{}", app.as_str())))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "切换提示词应用到 {}",
                Self::app_label(app)
            )))
            .aria_selected(selected)
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .bg(if selected {
                theme::accent()
            } else {
                theme::surface()
            })
            .text_color(if selected {
                theme::accent_text()
            } else {
                theme::subtext()
            })
            .child(Self::app_label(app))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_app(app, cx);
            }))
    }

    fn render_card(&self, prompt: &Prompt, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = prompt.enabled;
        let enable_id = prompt.id.clone();
        let edit_prompt = prompt.clone();
        let delete_id = prompt.id.clone();
        let name = prompt.name.clone();
        let desc = prompt.description.clone();
        let preview = Self::prompt_preview(prompt);

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(if enabled {
                theme::green()
            } else {
                theme::border()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(name)),
                            )
                            .when(enabled, |s| {
                                s.child(
                                    div()
                                        .px_2()
                                        .rounded_md()
                                        .bg(theme::green())
                                        .text_color(theme::accent_text())
                                        .text_xs()
                                        .child("已启用"),
                                )
                            }),
                    )
                    .when_some(desc, |s, d| {
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(SharedString::from(d)),
                        )
                    })
                    .child(
                        div()
                            .max_w(px(680.))
                            .text_color(theme::subtext())
                            .text_xs()
                            .line_height(px(18.))
                            .child(SharedString::from(preview)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("prompt-enable-{}", prompt.id)))
                            .role(gpui::Role::Switch)
                            .aria_label("启用提示词")
                            .aria_toggled(if enabled {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if enabled {
                                theme::surface_hover()
                            } else {
                                theme::accent()
                            })
                            .text_color(if enabled {
                                theme::subtext()
                            } else {
                                theme::accent_text()
                            })
                            .text_sm()
                            .child(if enabled { "已启用" } else { "启用" })
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.do_enable(enable_id.clone(), cx);
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("prompt-edit-{}", prompt.id)))
                            .role(gpui::Role::Button)
                            .aria_label("编辑提示词")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::surface_hover())
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("编辑")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.start_edit(edit_prompt.clone(), cx);
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("prompt-delete-{}", prompt.id)))
                            .role(gpui::Role::Button)
                            .aria_label("删除提示词")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::surface_hover())
                            .text_color(theme::red())
                            .text_sm()
                            .child("删除")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.do_delete(delete_id.clone(), cx);
                            })),
                    ),
            )
    }

    fn render_field(&self, label: &str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .text_color(theme::subtext())
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(label.to_string())),
            )
            .child(input.clone())
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.form_mode == FormMode::Edit {
            "编辑提示词"
        } else {
            "新增提示词"
        };

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
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .id("prompt-form-back")
                            .role(gpui::Role::Button)
                            .aria_label("返回提示词列表")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::surface())
                            .text_color(theme::subtext())
                            .text_sm()
                            .child("返回列表")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.cancel_form(cx);
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .max_w(px(720.))
                    .child(self.render_field("名称", &self.name))
                    .child(self.render_field("描述", &self.description))
                    .child(self.render_field("正文", &self.content))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .line_height(px(18.))
                            .child("保存已启用的提示词时，会同步写回对应应用的提示词文件。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .child(
                                div()
                                    .id("prompt-form-save")
                                    .role(gpui::Role::Button)
                                    .aria_label("保存提示词")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::accent())
                                    .text_color(theme::accent_text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("保存")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_save(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("prompt-form-cancel")
                                    .role(gpui::Role::Button)
                                    .aria_label("取消编辑提示词")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::surface())
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child("取消")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_form(cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for PromptsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_mode != FormMode::List {
            return self.render_form(cx).into_any_element();
        }

        let cards: Vec<_> = self
            .prompts
            .iter()
            .map(|p| self.render_card(p, cx))
            .collect();
        let is_empty = cards.is_empty();

        layout::page()
            .child(
                div()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .child("提示词"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .mt_3()
                            .child(
                                div()
                                    .id("prompt-add")
                                    .role(gpui::Role::Button)
                                    .aria_label("新增提示词")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::accent())
                                    .text_color(theme::accent_text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("新增")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.start_add(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("prompt-import-file")
                                    .role(gpui::Role::Button)
                                    .aria_label("从当前文件导入提示词")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::surface())
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child("从当前文件导入")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_import_file(cx);
                                    })),
                            ),
                    )
                    .child(
                        div().flex().flex_row().flex_wrap().gap_2().mt_3().children(
                            Self::apps()
                                .into_iter()
                                .map(|a| self.render_app_pill(a, cx)),
                        ),
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
                "prompt-list",
                layout::content_column()
                    .when(is_empty, |s| {
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .child("这个应用还没有提示词。"),
                        )
                    })
                    .children(cards),
            ))
            .into_any_element()
    }
}
