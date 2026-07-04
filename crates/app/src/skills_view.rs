//! Skills panel. Lists installed skills from the SSOT registry and exposes the
//! installed-skill lifecycle actions that can run safely inside the GPUI app.

use std::sync::Arc;

use std::collections::{HashMap, HashSet};

use gpui::{div, prelude::*, px, Context, FontWeight, SharedString, Window};
use routedeck_core::db::legacy_json::{InstalledSkill, SkillApps, UnmanagedSkill};
use routedeck_core::services::skill::{
    DiscoverableSkill, ImportSkillSelection, SkillBackupEntry, SkillUpdateInfo,
};
use routedeck_core::services::SkillService;
use routedeck_core::{AppState, AppType};

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

pub struct SkillsView {
    app: Arc<AppState>,
    skills: Vec<InstalledSkill>,
    discoverable: Vec<DiscoverableSkill>,
    backups: Vec<SkillBackupEntry>,
    unmanaged: Vec<UnmanagedSkill>,
    updates: HashMap<String, SkillUpdateInfo>,
    updating: HashSet<String>,
    checking_updates: bool,
    discovering: bool,
    installing: HashSet<String>,
    restoring: HashSet<String>,
    selected_app: AppType,
    zip_path: gpui::Entity<TextInput>,
    status: Option<SharedString>,
}

impl SkillsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let zip_path = cx.new(|cx| TextInput::new(cx, "/path/to/skill.zip"));
        let mut this = Self {
            app,
            skills: Vec::new(),
            discoverable: Vec::new(),
            backups: Vec::new(),
            unmanaged: Vec::new(),
            updates: HashMap::new(),
            updating: HashSet::new(),
            checking_updates: false,
            discovering: false,
            installing: HashSet::new(),
            restoring: HashSet::new(),
            selected_app: AppType::Claude,
            zip_path,
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
        self.backups = SkillService::list_backups().unwrap_or_default();
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

    fn enabled_apps_label(skill: &InstalledSkill) -> String {
        let mut apps = Vec::new();
        if skill.apps.claude {
            apps.push("claude");
        }
        if skill.apps.codex {
            apps.push("codex");
        }
        if skill.apps.gemini {
            apps.push("gemini");
        }
        if skill.apps.opencode {
            apps.push("opencode");
        }
        if skill.apps.hermes {
            apps.push("hermes");
        }
        if apps.is_empty() {
            "未启用应用".to_string()
        } else {
            apps.join(", ")
        }
    }

    fn do_uninstall(&mut self, id: String, cx: &mut Context<Self>) {
        match SkillService::uninstall(&self.app.db, &id) {
            Ok(_) => self.status = Some(SharedString::from("技能已卸载")),
            Err(err) => self.status = Some(SharedString::from(format!("卸载失败: {err}"))),
        }
        self.updates.remove(&id);
        self.updating.remove(&id);
        self.reload();
        cx.notify();
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
        cx.spawn(async move |this, cx| {
            let result = app
                .db
                .get_skill_repos()
                .map_err(|err| anyhow::anyhow!(err.to_string()));
            let result = match result {
                Ok(repos) => SkillService::new().discover_available(repos).await,
                Err(err) => Err(err),
            };
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
        cx.spawn(async move |this, cx| {
            let result = SkillService::new()
                .install(&app.db, &skill, &target_app)
                .await;
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

    fn scan_unmanaged(&mut self, cx: &mut Context<Self>) {
        match SkillService::scan_unmanaged(&self.app.db) {
            Ok(list) => {
                let count = list.len();
                self.unmanaged = list;
                self.status = Some(SharedString::from(format!("发现 {count} 个未管理技能")));
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("扫描未管理技能失败: {err}")));
            }
        }
        cx.notify();
    }

    fn import_unmanaged(&mut self, directory: String, cx: &mut Context<Self>) {
        let selection = ImportSkillSelection {
            directory: directory.clone(),
            apps: SkillApps::only(&self.selected_app),
        };
        match SkillService::import_from_apps(&self.app.db, vec![selection]) {
            Ok(imported) => {
                self.status = Some(SharedString::from(format!(
                    "已导入 {} 个技能",
                    imported.len()
                )));
                self.unmanaged.retain(|skill| skill.directory != directory);
                self.reload();
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("导入失败: {err}")));
            }
        }
        cx.notify();
    }

    fn install_zip(&mut self, cx: &mut Context<Self>) {
        let path = self.zip_path.read(cx).content().trim().to_string();
        if path.is_empty() {
            self.status = Some(SharedString::from("请输入 ZIP 文件路径"));
            cx.notify();
            return;
        }
        match SkillService::install_from_zip(
            &self.app.db,
            std::path::Path::new(&path),
            &self.selected_app,
        ) {
            Ok(skills) => {
                self.status = Some(SharedString::from(format!(
                    "已从 ZIP 安装 {} 个技能",
                    skills.len()
                )));
                self.reload();
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("ZIP 安装失败: {err}")));
            }
        }
        cx.notify();
    }

    fn restore_backup(&mut self, backup_id: String, cx: &mut Context<Self>) {
        if self.restoring.contains(&backup_id) {
            return;
        }
        self.restoring.insert(backup_id.clone());
        match SkillService::restore_from_backup(&self.app.db, &backup_id, &self.selected_app) {
            Ok(skill) => {
                self.status = Some(SharedString::from(format!("{} 已从备份恢复", skill.name)));
                self.reload();
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("恢复备份失败: {err}")));
            }
        }
        self.restoring.remove(&backup_id);
        cx.notify();
    }

    fn delete_backup(&mut self, backup_id: String, cx: &mut Context<Self>) {
        match SkillService::delete_backup(&backup_id) {
            Ok(()) => {
                self.status = Some(SharedString::from("技能备份已删除"));
                self.backups = SkillService::list_backups().unwrap_or_default();
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("删除备份失败: {err}")));
            }
        }
        cx.notify();
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.checking_updates {
            return;
        }
        self.checking_updates = true;
        self.status = Some(SharedString::from("正在检查技能更新..."));
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let service = SkillService::new();
            let result = service.check_updates(&app.db).await;
            this.update(cx, |this, cx| {
                this.checking_updates = false;
                match result {
                    Ok(updates) => {
                        this.updates = updates
                            .into_iter()
                            .map(|update| (update.id.clone(), update))
                            .collect();
                        this.status = Some(SharedString::from(if this.updates.is_empty() {
                            "所有远程技能都是最新版本".to_string()
                        } else {
                            format!("发现 {} 个技能可更新", this.updates.len())
                        }));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("检查更新失败: {err}")));
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
        cx.spawn(async move |this, cx| {
            let service = SkillService::new();
            let result = service.update_skill(&app.db, &id).await;
            this.update(cx, |this, cx| {
                this.updating.remove(&id);
                match result {
                    Ok(skill) => {
                        this.status = Some(SharedString::from(format!("{} 已更新", skill.name)));
                        this.updates.remove(&id);
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

    fn update_summary(&self) -> String {
        if self.checking_updates {
            return "检查中".to_string();
        }
        match self.updates.len() {
            0 => "无待更新".to_string(),
            count => format!("{count} 个可更新"),
        }
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

    fn skill_apps() -> [AppType; 5] {
        [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::OpenCode,
            AppType::Hermes,
        ]
    }

    fn app_label(app: AppType) -> &'static str {
        match app {
            AppType::Claude => "Claude",
            AppType::Codex => "Codex",
            AppType::Gemini => "Gemini",
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
                    .child("安装/导入目标"),
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

    fn render_unmanaged_row(
        &self,
        skill: &UnmanagedSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let directory = skill.directory.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
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
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(skill.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} · {}",
                                skill.found_in.join(", "),
                                skill.path
                            ))),
                    ),
            )
            .child(
                Self::action_button(format!("skill-import-{}", skill.directory), "导入", true)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.import_unmanaged(directory.clone(), cx);
                    })),
            )
    }

    fn render_backup_row(
        &self,
        backup: &SkillBackupEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let restore_id = backup.backup_id.clone();
        let delete_id = backup.backup_id.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
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
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(SharedString::from(backup.skill.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} · {}",
                                backup.backup_id, backup.backup_path
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
                            format!("skill-backup-restore-{}", backup.backup_id),
                            "恢复",
                            true,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.restore_backup(restore_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("skill-backup-delete-{}", backup.backup_id),
                            "删除",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_backup(delete_id.clone(), cx);
                            },
                        )),
                    ),
            )
    }

    fn render_card(&self, skill: &InstalledSkill, cx: &mut Context<Self>) -> impl IntoElement {
        let uninstall_id = skill.id.clone();
        let update_id = skill.id.clone();
        let name = skill.name.clone();
        let source = Self::source_label(skill);
        let apps = Self::enabled_apps_label(skill);
        let desc = skill.description.clone();
        let update = self.updates.get(&skill.id).cloned();
        let is_updating = self.updating.contains(&skill.id);
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
            .border_color(theme::c(if update.is_some() {
                theme::YELLOW
            } else {
                theme::BORDER
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .min_w_0()
                    .w_full()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(name)),
                            )
                            .when(update.is_some(), |s| {
                                s.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(theme::c(theme::YELLOW))
                                        .text_color(theme::c(theme::ACCENT_TEXT))
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("可更新"),
                                )
                            }),
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
                    .child(
                        div()
                            .w_full()
                            .text_color(theme::c(theme::TEAL))
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(format!("应用：{apps}"))),
                    )
                    .when_some(update, |s, update| {
                        s.child(div().text_color(theme::c(theme::MUTED)).text_xs().child(
                            SharedString::from(format!(
                                    "本地 {} → 远程 {}",
                                    update
                                        .current_hash
                                        .as_deref()
                                        .map(short_hash)
                                        .unwrap_or("未知".to_string()),
                                    short_hash(&update.remote_hash)
                                )),
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .w_full()
                    .when(self.updates.contains_key(&skill.id), |s| {
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
                            .child("卸载")
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
        let unmanaged_rows: Vec<_> = self
            .unmanaged
            .iter()
            .map(|s| self.render_unmanaged_row(s, cx))
            .collect();
        let backup_rows: Vec<_> = self
            .backups
            .iter()
            .take(8)
            .map(|backup| self.render_backup_row(backup, cx))
            .collect();
        let is_empty = cards.is_empty();
        let discoverable_empty = discoverable_cards.is_empty();
        let unmanaged_empty = unmanaged_rows.is_empty();
        let backup_empty = backup_rows.is_empty();
        let remote_count = self
            .skills
            .iter()
            .filter(|skill| skill.repo_owner.is_some() && skill.repo_name.is_some())
            .count();

        layout::page()
            .child(
                layout::page_header("技能", Some("SSOT 技能库、应用同步与远程更新".into())).child(
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
                            Self::action_button("skill-scan-unmanaged", "扫描导入", false)
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.scan_unmanaged(cx);
                                })),
                        )
                        .child(
                            Self::action_button(
                                "skill-check-updates",
                                if self.checking_updates {
                                    "检查中"
                                } else {
                                    "检查更新"
                                },
                                false,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.check_updates(cx);
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
                            ))
                            .child(Self::render_stat(
                                "更新状态",
                                self.update_summary(),
                                if self.updates.is_empty() {
                                    theme::GREEN
                                } else {
                                    theme::YELLOW
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .pt_2()
                            .child(Self::header("从 ZIP 安装"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.zip_path.clone())
                                    .child(
                                        Self::action_button("skill-install-zip", "安装 ZIP", true)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.install_zip(cx);
                                            })),
                                    ),
                            ),
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
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .pt_2()
                            .child(Self::header("未管理技能"))
                            .when(unmanaged_empty, |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("点击“扫描导入”查找应用目录中尚未纳管的技能。"),
                                )
                            })
                            .children(unmanaged_rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .pt_2()
                            .child(Self::header("卸载备份"))
                            .when(backup_empty, |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("暂无可恢复的技能备份。"),
                                )
                            })
                            .children(backup_rows),
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

fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}
