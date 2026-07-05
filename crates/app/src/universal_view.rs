//! Universal provider management. This ports the cc-switch "统一供应商" page
//! into GPUI: one shared endpoint/key can generate Claude and Codex
//! providers via `ProviderService::sync_universal_to_apps`.

use std::sync::Arc;

use gpui::{div, prelude::*, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::model::{
    ClaudeModelConfig, CodexModelConfig, UniversalProvider, UniversalProviderApps,
    UniversalProviderModels,
};
use routedeck_core::services::ProviderService;
use routedeck_core::AppState;
use uuid::Uuid;

use crate::components;
use crate::layout;
use crate::shell_menu;
use crate::text_input::TextInput;
use crate::theme;

pub struct UniversalView {
    app: Arc<AppState>,
    providers: Vec<UniversalProvider>,
    editing_id: Option<String>,
    name: Entity<TextInput>,
    base_url: Entity<TextInput>,
    api_key: Entity<TextInput>,
    website_url: Entity<TextInput>,
    notes: Entity<TextInput>,
    claude_model: Entity<TextInput>,
    codex_model: Entity<TextInput>,
    claude_enabled: bool,
    codex_enabled: bool,
    status: Option<SharedString>,
}

impl UniversalView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "供应商名称"));
        let base_url = cx.new(|cx| TextInput::new(cx, "https://api.example.com"));
        let api_key = cx.new(|cx| TextInput::new(cx, "API Key").masked(true));
        let website_url = cx.new(|cx| TextInput::new(cx, "https://example.com"));
        let notes = cx.new(|cx| TextInput::new(cx, "备注").multiline(true));
        let claude_model = cx.new(|cx| {
            let mut input = TextInput::new(cx, "claude-sonnet-4-20250514");
            input.set_content("claude-sonnet-4-20250514", cx);
            input
        });
        let codex_model = cx.new(|cx| {
            let mut input = TextInput::new(cx, "gpt-5.5");
            input.set_content("gpt-5.5", cx);
            input
        });
        let mut this = Self {
            app,
            providers: Vec::new(),
            editing_id: None,
            name,
            base_url,
            api_key,
            website_url,
            notes,
            claude_model,
            codex_model,
            claude_enabled: true,
            codex_enabled: true,
            status: None,
        };
        this.reload();
        this
    }

    pub fn reload(&mut self) {
        match ProviderService::list_universal(&self.app) {
            Ok(map) => {
                self.providers = map.into_values().collect();
                self.providers
                    .sort_by_key(|provider| provider.name.to_lowercase());
            }
            Err(err) => {
                self.providers = Vec::new();
                self.status = Some(SharedString::from(format!("加载统一供应商失败: {err}")));
            }
        }
    }

    fn input_value(input: &Entity<TextInput>, cx: &mut Context<Self>) -> String {
        input.read(cx).content().trim().to_string()
    }

    fn set_input(
        input: &Entity<TextInput>,
        value: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |input, cx| input.set_content(value, cx));
    }

    fn clear_form(&mut self, cx: &mut Context<Self>) {
        self.editing_id = None;
        Self::set_input(&self.name, "", cx);
        Self::set_input(&self.base_url, "", cx);
        Self::set_input(&self.api_key, "", cx);
        Self::set_input(&self.website_url, "", cx);
        Self::set_input(&self.notes, "", cx);
        Self::set_input(&self.claude_model, "claude-sonnet-4-20250514", cx);
        Self::set_input(&self.codex_model, "gpt-5.5", cx);
        self.claude_enabled = true;
        self.codex_enabled = true;
        self.status = Some(SharedString::from("已清空表单"));
        cx.notify();
    }

    fn edit_provider(&mut self, provider: UniversalProvider, cx: &mut Context<Self>) {
        self.editing_id = Some(provider.id.clone());
        Self::set_input(&self.name, provider.name.clone(), cx);
        Self::set_input(&self.base_url, provider.base_url.clone(), cx);
        Self::set_input(&self.api_key, provider.api_key.clone(), cx);
        Self::set_input(
            &self.website_url,
            provider.website_url.clone().unwrap_or_default(),
            cx,
        );
        Self::set_input(&self.notes, provider.notes.clone().unwrap_or_default(), cx);
        Self::set_input(
            &self.claude_model,
            provider
                .models
                .claude
                .as_ref()
                .and_then(|model| model.model.clone())
                .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string()),
            cx,
        );
        Self::set_input(
            &self.codex_model,
            provider
                .models
                .codex
                .as_ref()
                .and_then(|model| model.model.clone())
                .unwrap_or_else(|| "gpt-5.5".to_string()),
            cx,
        );
        self.claude_enabled = provider.apps.claude;
        self.codex_enabled = provider.apps.codex;
        self.status = Some(SharedString::from("正在编辑统一供应商"));
        cx.notify();
    }

    fn build_provider(&self, cx: &mut Context<Self>) -> Result<UniversalProvider, String> {
        let name = Self::input_value(&self.name, cx);
        let base_url = Self::input_value(&self.base_url, cx);
        let api_key = Self::input_value(&self.api_key, cx);
        if name.is_empty() || base_url.is_empty() || api_key.is_empty() {
            return Err("名称、Base URL 和 API Key 不能为空".to_string());
        }

        let id = self
            .editing_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut provider =
            UniversalProvider::new(id, name, "openai-compatible".to_string(), base_url, api_key);
        provider.apps = UniversalProviderApps {
            claude: self.claude_enabled,
            codex: self.codex_enabled,
            gemini: false,
        };
        provider.models = UniversalProviderModels {
            claude: Some(ClaudeModelConfig {
                model: Some(Self::input_value(&self.claude_model, cx)),
                haiku_model: None,
                sonnet_model: None,
                opus_model: None,
            }),
            codex: Some(CodexModelConfig {
                model: Some(Self::input_value(&self.codex_model, cx)),
                reasoning_effort: Some("high".to_string()),
            }),
            gemini: None,
        };
        provider.website_url = nonempty(Self::input_value(&self.website_url, cx));
        provider.notes = nonempty(Self::input_value(&self.notes, cx));
        if let Some(editing_id) = self.editing_id.as_deref() {
            if let Ok(Some(existing)) = ProviderService::get_universal(&self.app, editing_id) {
                provider.provider_type = existing.provider_type;
                provider.icon = existing.icon;
                provider.icon_color = existing.icon_color;
                provider.meta = existing.meta;
                provider.created_at = existing.created_at;
                provider.sort_index = existing.sort_index;
            }
        }
        Ok(provider)
    }

    fn save(&mut self, sync: bool, cx: &mut Context<Self>) {
        match self.build_provider(cx) {
            Ok(provider) => {
                let id = provider.id.clone();
                match ProviderService::upsert_universal(&self.app, provider) {
                    Ok(_) => {
                        if sync {
                            match ProviderService::sync_universal_to_apps(&self.app, &id) {
                                Ok(_) => {
                                    self.status = Some(SharedString::from("已保存并同步到各应用"))
                                }
                                Err(err) => {
                                    self.status = Some(SharedString::from(format!(
                                        "保存成功，同步失败: {err}"
                                    )))
                                }
                            }
                        } else {
                            self.status = Some(SharedString::from("统一供应商已保存"));
                        }
                        self.reload();
                        shell_menu::refresh(&self.app, cx);
                    }
                    Err(err) => {
                        self.status = Some(SharedString::from(format!("保存失败: {err}")));
                    }
                }
            }
            Err(err) => {
                self.status = Some(SharedString::from(err));
            }
        }
        cx.notify();
    }

    fn sync_provider(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::sync_universal_to_apps(&self.app, &id) {
            Ok(_) => {
                self.status = Some(SharedString::from("已同步到各应用"));
                shell_menu::refresh(&self.app, cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("同步失败: {err}"))),
        }
        cx.notify();
    }

    fn delete_provider(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::delete_universal(&self.app, &id) {
            Ok(_) => {
                if self.editing_id.as_deref() == Some(&id) {
                    self.editing_id = None;
                }
                self.status = Some(SharedString::from("统一供应商已删除"));
                self.reload();
                shell_menu::refresh(&self.app, cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        cx.notify();
    }

    fn toggle_app(&mut self, app: &'static str, cx: &mut Context<Self>) {
        match app {
            "claude" => self.claude_enabled = !self.claude_enabled,
            "codex" => self.codex_enabled = !self.codex_enabled,
            _ => {}
        }
        cx.notify();
    }

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary)
    }

    fn render_toggle(
        id: &'static str,
        label: &'static str,
        value: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .role(gpui::Role::Switch)
            .aria_label(label)
            .aria_toggled(if value {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .bg(theme::c(if value {
                theme::ACCENT
            } else {
                theme::SURFACE_HOVER
            }))
            .text_color(theme::c(if value {
                theme::ACCENT_TEXT
            } else {
                theme::TEXT
            }))
            .text_sm()
            .child(label)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                let key = match label {
                    "Claude" => "claude",
                    _ => "codex",
                };
                this.toggle_app(key, cx);
            }))
    }

    fn render_input_row(label: &'static str, input: Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(label),
            )
            .child(input)
    }

    fn render_provider_card(
        &self,
        provider: &UniversalProvider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let edit_provider = provider.clone();
        let sync_id = provider.id.clone();
        let delete_id = provider.id.clone();
        let app_labels = [
            (provider.apps.claude, "Claude"),
            (provider.apps.codex, "Codex"),
        ]
        .into_iter()
        .filter_map(|(enabled, label)| enabled.then_some(label))
        .collect::<Vec<_>>()
        .join(", ");

        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(provider.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(provider.base_url.clone())),
                    )
                    .child(div().text_color(theme::c(theme::TEAL)).text_xs().child(
                        SharedString::from(if app_labels.is_empty() {
                            "未启用应用".to_string()
                        } else {
                            format!("应用：{app_labels}")
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        Self::action_button(
                            format!("universal-edit-{}", provider.id),
                            "编辑",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.edit_provider(edit_provider.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("universal-sync-{}", provider.id),
                            "同步",
                            true,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.sync_provider(sync_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("universal-delete-{}", provider.id),
                            "删除",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_provider(delete_id.clone(), cx);
                            },
                        )),
                    ),
            )
    }
}

impl Render for UniversalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<_> = self
            .providers
            .iter()
            .map(|provider| self.render_provider_card(provider, cx))
            .collect();
        let is_empty = cards.is_empty();

        layout::page()
            .child(
                layout::page_header(
                    "统一供应商",
                    Some("同时生成并同步 Claude 和 Codex 的供应商配置。".into()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            Self::action_button("universal-new", "清空表单", false).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.clear_form(cx);
                                }),
                            ),
                        )
                        .child(
                            Self::action_button("universal-refresh", "刷新", false).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.reload();
                                    cx.notify();
                                }),
                            ),
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
                "universal-body",
                layout::content_column()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .p_4()
                            .rounded_lg()
                            .bg(theme::c(theme::SURFACE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if self.editing_id.is_some() {
                                        "编辑统一供应商"
                                    } else {
                                        "添加统一供应商"
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(Self::render_toggle(
                                        "universal-toggle-claude",
                                        "Claude",
                                        self.claude_enabled,
                                        cx,
                                    ))
                                    .child(Self::render_toggle(
                                        "universal-toggle-codex",
                                        "Codex",
                                        self.codex_enabled,
                                        cx,
                                    )),
                            )
                            .child(
                                div()
                                    .grid()
                                    .grid_cols(2)
                                    .gap_3()
                                    .child(Self::render_input_row("名称", self.name.clone()))
                                    .child(Self::render_input_row(
                                        "Base URL",
                                        self.base_url.clone(),
                                    ))
                                    .child(Self::render_input_row("API Key", self.api_key.clone()))
                                    .child(Self::render_input_row(
                                        "网站 URL",
                                        self.website_url.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "Claude 模型",
                                        self.claude_model.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "Codex 模型",
                                        self.codex_model.clone(),
                                    )),
                            )
                            .child(Self::render_input_row("备注", self.notes.clone()))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        Self::action_button("universal-save", "保存", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.save(false, cx);
                                            })),
                                    )
                                    .child(
                                        Self::action_button(
                                            "universal-save-sync",
                                            "保存并同步",
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.save(true, cx);
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("已保存统一供应商 {}", self.providers.len())),
                            )
                            .when(is_empty, |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("还没有统一供应商。填写上方表单后保存并同步。"),
                                )
                            })
                            .children(cards),
                    ),
            ))
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}
