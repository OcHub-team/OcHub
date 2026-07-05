//! Skills panel. Lists installed skills from the SQLite registry and drives
//! the Vercel `skills` CLI wrapper for install/uninstall/toggle/update.

use std::sync::Arc;

use std::collections::HashSet;

use gpui::{div, prelude::*, px, Context, FontWeight, SharedString, Window};
use routedeck_core::db::legacy_json::InstalledSkill;
use routedeck_core::services::skill::DiscoverableSkill;
use routedeck_core::services::SkillService;
use routedeck_core::{AppState, AppType};

use crate::components;
use crate::layout;
use crate::theme;

/// Drive a tokio-dependent future to completion on a dedicated current-thread
/// runtime. GPUI's executor has no tokio reactor, so SkillService calls that
/// reach into tokio::process / reqwest must be run here — and only ever from a
/// `cx.background_spawn` task, never inline on the UI thread. Mirrors the
/// pattern in tools_view.rs / usage_view.rs.
fn block_on_tokio<F, T>(fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(fut),
        Err(e) => Err(anyhow::anyhow!("创建异步运行时失败: {e}")),
    }
}

pub struct SkillsView {
    app: Arc<AppState>,
    skills: Vec<InstalledSkill>,
    discoverable: Vec<DiscoverableSkill>,
    updating: HashSet<String>,
    updating_all: bool,
    discovering: bool,
    installing: HashSet<String>,
    uninstalling: HashSet<String>,
    toggling: HashSet<String>,
    selected_app: AppType,
    status: Option<SharedString>,
}

impl SkillsView {
    pub fn new(app: Arc<AppState>, _cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            app,
            skills: Vec::new(),
            discoverable: Vec::new(),
            updating: HashSet::new(),
            updating_all: false,
            discovering: false,
            installing: HashSet::new(),
            uninstalling: HashSet::new(),
            toggling: HashSet::new(),
            selected_app: AppType::Claude,
            status: None,
        };
        this.reload();
        this
    }

    pub fn reload(&mut self) {
        match SkillService::get_all_installed(&self.app.db) {
            Ok(list) => self.skills = list,
            Err(err) => {
                self.skills = Vec::new();
                self.status = Some(SharedString::from(format!("加载技能失败: {err}")));
            }
        }
    }

    /// InstalledSkill has no version field; surface the source repo (the closest
    /// proxy for "which version") instead.
    fn source_label(skill: &InstalledSkill) -> String {
        match (&skill.repo_owner, &skill.repo_name) {
            (Some(owner), Some(name)) => {
                let branch = skill.repo_branch.as_deref().unwrap_or("main");
                format!("{owner}/{name}@{branch}")
            }
            _ => "本地".to_string(),
        }
    }

    fn do_uninstall(&mut self, id: String, cx: &mut Context<Self>) {
        if self.uninstalling.contains(&id) {
            return;
        }
        self.uninstalling.insert(id.clone());
        self.status = Some(SharedString::from("正在卸载技能..."));
        cx.notify();

        let app = self.app.clone();
        let task = cx.background_spawn(async move {
            let result = block_on_tokio(SkillService::uninstall(&app.db, &id));
            (id, result)
        });
        cx.spawn(async move |this, cx| {
            let (id, result) = task.await;
            this.update(cx, |this, cx| {
                this.uninstalling.remove(&id);
                match result {
                    Ok(_) => {
                        this.status = Some(SharedString::from("技能已卸载"));
                        this.updating.remove(&id);
                        this.reload();
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("卸载失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        if self.selected_app != app {
            self.selected_app = app;
            cx.notify();
        }
    }

    fn discover_skills(&mut self, cx: &mut Context<Self>) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.status = Some(SharedString::from("正在从技能仓库发现可安装技能..."));
        cx.notify();

        let app = self.app.clone();
        let task = cx.background_spawn(async move {
            let repos = app
                .db
                .get_skill_repos()
                .map_err(|err| anyhow::anyhow!(err.to_string()));
            match repos {
                Ok(repos) => block_on_tokio(SkillService::new().discover_available(repos)),
                Err(err) => Err(err),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.discovering = false;
                match result {
                    Ok(skills) => {
                        let count = skills.len();
                        this.discoverable = skills;
                        this.status =
                            Some(SharedString::from(format!("发现 {count} 个可安装技能")));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("发现技能失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install_discoverable(&mut self, skill: DiscoverableSkill, cx: &mut Context<Self>) {
        if self.installing.contains(&skill.key) {
            return;
        }
        let key = skill.key.clone();
        self.installing.insert(key.clone());
        self.status = Some(SharedString::from(format!("正在安装 {}...", skill.name)));
        cx.notify();

        let app = self.app.clone();
        let target_app = self.selected_app;
        let task = cx.background_spawn(async move {
            block_on_tokio(SkillService::new().install(&app.db, &skill, &target_app))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.installing.remove(&key);
                match result {
                    Ok(installed) => {
                        this.status =
                            Some(SharedString::from(format!("{} 已安装", installed.name)));
                        this.reload();
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("安装失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_app_for_skill(
        &mut self,
        id: String,
        target: AppType,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let toggle_key = format!("{id}:{}", target.as_str());
        if self.toggling.contains(&toggle_key) {
            return;
        }
        self.toggling.insert(toggle_key.clone());
        self.status = Some(SharedString::from("正在切换技能启用状态..."));
        cx.notify();

        let app = self.app.clone();
        let task = cx.background_spawn(async move {
            block_on_tokio(SkillService::toggle_app(&app.db, &id, &target, enabled))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.toggling.remove(&toggle_key);
                match result {
                    Ok(()) => {
                        this.status = Some(SharedString::from(if enabled {
                            "技能已启用"
                        } else {
                            "技能已禁用"
                        }));
                        this.reload();
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("切换失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_all(&mut self, cx: &mut Context<Self>) {
        if self.updating_all {
            return;
        }
        self.updating_all = true;
        self.status = Some(SharedString::from("正在更新全部技能..."));
        cx.notify();

        let app = self.app.clone();
        let task = cx.background_spawn(async move {
            block_on_tokio(SkillService::new().update_all(&app.db))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.updating_all = false;
                match result {
                    Ok(()) => {
                        this.status = Some(SharedString::from("全部技能已更新"));
                        this.reload();
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("更新失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_skill(&mut self, id: String, cx: &mut Context<Self>) {
        if self.updating.contains(&id) {
            return;
        }
        self.updating.insert(id.clone());
        self.status = Some(SharedString::from("正在更新技能..."));
        cx.notify();

        let app = self.app.clone();
        let task = cx.background_spawn(async move {
            let result = block_on_tokio(SkillService::new().update_skill(&app.db, &id));
            (id, result)
        });
        cx.spawn(async move |this, cx| {
            let (id, result) = task.await;
            this.update(cx, |this, cx| {
                this.updating.remove(&id);
                match result {
                    Ok(skill) => {
                        this.status = Some(SharedString::from(format!("{} 已更新", skill.name)));
                        this.reload();
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("更新失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_stat(label: &str, value: String, color: u32) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w(px(120.))
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(SharedString::from(label.to_string())),
            )
            .child(
                div()
                    .text_color(theme::c(color))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(value)),
            )
    }

    /// Apps the `skills` CLI can install into and the registry can persist.
    fn skill_apps() -> [AppType; 4] {
        [
            AppType::Claude,
            AppType::Codex,
            AppType::OpenCode,
            AppType::Hermes,
        ]
    }

    fn app_label(app: AppType) -> &'static str {
        match app {
            AppType::Claude => "Claude",
            AppType::Codex => "Codex",
            AppType::OpenCode => "OpenCode",
            AppType::Hermes => "Hermes",
            AppType::ClaudeDesktop => "Claude Desktop",
            AppType::OpenClaw => "OpenClaw",
        }
    }

    fn header(title: &str) -> impl IntoElement {
        div()
            .text_color(theme::c(theme::TEXT))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child(SharedString::from(title.to_string()))
    }

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary)
    }

    fn render_target_app_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child("安装目标"),
            )
            .children(Self::skill_apps().map(|app| {
                let selected = self.selected_app == app;
                div()
                    .id(SharedString::from(format!("skill-target-{}", app.as_str())))
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!(
                        "选择技能目标 {}",
                        Self::app_label(app)
                    )))
                    .aria_selected(selected)
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme::c(if selected {
                        theme::ACCENT
                    } else {
                        theme::SURFACE_HOVER
                    }))
                    .text_color(theme::c(if selected {
                        theme::ACCENT_TEXT
                    } else {
                        theme::TEXT
                    }))
                    .text_sm()
                    .child(Self::app_label(app))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.select_app(app, cx);
                    }))
            }))
    }

    fn render_discoverable_card(
        &self,
        skill: &DiscoverableSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let install_skill = skill.clone();
        let installing = self.installing.contains(&skill.key);
        let already_installed = self
            .skills
            .iter()
            .any(|installed| installed.directory == skill.directory);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(skill.name.clone())),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .truncate()
                                    .child(SharedString::from(format!(
                                        "{}/{}@{} · {}",
                                        skill.repo_owner,
                                        skill.repo_name,
                                        skill.repo_branch,
                                        skill.directory
                                    ))),
                            ),
                    )
                    .child(
                        Self::action_button(
                            format!("skill-install-{}", skill.key),
                            if already_installed {
                                "已安装"
                            } else if installing {
                                "安装中"
                            } else {
                                "安装"
                            },
                            !already_installed && !installing,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                if !already_installed {
                                    this.install_discoverable(install_skill.clone(), cx);
                                }
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .text_color(theme::c(theme::SUBTEXT))
                    .text_xs()
                    .line_clamp(2)
                    .child(SharedString::from(skill.description.clone())),
            )
    }

    fn render_app_toggles(
        &self,
        skill: &InstalledSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = skill.id.clone();
        let apps = skill.apps.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child("应用"),
            )
            .children(Self::skill_apps().map(|app| {
                let enabled = apps.is_enabled_for(&app);
                let toggle_id = id.clone();
                let busy = self
                    .toggling
                    .contains(&format!("{}:{}", id, app.as_str()));
                div()
                    .id(SharedString::from(format!(
                        "skill-toggle-{}-{}",
                        id,
                        app.as_str()
                    )))
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!(
                        "切换 {} 的 {} 启用状态",
                        toggle_id,
                        Self::app_label(app)
                    )))
                    .aria_selected(enabled)
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme::c(if enabled {
                        theme::ACCENT
                    } else {
                        theme::SURFACE_HOVER
                    }))
                    .text_color(theme::c(if busy {
                        theme::SUBTEXT
                    } else if enabled {
                        theme::ACCENT_TEXT
                    } else {
                        theme::TEXT
                    }))
                    .text_xs()
                    .child(Self::app_label(app))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.toggle_app_for_skill(toggle_id.clone(), app, !enabled, cx);
                    }))
            }))
    }

    fn render_card(&self, skill: &InstalledSkill, cx: &mut Context<Self>) -> impl IntoElement {
        let uninstall_id = skill.id.clone();
        let update_id = skill.id.clone();
        let name = skill.name.clone();
        let source = Self::source_label(skill);
        let desc = skill.description.clone();
        let is_updating = self.updating.contains(&skill.id);
        let is_uninstalling = self.uninstalling.contains(&skill.id);
        let is_remote = skill.repo_owner.is_some() && skill.repo_name.is_some();

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
                    .flex()
                    .flex_col()
                    .gap_2()
                    .min_w_0()
                    .w_full()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(source)),
                    )
                    .when_some(desc, |s, d| {
                        s.child(
                            div()
                                .w_full()
                                .text_color(theme::c(theme::SUBTEXT))
                                .text_xs()
                                .line_clamp(2)
                                .child(SharedString::from(d)),
                        )
                    })
                    .child(self.render_app_toggles(skill, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .w_full()
                    .when(is_remote, |s| {
                        s.child(
                            div()
                                .id(SharedString::from(format!("skill-update-{}", update_id)))
                                .role(gpui::Role::Button)
                                .aria_label("更新技能")
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(theme::c(if is_updating {
                                    theme::SURFACE_HOVER
                                } else {
                                    theme::ACCENT
                                }))
                                .text_color(theme::c(if is_updating {
                                    theme::SUBTEXT
                                } else {
                                    theme::ACCENT_TEXT
                                }))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(if is_updating { "更新中" } else { "更新" })
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.update_skill(update_id.clone(), cx);
                                })),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("skill-uninstall-{}", skill.id)))
                            .role(gpui::Role::Button)
                            .aria_label("卸载技能")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::c(theme::SURFACE_HOVER))
                            .text_color(theme::c(theme::RED))
                            .text_sm()
                            .child(if is_uninstalling { "卸载中" } else { "卸载" })
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.do_uninstall(uninstall_id.clone(), cx);
                            })),
                    )
                    .when(!is_remote, |s| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::MUTED))
                                .text_xs()
                                .child("本地技能"),
                        )
                    }),
            )
    }
}

impl Render for SkillsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards: Vec<_> = self
            .skills
            .iter()
            .map(|s| self.render_card(s, cx))
            .collect();
        let discoverable_cards: Vec<_> = self
            .discoverable
            .iter()
            .take(12)
            .map(|s| self.render_discoverable_card(s, cx))
            .collect();
        let is_empty = cards.is_empty();
        let discoverable_empty = discoverable_cards.is_empty();
        let remote_count = self
            .skills
            .iter()
            .filter(|skill| skill.repo_owner.is_some() && skill.repo_name.is_some())
            .count();

        layout::page()
            .child(
                layout::page_header("技能", Some("由 skills CLI 安装与同步的技能库".into())).child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            Self::action_button(
                                "skill-discover",
                                if self.discovering {
                                    "发现中"
                                } else {
                                    "发现技能"
                                },
                                true,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.discover_skills(cx);
                                },
                            )),
                        )
                        .child(
                            Self::action_button(
                                "skill-update-all",
                                if self.updating_all {
                                    "更新中"
                                } else {
                                    "全部更新"
                                },
                                false,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.update_all(cx);
                                },
                            )),
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
                "skill-list",
                layout::content_column()
                    .child(self.render_target_app_picker(cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_6()
                            .pb_2()
                            .child(Self::render_stat(
                                "已安装",
                                self.skills.len().to_string(),
                                theme::TEXT,
                            ))
                            .child(Self::render_stat(
                                "远程来源",
                                remote_count.to_string(),
                                theme::TEAL,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .pt_2()
                            .child(Self::header("可安装技能"))
                            .when(discoverable_empty, |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("点击“发现技能”从已启用仓库加载可安装技能。"),
                                )
                            })
                            .children(discoverable_cards),
                    )
                    .child(Self::header("已安装技能"))
                    .when(is_empty, |s| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::MUTED))
                                .child("还没有安装技能。"),
                        )
                    })
                    .children(cards),
            ))
    }
}
