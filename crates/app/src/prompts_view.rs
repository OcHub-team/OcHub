//! Prompts panel. Prompts are maintained per-app, so this view carries its own
//! app switcher and exposes the core prompt workflow: create, edit, import from
//! the live prompt file, enable, and delete.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::db::legacy_json::Prompt;
use ochub_core::services::PromptService;
use ochub_core::{AppState, AppType};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
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
    /// 待确认删除的提示词（id, 名称）；`Some` 时展示确认模态。
    confirm_delete: Option<(String, String)>,
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
            confirm_delete: None,
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

    /// app 切换器：`components::segmented`，选中 ix 映射回 `AppType`。
    fn render_app_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let apps = Self::apps();
        let labels: Vec<String> = apps
            .iter()
            .map(|app| Self::app_label(*app).to_string())
            .collect();
        let options: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = apps
            .iter()
            .position(|app| *app == self.selected_app)
            .unwrap_or(0);
        let apps_for_select = apps.clone();
        let on_select = cx.listener(move |this, ix: &usize, _window, cx| {
            if let Some(app) = apps_for_select.get(*ix).copied() {
                this.select_app(app, cx);
            }
        });
        components::segmented("prompts-app", &options, selected, move |ix, window, cx| {
            on_select(&ix, window, cx)
        })
    }

    fn render_card(&self, prompt: &Prompt, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = prompt.enabled;
        let enable_id = prompt.id.clone();
        let edit_prompt = prompt.clone();
        let delete_id = prompt.id.clone();
        let delete_name = prompt.name.clone();
        let name = prompt.name.clone();
        let desc = prompt.description.clone();
        let preview = Self::prompt_preview(prompt);

        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
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
                                s.child(components::badge(BadgeTone::Success, "已启用"))
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
                        components::button(
                            format!("prompt-enable-{}", prompt.id),
                            if enabled { "已启用" } else { "启用" },
                            if enabled {
                                ButtonTone::Neutral
                            } else {
                                ButtonTone::Primary
                            },
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.do_enable(enable_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            format!("prompt-edit-{}", prompt.id),
                            "编辑",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.start_edit(edit_prompt.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            format!("prompt-delete-{}", prompt.id),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete =
                                    Some((delete_id.clone(), delete_name.clone()));
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.form_mode == FormMode::Edit {
            "编辑提示词"
        } else {
            "新增提示词"
        };

        layout::page()
            .child(
                layout::page_header(title, None).child(
                    components::button(
                        "prompt-form-back",
                        "← 返回",
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.cancel_form(cx);
                    })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "prompt-form-body",
                layout::content_column().child(
                    components::card()
                        .gap_4()
                        .child(components::field("名称", false, None, self.name.clone()))
                        .child(components::field(
                            "描述",
                            false,
                            None,
                            self.description.clone(),
                        ))
                        .child(components::field("正文", false, None, self.content.clone()))
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
                                    components::button(
                                        "prompt-form-save",
                                        "保存",
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.do_save(cx);
                                        },
                                    )),
                                )
                                .child(
                                    components::button(
                                        "prompt-form-cancel",
                                        "取消",
                                        ButtonTone::Neutral,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.cancel_form(cx);
                                        },
                                    )),
                                ),
                        ),
                ),
            ))
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
            .relative()
            .child(
                layout::page_header(
                    "提示词",
                    Some("按应用管理提示词；启用后写回对应应用的提示词文件。".into()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            components::icon_button_tone(
                                "prompt-add",
                                "新增",
                                IconName::Add,
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.start_add(cx);
                                },
                            )),
                        )
                        .child(
                            components::icon_button_tone(
                                "prompt-import-file",
                                "从当前文件导入",
                                IconName::Archive,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.do_import_file(cx);
                                },
                            )),
                        ),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "prompt-list",
                layout::content_column()
                    .child(self.render_app_switcher(cx))
                    .when(is_empty, |s| {
                        s.child(components::empty_state(
                            IconName::Message,
                            "这个应用还没有提示词",
                            "新建提示词，或从当前文件导入现有内容。",
                            Some(
                                components::button(
                                    "prompt-add-empty",
                                    "新增提示词",
                                    ButtonTone::Primary,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.start_add(cx);
                                }))
                                .into_any_element(),
                            ),
                        ))
                    })
                    .children(cards),
            ))
            .when_some(self.confirm_delete.clone(), |root, target| {
                let (delete_id, name) = target;
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除提示词"))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(format!(
                                        "确定删除提示词「{name}」吗？此操作不可撤销。"
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "prompt-confirm-delete-cancel",
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
                                "prompt-confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(delete_id.clone(), cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }
}
