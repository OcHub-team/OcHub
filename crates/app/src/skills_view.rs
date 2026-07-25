//! Skills panel. Tabbed management of the central skill registry: installed
//! skills with per-app toggles plus discovery through skills.sh and configured
//! repositories. File installation and app linking are delegated to the
//! Vercel `skills` CLI; all disk and network work runs off the UI thread.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use std::collections::{HashMap, HashSet};

use gpui::{
    div, prelude::*, px, Context, FontWeight, ListAlignment, ListState, SharedString, Window,
};
use ochub_core::db::legacy_json::{InstalledSkill, SkillRepo};
use ochub_core::services::skill::{DiscoverableSkill, SkillUpdateInfo, SkillsShDiscoverableSkill};
use ochub_core::services::SkillService;
use ochub_core::{AppState, AppType};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

/// 每次市场搜索的分页大小。
const MARKET_PAGE_SIZE: usize = 30;

/// 破坏性操作确认目标（卸载技能 / 删除仓库），携带展示名称。
#[derive(Clone)]
enum ConfirmAction {
    Uninstall { id: String, name: String },
    DeleteRepo { owner: String, name: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SkillsTab {
    Installed,
    Discover,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscoverMode {
    Market,
    Repos,
}

/// One row of the virtualized body. Fixed sections carry their own variant;
/// the dynamic card lists index into their backing vecs.
#[derive(Clone, Copy)]
enum SkillRow {
    Tabs,
    // 已安装
    Stats,
    InstalledToolbar,
    Installed(usize),
    // 发现
    DiscoverToolbar,
    MarketBar,
    Market(usize),
    MarketMore,
    RepoManager,
    RepoResultsHeader,
    Discoverable(usize),
}

pub struct SkillsView {
    app: Arc<AppState>,
    tab: SkillsTab,
    discover_mode: DiscoverMode,
    skills: Vec<InstalledSkill>,
    repos: Vec<SkillRepo>,
    discoverable: Vec<DiscoverableSkill>,
    market_results: Vec<SkillsShDiscoverableSkill>,
    market_total: usize,
    updates: HashMap<String, SkillUpdateInfo>,
    updating: HashSet<String>,
    installing: HashSet<String>,
    /// `"<skill id>:<app id>"` 键，标记正在切换的应用开关。
    toggling: HashSet<String>,
    uninstalling: HashSet<String>,
    checking_updates: bool,
    updating_all: bool,
    discovering: bool,
    searching_market: bool,
    /// 自动检查更新是否已触发过（视图生命周期内一次）。
    auto_checked: bool,
    selected_app: AppType,
    search_input: gpui::Entity<TextInput>,
    market_input: gpui::Entity<TextInput>,
    repo_input: gpui::Entity<TextInput>,
    /// 上次生效的过滤词，用于把输入框光标闪烁与真实内容变化区分开。
    last_filter: SharedString,
    /// 待确认的破坏性操作；`Some` 时展示确认模态。
    confirm: Option<ConfirmAction>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    list_state: ListState,
}

impl SkillsView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // Placeholders are captured when the input is constructed, and this
        // view is built once at startup, so they need pushing in by hand.
        self.search_input.update(cx, |input, cx| {
            input.set_placeholder(t(k::SKILLS_INSTALLED_SEARCH_PLACEHOLDER), cx)
        });
        self.market_input.update(cx, |input, cx| {
            input.set_placeholder(t(k::SKILLS_MARKET_SEARCH_PLACEHOLDER), cx)
        });
        self.repo_input.update(cx, |input, cx| {
            input.set_placeholder(t(k::SKILLS_REPO_INPUT_PLACEHOLDER), cx)
        });
        self.list_state.remeasure();
        cx.notify();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let search_input =
            cx.new(|cx| TextInput::new(cx, t(k::SKILLS_INSTALLED_SEARCH_PLACEHOLDER)));
        let market_input = cx.new(|cx| TextInput::new(cx, t(k::SKILLS_MARKET_SEARCH_PLACEHOLDER)));
        let repo_input = cx.new(|cx| TextInput::new(cx, t(k::SKILLS_REPO_INPUT_PLACEHOLDER)));
        let mut this = Self {
            app,
            tab: SkillsTab::Installed,
            discover_mode: DiscoverMode::Market,
            skills: Vec::new(),
            repos: Vec::new(),
            discoverable: Vec::new(),
            market_results: Vec::new(),
            market_total: 0,
            updates: HashMap::new(),
            updating: HashSet::new(),
            installing: HashSet::new(),
            toggling: HashSet::new(),
            uninstalling: HashSet::new(),
            checking_updates: false,
            updating_all: false,
            discovering: false,
            searching_market: false,
            auto_checked: false,
            selected_app: AppType::Claude,
            search_input,
            market_input,
            repo_input,
            last_filter: SharedString::default(),
            confirm: None,
            status: None,
            status_level: None,
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
        };

        // 过滤输入变化时重排列表（忽略光标闪烁等无关通知）。
        cx.observe(&this.search_input, |this, input, cx| {
            let content = input.read(cx).content();
            if content != this.last_filter {
                this.last_filter = content;
                this.list_state.remeasure();
                cx.notify();
            }
        })
        .detach();

        let run_search = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.run_market_search(false, cx);
        });
        this.market_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| run_search(&(), window, cx));
        });
        let add_repo = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.add_repo(cx);
        });
        this.repo_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| add_repo(&(), window, cx));
        });

        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let apps = Self::skill_apps();
        if !apps.contains(&self.selected_app) {
            if let Some(first) = apps.first() {
                self.selected_app = *first;
            }
        }
        match SkillService::get_all_installed(&self.app.db) {
            Ok(list) => self.skills = list,
            Err(err) => {
                self.skills = Vec::new();
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SKILLS_STATUS_LOAD_FAILED, error = err),
                    cx,
                );
            }
        }
        self.repos = self.app.db.get_skill_repos().unwrap_or_default();
        self.list_state.remeasure();
        cx.notify();
    }

    /// 页面首次显示且存在远程技能时自动检查一次更新。视图在应用启动时就被
    /// 构造，因此挂在首次 render 而不是 `new()`，避免启动即触发网络请求。
    fn maybe_auto_check_updates(&mut self, cx: &mut Context<Self>) {
        if self.auto_checked {
            return;
        }
        self.auto_checked = true;
        if self.skills.iter().any(is_remote) {
            self.check_updates(cx);
        }
    }

    // ── 通用工具 ────────────────────────────────────────────────────────────

    fn skill_apps() -> Vec<AppType> {
        crate::app_meta::enabled_skill_apps()
    }

    fn app_label(app: AppType) -> SharedString {
        crate::app_meta::label(app)
    }

    fn source_label(skill: &InstalledSkill) -> String {
        match (&skill.repo_owner, &skill.repo_name) {
            (Some(owner), Some(name)) => {
                let branch = skill.repo_branch.as_deref().unwrap_or("main");
                format!("{owner}/{name}@{branch}")
            }
            _ => raw(k::SKILLS_SOURCE_LOCAL).to_string(),
        }
    }

    fn refresh_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.remeasure();
        cx.notify();
    }

    /// Every status toast carries its severity explicitly. Guessing it from the
    /// wording mis-reads several of these messages (a partial batch update is
    /// not a clean success) and stops working entirely once the copy is
    /// translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        msg: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(msg.into());
        self.status_level = Some(level);
        cx.notify();
    }

    /// 在后台线程的临时 tokio runtime 中执行核心异步调用。
    /// SkillService 的网络路径（下载仓库、市场搜索）依赖 tokio 定时器，
    /// 不能在 GPUI 前台 executor 上直接 await，否则 `Handle::current` 会
    /// panic 并因跨 objc 栈无法 unwind 而直接 abort。
    fn spawn_tokio<T: Send + 'static>(
        cx: &mut Context<Self>,
        fut: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> gpui::Task<anyhow::Result<T>> {
        cx.background_spawn(crate::core_async::run(fut))
    }

    fn set_tab(&mut self, tab: SkillsTab, cx: &mut Context<Self>) {
        if self.tab == tab {
            self.refresh_list(cx);
            return;
        }
        self.tab = tab;
        if tab == SkillsTab::Discover
            && self.discover_mode == DiscoverMode::Repos
            && self.discoverable.is_empty()
        {
            self.discover_skills(cx);
        }
        self.refresh_list(cx);
    }

    fn set_discover_mode(&mut self, mode: DiscoverMode, cx: &mut Context<Self>) {
        if self.discover_mode == mode {
            return;
        }
        self.discover_mode = mode;
        if mode == DiscoverMode::Repos && self.discoverable.is_empty() {
            self.discover_skills(cx);
        }
        self.refresh_list(cx);
    }

    fn select_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        if self.selected_app != app {
            self.selected_app = app;
            cx.notify();
        }
    }

    /// 过滤 + 排序后的已安装技能下标：可更新的在前，其余按名称。
    fn installed_indices(&self, filter: &str) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.skills.len())
            .filter(|&ix| {
                if filter.is_empty() {
                    return true;
                }
                let skill = &self.skills[ix];
                let haystack = format!(
                    "{} {} {} {}",
                    skill.name,
                    skill.description.as_deref().unwrap_or(""),
                    skill.directory,
                    Self::source_label(skill)
                )
                .to_lowercase();
                haystack.contains(filter)
            })
            .collect();
        indices.sort_by(|&a, &b| {
            let ua = self.updates.contains_key(&self.skills[a].id);
            let ub = self.updates.contains_key(&self.skills[b].id);
            ub.cmp(&ua)
                .then_with(|| self.skills[a].name.cmp(&self.skills[b].name))
        });
        indices
    }

    // ── 已安装：更新 / 开关 / 卸载 / 同步 ───────────────────────────────────

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.checking_updates {
            return;
        }
        self.checking_updates = true;
        self.set_status(
            NotificationLevel::Info,
            t(k::SKILLS_STATUS_CHECKING_UPDATES),
            cx,
        );

        let app = self.app.clone();
        let task = Self::spawn_tokio(cx, async move {
            SkillService::new().check_updates(&app.db).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.checking_updates = false;
                match result {
                    Ok(updates) => {
                        this.updates = updates
                            .into_iter()
                            .map(|update| (update.id.clone(), update))
                            .collect();
                        // Nothing to do is a clean result; pending updates are
                        // neutral news, not a warning.
                        let (level, message) = if this.updates.is_empty() {
                            (
                                NotificationLevel::Success,
                                raw(k::SKILLS_STATUS_ALL_UP_TO_DATE).to_string(),
                            )
                        } else {
                            (
                                NotificationLevel::Info,
                                tf!(k::SKILLS_STATUS_UPDATES_FOUND, count = this.updates.len()),
                            )
                        };
                        this.set_status(level, message, cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_CHECK_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.refresh_list(cx);
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
        self.set_status(NotificationLevel::Info, t(k::SKILLS_STATUS_UPDATING), cx);

        let app = self.app.clone();
        let task_id = id.clone();
        let task = Self::spawn_tokio(cx, async move {
            SkillService::new().update_skill(&app.db, &task_id).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.updating.remove(&id);
                match result {
                    Ok(skill) => {
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::SKILLS_STATUS_UPDATED, name = skill.name),
                            cx,
                        );
                        this.updates.remove(&id);
                        this.reload(cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_UPDATE_FAILED, error = err),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 逐个更新所有有新版本的技能（顺序执行，避免仓库并发下载冲突）。
    fn update_all(&mut self, cx: &mut Context<Self>) {
        if self.updating_all || self.updates.is_empty() {
            return;
        }
        let ids: Vec<String> = self.updates.keys().cloned().collect();
        self.updating_all = true;
        for id in &ids {
            self.updating.insert(id.clone());
        }
        self.set_status(
            NotificationLevel::Info,
            tf!(k::SKILLS_STATUS_UPDATING_MANY, count = ids.len()),
            cx,
        );

        let app = self.app.clone();
        let task = Self::spawn_tokio(cx, async move {
            let service = SkillService::new();
            let mut ok_ids = Vec::new();
            let mut errors = Vec::new();
            for id in ids {
                match service.update_skill(&app.db, &id).await {
                    Ok(skill) => ok_ids.push((id, skill.name)),
                    Err(err) => errors.push(format!("{err}")),
                }
            }
            anyhow::Ok((ok_ids, errors))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.updating_all = false;
                this.updating.clear();
                match result {
                    Ok((ok_ids, errors)) => {
                        for (id, _) in &ok_ids {
                            this.updates.remove(id);
                        }
                        // A batch that left some skills behind is a warning, not
                        // the clean success the leading "已更新" suggests.
                        let (level, message) = if errors.is_empty() {
                            (
                                NotificationLevel::Success,
                                tf!(k::SKILLS_STATUS_UPDATED_MANY, count = ok_ids.len()),
                            )
                        } else {
                            (
                                NotificationLevel::Warning,
                                tf!(
                                    k::SKILLS_STATUS_UPDATED_PARTIAL,
                                    count = ok_ids.len(),
                                    failed = errors.len(),
                                    details = errors.join(raw(k::SKILLS_STATUS_ERROR_SEPARATOR)),
                                ),
                            )
                        };
                        this.set_status(level, message, cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_UPDATE_ALL_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    fn toggle_skill_app(
        &mut self,
        id: String,
        app_type: AppType,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let key = format!("{id}:{}", app_type.as_str());
        if self.toggling.contains(&key) {
            return;
        }
        self.toggling.insert(key.clone());
        cx.notify();

        let app = self.app.clone();
        let task = Self::spawn_tokio(cx, async move {
            SkillService::toggle_app(&app.db, &id, &app_type, enabled).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.toggling.remove(&key);
                match result {
                    Ok(()) => {
                        // Whole sentences per branch: the verb cannot be swapped
                        // into a Chinese frame and still read as English.
                        let app = Self::app_label(app_type);
                        let message = if enabled {
                            tf!(k::SKILLS_STATUS_APP_ENABLED, app = app)
                        } else {
                            tf!(k::SKILLS_STATUS_APP_DISABLED, app = app)
                        };
                        this.set_status(NotificationLevel::Success, message, cx);
                        this.reload(cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_TOGGLE_FAILED, error = err),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn do_uninstall(&mut self, id: String, cx: &mut Context<Self>) {
        if self.uninstalling.contains(&id) {
            return;
        }
        self.uninstalling.insert(id.clone());
        self.set_status(
            NotificationLevel::Info,
            t(k::SKILLS_STATUS_UNINSTALLING),
            cx,
        );

        let app = self.app.clone();
        let task_id = id.clone();
        let task = Self::spawn_tokio(cx, async move {
            SkillService::uninstall(&app.db, &task_id).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.uninstalling.remove(&id);
                this.updates.remove(&id);
                this.updating.remove(&id);
                match result {
                    Ok(_) => {
                        this.set_status(
                            NotificationLevel::Success,
                            t(k::SKILLS_STATUS_UNINSTALLED),
                            cx,
                        );
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_UNINSTALL_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_repo_page(&mut self, skill: &InstalledSkill, cx: &mut Context<Self>) {
        let url =
            skill
                .readme_url
                .clone()
                .or_else(|| match (&skill.repo_owner, &skill.repo_name) {
                    (Some(owner), Some(name)) => Some(format!("https://github.com/{owner}/{name}")),
                    _ => None,
                });
        let Some(url) = url else {
            // Refused rather than failed: a local skill simply has no repo.
            self.set_status(NotificationLevel::Warning, t(k::SKILLS_STATUS_NO_REPO), cx);
            return;
        };
        if let Err(err) = open_url(&url) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::SKILLS_STATUS_OPEN_REPO_FAILED, error = err),
                cx,
            );
        }
    }

    fn open_skill_dir(&mut self, directory: &str, cx: &mut Context<Self>) {
        let path = match SkillService::get_ssot_dir() {
            Ok(dir) => dir.join(directory),
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SKILLS_STATUS_SKILL_DIR_FAILED, error = err),
                    cx,
                );
                return;
            }
        };
        if let Err(err) = open_path(&path) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::SKILLS_STATUS_OPEN_DIR_FAILED, error = err),
                cx,
            );
        }
    }

    // ── 发现：市场搜索 / 仓库浏览 / 安装 ───────────────────────────────────

    fn run_market_search(&mut self, append: bool, cx: &mut Context<Self>) {
        let query = self.market_input.read(cx).content().trim().to_string();
        if query.is_empty() {
            // Refused, not failed: nothing was attempted.
            self.set_status(
                NotificationLevel::Warning,
                t(k::SKILLS_STATUS_SEARCH_QUERY_REQUIRED),
                cx,
            );
            return;
        }
        if self.searching_market {
            return;
        }
        self.searching_market = true;
        let offset = if append { self.market_results.len() } else { 0 };
        self.set_status(
            NotificationLevel::Info,
            t(k::SKILLS_STATUS_SEARCHING_MARKET),
            cx,
        );

        let task = Self::spawn_tokio(cx, async move {
            SkillService::search_skills_sh(&query, MARKET_PAGE_SIZE, offset).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.searching_market = false;
                match result {
                    Ok(found) => {
                        this.market_total = found.total_count;
                        if append {
                            this.market_results.extend(found.skills);
                        } else {
                            this.market_results = found.skills;
                        }
                        let message = tf!(
                            k::SKILLS_STATUS_MARKET_FOUND,
                            total = this.market_total,
                            loaded = this.market_results.len(),
                        );
                        this.set_status(NotificationLevel::Success, message, cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_MARKET_SEARCH_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.refresh_list(cx);
            })
            .ok();
        })
        .detach();
    }

    fn discover_skills(&mut self, cx: &mut Context<Self>) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.set_status(NotificationLevel::Info, t(k::SKILLS_STATUS_DISCOVERING), cx);

        let app = self.app.clone();
        let task = Self::spawn_tokio(cx, async move {
            let repos = app
                .db
                .get_skill_repos()
                .map_err(|err| anyhow::anyhow!(err.to_string()))?;
            SkillService::new().discover_available(repos).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.discovering = false;
                match result {
                    Ok(skills) => {
                        let count = skills.len();
                        this.discoverable = skills;
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::SKILLS_STATUS_DISCOVERED, count = count),
                            cx,
                        );
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_DISCOVER_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.refresh_list(cx);
            })
            .ok();
        })
        .detach();
    }

    /// 该目录是否已被某个已安装技能占用；返回 (已安装同源, 目录被其它仓库占用)。
    fn install_state(&self, directory: &str, owner: &str, name: &str) -> (bool, bool) {
        for skill in &self.skills {
            if skill.directory.eq_ignore_ascii_case(directory) {
                let same_repo = skill.repo_owner.as_deref() == Some(owner)
                    && skill.repo_name.as_deref() == Some(name);
                return (same_repo, !same_repo);
            }
        }
        (false, false)
    }

    fn install_discoverable(&mut self, skill: DiscoverableSkill, cx: &mut Context<Self>) {
        if self.installing.contains(&skill.key) {
            return;
        }
        let key = skill.key.clone();
        self.installing.insert(key.clone());
        self.set_status(
            NotificationLevel::Info,
            tf!(k::SKILLS_STATUS_INSTALLING, name = skill.name),
            cx,
        );

        let app = self.app.clone();
        let target_app = self.selected_app;
        let task = Self::spawn_tokio(cx, async move {
            SkillService::new()
                .install(&app.db, &skill, &target_app)
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.installing.remove(&key);
                match result {
                    Ok(installed) => {
                        let message = tf!(
                            k::SKILLS_STATUS_INSTALLED,
                            name = installed.name,
                            app = Self::app_label(target_app),
                        );
                        this.set_status(NotificationLevel::Success, message, cx);
                        this.reload(cx);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SKILLS_STATUS_INSTALL_FAILED, error = err),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install_market(&mut self, skill: &SkillsShDiscoverableSkill, cx: &mut Context<Self>) {
        let discoverable = DiscoverableSkill {
            key: skill.key.clone(),
            name: skill.name.clone(),
            description: String::new(),
            directory: skill.directory.clone(),
            readme_url: skill.readme_url.clone(),
            repo_owner: skill.repo_owner.clone(),
            repo_name: skill.repo_name.clone(),
            repo_branch: skill.repo_branch.clone(),
        };
        self.install_discoverable(discoverable, cx);
    }

    // ── 仓库管理 ────────────────────────────────────────────────────────────

    fn add_repo(&mut self, cx: &mut Context<Self>) {
        // Named `input` rather than `raw`: `i18n::raw` is in scope here.
        let input = self.repo_input.read(cx).content().trim().to_string();
        if input.is_empty() {
            // Refused, not failed: nothing was attempted.
            self.set_status(
                NotificationLevel::Warning,
                t(k::SKILLS_STATUS_REPO_INPUT_REQUIRED),
                cx,
            );
            return;
        }
        let Some(repo) = parse_repo_input(&input) else {
            self.set_status(
                NotificationLevel::Warning,
                t(k::SKILLS_STATUS_REPO_INPUT_INVALID),
                cx,
            );
            return;
        };
        if self
            .repos
            .iter()
            .any(|r| r.owner == repo.owner && r.name == repo.name)
        {
            self.set_status(
                NotificationLevel::Warning,
                tf!(
                    k::SKILLS_STATUS_REPO_EXISTS,
                    owner = repo.owner,
                    name = repo.name,
                ),
                cx,
            );
            return;
        }
        match self.app.db.save_skill_repo(&repo) {
            Ok(()) => {
                let message = tf!(
                    k::SKILLS_STATUS_REPO_ADDED,
                    owner = repo.owner,
                    name = repo.name,
                    branch = repo.branch,
                );
                self.set_status(NotificationLevel::Success, message, cx);
                self.repo_input
                    .update(cx, |input, cx| input.set_content("", cx));
                self.repos = self.app.db.get_skill_repos().unwrap_or_default();
                self.discover_skills(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::SKILLS_STATUS_REPO_ADD_FAILED, error = err),
                cx,
            ),
        }
        self.refresh_list(cx);
    }

    fn toggle_repo(&mut self, owner: String, name: String, cx: &mut Context<Self>) {
        let Some(repo) = self
            .repos
            .iter()
            .find(|r| r.owner == owner && r.name == name)
            .cloned()
        else {
            return;
        };
        let toggled = SkillRepo {
            enabled: !repo.enabled,
            ..repo
        };
        match self.app.db.save_skill_repo(&toggled) {
            Ok(()) => {
                self.repos = self.app.db.get_skill_repos().unwrap_or_default();
                // One whole sentence per branch; the verb is not a fragment
                // that other languages can slot into the same frame.
                let message = if toggled.enabled {
                    tf!(k::SKILLS_STATUS_REPO_ENABLED, owner = owner, name = name)
                } else {
                    tf!(k::SKILLS_STATUS_REPO_DISABLED, owner = owner, name = name)
                };
                self.set_status(NotificationLevel::Success, message, cx);
                self.discover_skills(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::SKILLS_STATUS_REPO_UPDATE_FAILED, error = err),
                cx,
            ),
        }
        self.refresh_list(cx);
    }

    fn do_delete_repo(&mut self, owner: String, name: String, cx: &mut Context<Self>) {
        match self.app.db.delete_skill_repo(&owner, &name) {
            Ok(()) => {
                self.repos = self.app.db.get_skill_repos().unwrap_or_default();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::SKILLS_STATUS_REPO_DELETED, owner = owner, name = name),
                    cx,
                );
                self.discover_skills(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::SKILLS_STATUS_REPO_DELETE_FAILED, error = err),
                cx,
            ),
        }
        self.refresh_list(cx);
    }

    // ── 渲染 ────────────────────────────────────────────────────────────────

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let labels = [
            tf!(k::SKILLS_TAB_INSTALLED, count = self.skills.len()),
            raw(k::SKILLS_TAB_DISCOVER).to_string(),
        ];
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = match self.tab {
            SkillsTab::Installed => 0,
            SkillsTab::Discover => 1,
        };
        let on_select = cx.listener(move |this: &mut Self, ix: &usize, _window, cx| {
            let tab = match ix {
                0 => SkillsTab::Installed,
                _ => SkillsTab::Discover,
            };
            this.set_tab(tab, cx);
        });
        div().flex().flex_row().child(components::segmented(
            "skills-tabs",
            &label_refs,
            selected,
            move |ix, window, cx| on_select(&ix, window, cx),
        ))
    }

    fn render_target_picker(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let apps = Self::skill_apps();
        let labels: Vec<String> = apps
            .iter()
            .map(|app| Self::app_label(*app).to_string())
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = apps
            .iter()
            .position(|app| *app == self.selected_app)
            .unwrap_or(0);
        let on_select = cx.listener(move |this: &mut Self, ix: &usize, _window, cx| {
            if let Some(app) = apps.get(*ix) {
                this.select_app(*app, cx);
            }
        });
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(t(k::SKILLS_TARGET_LABEL)),
            )
            .child(components::segmented(
                SharedString::from(id),
                &label_refs,
                selected,
                move |ix, window, cx| on_select(&ix, window, cx),
            ))
    }

    fn render_stats(&self) -> impl IntoElement {
        let remote_count = self.skills.iter().filter(|s| is_remote(s)).count();
        let tile = |child: gpui::Div| div().flex_1().min_w(px(200.)).child(child);
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_3()
            .w_full()
            .child(tile(components::stat_tile(
                Some(IconName::Blocks),
                theme::accent(),
                t(k::SKILLS_STATS_INSTALLED_LABEL),
                self.skills.len().to_string(),
                t(k::SKILLS_STATS_INSTALLED_DETAIL),
            )))
            .child(tile(components::stat_tile(
                Some(IconName::Cloud),
                theme::teal(),
                t(k::SKILLS_STATS_REMOTE_LABEL),
                remote_count.to_string(),
                t(k::SKILLS_STATS_REMOTE_DETAIL),
            )))
            .child(tile(components::stat_tile(
                Some(IconName::Refresh),
                if self.updates.is_empty() {
                    theme::green()
                } else {
                    theme::yellow()
                },
                t(k::SKILLS_STATS_UPDATES_LABEL),
                if self.checking_updates {
                    raw(k::SKILLS_STATS_UPDATES_CHECKING).to_string()
                } else {
                    match self.updates.len() {
                        0 => raw(k::SKILLS_STATS_UPDATES_LATEST).to_string(),
                        count => tf!(k::SKILLS_STATS_UPDATES_AVAILABLE, count = count),
                    }
                },
                if self.checking_updates {
                    raw(k::SKILLS_STATS_UPDATES_DETAIL_CHECKING)
                } else if self.updates.is_empty() {
                    raw(k::SKILLS_STATS_UPDATES_DETAIL_LATEST)
                } else {
                    raw(k::SKILLS_STATS_UPDATES_DETAIL_AVAILABLE)
                },
            )))
    }

    fn render_installed_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut actions = div().flex().flex_row().items_center().gap_2().flex_none();
        actions = actions.child(
            components::button(
                "skill-check-updates",
                if self.checking_updates {
                    raw(k::SKILLS_INSTALLED_CHECK_UPDATES_BUSY)
                } else {
                    raw(k::SKILLS_INSTALLED_CHECK_UPDATES)
                },
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.check_updates(cx);
            })),
        );
        if !self.updates.is_empty() {
            actions = actions.child(
                components::button(
                    "skill-update-all",
                    if self.updating_all {
                        raw(k::SKILLS_ACTION_UPDATE_BUSY).to_string()
                    } else {
                        tf!(k::SKILLS_INSTALLED_UPDATE_ALL, count = self.updates.len())
                    },
                    if self.updating_all {
                        ButtonTone::Neutral
                    } else {
                        ButtonTone::Primary
                    },
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.update_all(cx);
                })),
            );
        }
        let mut col = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w_full()
                .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                .child(actions),
        );

        let filter = self.last_filter.trim().to_lowercase();
        if self.skills.is_empty() {
            let goto_discover = components::button(
                "skill-empty-discover",
                t(k::SKILLS_INSTALLED_EMPTY_ACTION),
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.set_tab(SkillsTab::Discover, cx);
            }))
            .into_any_element();
            col = col.child(components::empty_state(
                IconName::Blocks,
                t(k::SKILLS_INSTALLED_EMPTY_TITLE),
                t(k::SKILLS_INSTALLED_EMPTY_HINT),
                Some(goto_discover),
            ));
        } else if self.installed_indices(&filter).is_empty() {
            col = col.child(components::empty_state(
                IconName::Search,
                t(k::SKILLS_INSTALLED_NO_MATCHES_TITLE),
                t(k::SKILLS_INSTALLED_NO_MATCHES_HINT),
                None,
            ));
        }
        col
    }

    fn render_app_toggles(&self, skill: &InstalledSkill, cx: &mut Context<Self>) -> gpui::Div {
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(t(k::SKILLS_CARD_SYNC_TO)),
            );
        for app_type in Self::skill_apps() {
            let enabled = skill.apps.is_enabled_for(&app_type);
            let key = format!("{}:{}", skill.id, app_type.as_str());
            let busy = self.toggling.contains(&key);
            let skill_id = skill.id.clone();
            let label = Self::app_label(app_type);
            let chip = div()
                .id(SharedString::from(format!("skill-app-{key}")))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(tf!(
                    k::SKILLS_CARD_APP_TOGGLE_ARIA,
                    app = label
                )))
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .gap_1()
                .px_2()
                .py(px(3.))
                .rounded_full()
                .border_1()
                .border_color(if enabled {
                    theme::accent().alpha(0.4)
                } else {
                    theme::border()
                })
                .bg(if enabled {
                    theme::accent_soft()
                } else {
                    theme::inset()
                })
                .text_xs()
                .text_color(if busy {
                    theme::muted()
                } else if enabled {
                    theme::accent()
                } else {
                    theme::muted()
                })
                .cursor_pointer()
                .hover(|s| s.border_color(theme::accent().alpha(0.6)))
                .child(if busy {
                    SharedString::from(tf!(k::SKILLS_CARD_APP_TOGGLE_BUSY, app = label))
                } else if enabled {
                    SharedString::from(tf!(k::SKILLS_CARD_APP_TOGGLE_ENABLED, app = label))
                } else {
                    label.clone()
                })
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.toggle_skill_app(skill_id.clone(), app_type, !enabled, cx);
                }));
            row = row.child(chip);
        }
        row
    }

    fn render_installed_card(
        &self,
        skill: &InstalledSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let confirm_target = ConfirmAction::Uninstall {
            id: skill.id.clone(),
            name: skill.name.clone(),
        };
        let update_id = skill.id.clone();
        let update = self.updates.get(&skill.id).cloned();
        let is_updating = self.updating.contains(&skill.id);
        let is_uninstalling = self.uninstalling.contains(&skill.id);
        let remote = is_remote(skill);
        let repo_skill = skill.clone();
        let dir_for_open = skill.directory.clone();

        let mut meta = vec![Self::source_label(skill)];
        if let Some(installed_at) = format_ts(skill.installed_at) {
            meta.push(tf!(k::SKILLS_CARD_INSTALLED_AT, time = installed_at));
        }
        if let Some(updated_at) = format_ts(skill.updated_at) {
            meta.push(tf!(k::SKILLS_CARD_UPDATED_AT, time = updated_at));
        }

        components::card()
            .gap_3()
            .border_color(if update.is_some() {
                theme::yellow()
            } else {
                theme::border()
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(skill.name.clone())),
                            )
                            .child(components::badge(
                                if remote {
                                    BadgeTone::Teal
                                } else {
                                    BadgeTone::Neutral
                                },
                                if remote {
                                    raw(k::SKILLS_BADGE_REMOTE)
                                } else {
                                    raw(k::SKILLS_BADGE_LOCAL)
                                },
                            ))
                            .when(update.is_some(), |s| {
                                s.child(components::badge(
                                    BadgeTone::Warning,
                                    t(k::SKILLS_BADGE_UPDATE_AVAILABLE),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .flex_none()
                            .when(remote, |s| {
                                s.child(
                                    components::button(
                                        SharedString::from(format!("skill-repo-{}", skill.id)),
                                        t(k::SKILLS_ACTION_REPO),
                                        ButtonTone::Ghost,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.open_repo_page(&repo_skill, cx);
                                        },
                                    )),
                                )
                            })
                            .child(
                                components::button(
                                    SharedString::from(format!("skill-dir-{}", skill.id)),
                                    t(k::SKILLS_ACTION_DIRECTORY),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.open_skill_dir(&dir_for_open, cx);
                                    },
                                )),
                            )
                            .when(update.is_some(), |s| {
                                s.child(
                                    components::button(
                                        SharedString::from(format!("skill-update-{}", skill.id)),
                                        if is_updating {
                                            raw(k::SKILLS_ACTION_UPDATE_BUSY)
                                        } else {
                                            raw(k::SKILLS_ACTION_UPDATE)
                                        },
                                        if is_updating {
                                            ButtonTone::Neutral
                                        } else {
                                            ButtonTone::Primary
                                        },
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.update_skill(update_id.clone(), cx);
                                        },
                                    )),
                                )
                            })
                            .child(
                                components::button(
                                    SharedString::from(format!("skill-uninstall-{}", skill.id)),
                                    if is_uninstalling {
                                        raw(k::SKILLS_ACTION_UNINSTALL_BUSY)
                                    } else {
                                        raw(k::SKILLS_ACTION_UNINSTALL)
                                    },
                                    if is_uninstalling {
                                        ButtonTone::Neutral
                                    } else {
                                        ButtonTone::Danger
                                    },
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.confirm = Some(confirm_target.clone());
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .text_color(theme::muted())
                    .text_xs()
                    .truncate()
                    .child(SharedString::from(meta.join(" · "))),
            )
            .when_some(skill.description.clone(), |s, d| {
                s.child(
                    div()
                        .w_full()
                        .text_color(theme::subtext())
                        .text_xs()
                        .line_clamp(2)
                        .child(SharedString::from(d)),
                )
            })
            .when_some(update, |s, update| {
                s.child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(SharedString::from(tf!(
                            k::SKILLS_CARD_HASH_COMPARE,
                            local = update
                                .current_hash
                                .as_deref()
                                .map(short_hash)
                                .unwrap_or_else(|| raw(k::SKILLS_CARD_HASH_UNKNOWN).to_string()),
                            remote = short_hash(&update.remote_hash),
                        ))),
                )
            })
            .child(self.render_app_toggles(skill, cx))
    }

    fn render_discover_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_selected = match self.discover_mode {
            DiscoverMode::Market => 0,
            DiscoverMode::Repos => 1,
        };
        let on_mode = cx.listener(move |this: &mut Self, ix: &usize, _window, cx| {
            this.set_discover_mode(
                if *ix == 0 {
                    DiscoverMode::Market
                } else {
                    DiscoverMode::Repos
                },
                cx,
            );
        });
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .child(components::segmented(
                "skills-discover-mode",
                &[
                    raw(k::SKILLS_DISCOVER_MODE_MARKET),
                    raw(k::SKILLS_DISCOVER_MODE_REPOS),
                ],
                mode_selected,
                move |ix, window, cx| on_mode(&ix, window, cx),
            ))
            .child(self.render_target_picker("skills-target-discover", cx))
    }

    fn render_market_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                t(k::SKILLS_MARKET_SECTION_TITLE),
                t(k::SKILLS_MARKET_SECTION_DESC),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(div().flex_1().min_w_0().child(self.market_input.clone()))
                    .child(
                        components::button(
                            "skill-market-search",
                            if self.searching_market {
                                raw(k::SKILLS_MARKET_SEARCH_BUSY)
                            } else {
                                raw(k::SKILLS_MARKET_SEARCH)
                            },
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.run_market_search(false, cx);
                            },
                        )),
                    ),
            );
        if self.market_results.is_empty() {
            col = col.child(components::empty_state(
                IconName::Search,
                t(k::SKILLS_MARKET_EMPTY_TITLE),
                t(k::SKILLS_MARKET_EMPTY_HINT),
                None,
            ));
        }
        col
    }

    fn render_market_card(
        &self,
        skill: &SkillsShDiscoverableSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let installing = self.installing.contains(&skill.key);
        let (already, conflict) =
            self.install_state(&skill.directory, &skill.repo_owner, &skill.repo_name);
        let install_skill = skill.clone();
        let readme = skill.readme_url.clone();
        components::card().gap_2().child(
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
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(SharedString::from(skill.name.clone())),
                                )
                                .child(components::badge(
                                    BadgeTone::Accent,
                                    tf!(k::SKILLS_MARKET_INSTALLS, count = skill.installs),
                                )),
                        )
                        .child(div().text_color(theme::muted()).text_xs().truncate().child(
                            SharedString::from(format!(
                                "{}/{} · {}",
                                skill.repo_owner, skill.repo_name, skill.directory
                            )),
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_none()
                        .when_some(readme, |s, url| {
                            s.child(
                                components::button(
                                    SharedString::from(format!("skill-market-repo-{}", skill.key)),
                                    t(k::SKILLS_ACTION_REPO),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        if let Err(err) = open_url(&url) {
                                            this.set_status(
                                                NotificationLevel::Error,
                                                tf!(k::SKILLS_STATUS_OPEN_REPO_FAILED, error = err),
                                                cx,
                                            );
                                        }
                                    },
                                )),
                            )
                        })
                        .child(
                            components::button(
                                SharedString::from(format!("skill-market-install-{}", skill.key)),
                                if already {
                                    raw(k::SKILLS_ACTION_INSTALL_INSTALLED)
                                } else if conflict {
                                    raw(k::SKILLS_ACTION_INSTALL_CONFLICT)
                                } else if installing {
                                    raw(k::SKILLS_ACTION_INSTALL_BUSY)
                                } else {
                                    raw(k::SKILLS_ACTION_INSTALL)
                                },
                                if already || conflict || installing {
                                    ButtonTone::Neutral
                                } else {
                                    ButtonTone::Primary
                                },
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    let (already, conflict) = this.install_state(
                                        &install_skill.directory,
                                        &install_skill.repo_owner,
                                        &install_skill.repo_name,
                                    );
                                    if !already && !conflict {
                                        this.install_market(&install_skill, cx);
                                    }
                                },
                            )),
                        ),
                ),
        )
    }

    fn render_market_more(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_row().justify_center().w_full().child(
            components::button(
                "skill-market-more",
                if self.searching_market {
                    raw(k::SKILLS_MARKET_MORE_BUSY).to_string()
                } else {
                    tf!(
                        k::SKILLS_MARKET_MORE,
                        loaded = self.market_results.len(),
                        total = self.market_total,
                    )
                },
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.run_market_search(true, cx);
            })),
        )
    }

    fn render_repo_manager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for repo in &self.repos {
            let owner = repo.owner.clone();
            let name = repo.name.clone();
            let toggle_owner = owner.clone();
            let toggle_name = name.clone();
            let enabled = repo.enabled;
            rows.push(
                layout::row()
                    .child(layout::row_label(
                        format!("{}/{}", repo.owner, repo.name),
                        tf!(k::SKILLS_REPO_BRANCH, branch = repo.branch),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!("repo-toggle-{owner}-{name}")))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.toggle_repo(toggle_owner.clone(), toggle_name.clone(), cx);
                            }))
                            .child(layout::toggle(enabled)),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("repo-delete-{owner}-{name}")),
                            t(k::SKILLS_ACTION_DELETE),
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(ConfirmAction::DeleteRepo {
                                    owner: owner.clone(),
                                    name: name.clone(),
                                });
                                cx.notify();
                            },
                        )),
                    )
                    .into_any_element(),
            );
        }

        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                t(k::SKILLS_REPO_SECTION_TITLE),
                t(k::SKILLS_REPO_SECTION_DESC),
            ));
        if !rows.is_empty() {
            col = col.child(layout::group(rows));
        }
        col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w_full()
                .child(div().flex_1().min_w_0().child(self.repo_input.clone()))
                .child(
                    components::button(
                        "skill-repo-add",
                        t(k::SKILLS_REPO_ADD),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.add_repo(cx);
                    })),
                )
                .child(
                    components::button(
                        "skill-repo-discover",
                        if self.discovering {
                            raw(k::SKILLS_REPO_DISCOVER_BUSY)
                        } else {
                            raw(k::SKILLS_REPO_DISCOVER)
                        },
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.discover_skills(cx);
                    })),
                ),
        )
    }

    fn render_repo_results_header(&self) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                t(k::SKILLS_REPO_RESULTS_TITLE),
                tf!(k::SKILLS_REPO_RESULTS_DESC, count = self.discoverable.len()),
            ));
        if self.discoverable.is_empty() {
            col = col.child(components::empty_state(
                IconName::Cloud,
                if self.discovering {
                    raw(k::SKILLS_REPO_EMPTY_DISCOVERING_TITLE)
                } else {
                    raw(k::SKILLS_REPO_EMPTY_NONE_TITLE)
                },
                if self.discovering {
                    raw(k::SKILLS_REPO_EMPTY_DISCOVERING_HINT)
                } else {
                    raw(k::SKILLS_REPO_EMPTY_NONE_HINT)
                },
                None,
            ));
        }
        col
    }

    fn render_discoverable_card(
        &self,
        skill: &DiscoverableSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let install_skill = skill.clone();
        let installing = self.installing.contains(&skill.key);
        let (already, conflict) =
            self.install_state(&skill.directory, &skill.repo_owner, &skill.repo_name);
        components::card()
            .gap_2()
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
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(skill.name.clone())),
                            )
                            .child(div().text_color(theme::muted()).text_xs().truncate().child(
                                SharedString::from(format!(
                                    "{}/{}@{} · {}",
                                    skill.repo_owner,
                                    skill.repo_name,
                                    skill.repo_branch,
                                    skill.directory
                                )),
                            )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("skill-install-{}", skill.key)),
                            if already {
                                raw(k::SKILLS_ACTION_INSTALL_INSTALLED)
                            } else if conflict {
                                raw(k::SKILLS_ACTION_INSTALL_CONFLICT)
                            } else if installing {
                                raw(k::SKILLS_ACTION_INSTALL_BUSY)
                            } else {
                                raw(k::SKILLS_ACTION_INSTALL)
                            },
                            if already || conflict || installing {
                                ButtonTone::Neutral
                            } else {
                                ButtonTone::Primary
                            },
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                let (already, conflict) = this.install_state(
                                    &install_skill.directory,
                                    &install_skill.repo_owner,
                                    &install_skill.repo_name,
                                );
                                if !already && !conflict {
                                    this.install_discoverable(install_skill.clone(), cx);
                                }
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .text_color(theme::subtext())
                    .text_xs()
                    .line_clamp(2)
                    .child(SharedString::from(skill.description.clone())),
            )
    }

    #[cfg(any())]
    fn render_import_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut actions = div().flex().flex_row().flex_wrap().items_center().gap_2();
        actions = actions.child(
            components::button(
                "skill-install-zip",
                if self.installing_zip {
                    "安装中..."
                } else {
                    "选择 ZIP 安装"
                },
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.pick_and_install_zip(cx);
            })),
        );
        actions = actions.child(
            components::button(
                "skill-scan-unmanaged",
                if self.scanning {
                    "扫描中..."
                } else {
                    "重新扫描"
                },
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.scan_unmanaged(cx);
            })),
        );
        if !self.unmanaged.is_empty() {
            actions = actions.child(
                components::button(
                    "skill-import-all",
                    if self.importing_all {
                        "导入中...".to_string()
                    } else {
                        format!("全部导入 ({})", self.unmanaged.len())
                    },
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.import_all_unmanaged(cx);
                })),
            );
        }

        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                "导入技能",
                "从本地 ZIP 安装，或把应用目录中已有的技能纳入统一管理。",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .child(actions)
                    .child(self.render_target_picker("skills-target-import", cx)),
            );
        if self.unmanaged.is_empty() && !self.scanning {
            col = col.child(components::empty_state(
                IconName::Search,
                "没有未管理技能",
                "各应用目录中的技能都已纳入管理。",
                None,
            ));
        }
        col
    }

    #[cfg(any())]
    fn render_unmanaged_row(
        &self,
        skill: &UnmanagedSkill,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let import_skill = skill.clone();
        let importing = self.importing.contains(&skill.directory);
        let target_apps = self.unmanaged_target_apps(&skill.found_in);
        let target_labels: Vec<String> = target_apps
            .enabled_apps()
            .iter()
            .map(|app| Self::app_label(*app).to_string())
            .collect();
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
                    .flex_1()
                    .child(
                        div()
                            .text_color(theme::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(SharedString::from(skill.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(skill.path.clone())),
                    )
                    .child(div().text_color(theme::teal()).text_xs().truncate().child(
                        SharedString::from(format!("导入后启用：{}", target_labels.join("、"))),
                    )),
            )
            .child(
                components::button(
                    SharedString::from(format!("skill-import-{}", skill.directory)),
                    if importing { "导入中..." } else { "导入" },
                    if importing {
                        ButtonTone::Neutral
                    } else {
                        ButtonTone::Primary
                    },
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.import_unmanaged(&import_skill, cx);
                })),
            )
    }

    #[cfg(any())]
    fn render_backup_header(&self) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                "卸载备份",
                "卸载技能时自动创建，最多保留 20 份；恢复时会重新启用原来的应用。",
            ));
        if self.backups.is_empty() {
            col = col.child(components::empty_state(
                IconName::Archive,
                "暂无备份",
                "卸载技能时会自动在这里生成备份。",
                None,
            ));
        }
        col
    }

    #[cfg(any())]
    fn render_backup_row(
        &self,
        backup: &SkillBackupEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let restore_id = backup.backup_id.clone();
        let restoring = self.restoring.contains(&backup.backup_id);
        let confirm_target = ConfirmAction::DeleteBackup {
            id: backup.backup_id.clone(),
            name: backup.skill.name.clone(),
        };
        let apps_labels: Vec<String> = backup
            .skill
            .apps
            .enabled_apps()
            .iter()
            .map(|app| Self::app_label(*app).to_string())
            .collect();
        let mut meta = vec![Self::source_label(&backup.skill)];
        if let Some(created) = format_ts(backup.created_at) {
            meta.push(format!("备份于 {created}"));
        }
        if !apps_labels.is_empty() {
            meta.push(format!("原启用：{}", apps_labels.join("、")));
        }
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
                    .flex_1()
                    .child(
                        div()
                            .text_color(theme::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(SharedString::from(backup.skill.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(meta.join(" · "))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            SharedString::from(format!(
                                "skill-backup-restore-{}",
                                backup.backup_id
                            )),
                            if restoring { "恢复中..." } else { "恢复" },
                            if restoring {
                                ButtonTone::Neutral
                            } else {
                                ButtonTone::Primary
                            },
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.restore_backup(restore_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("skill-backup-delete-{}", backup.backup_id)),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(confirm_target.clone());
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
}

impl Render for SkillsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_auto_check_updates(cx);
        let filter = self.last_filter.trim().to_lowercase();
        let mut plan: Vec<SkillRow> = vec![SkillRow::Tabs];
        match self.tab {
            SkillsTab::Installed => {
                plan.push(SkillRow::Stats);
                plan.push(SkillRow::InstalledToolbar);
                plan.extend(
                    self.installed_indices(&filter)
                        .into_iter()
                        .map(SkillRow::Installed),
                );
            }
            SkillsTab::Discover => {
                plan.push(SkillRow::DiscoverToolbar);
                match self.discover_mode {
                    DiscoverMode::Market => {
                        plan.push(SkillRow::MarketBar);
                        plan.extend((0..self.market_results.len()).map(SkillRow::Market));
                        if self.market_results.len() < self.market_total {
                            plan.push(SkillRow::MarketMore);
                        }
                    }
                    DiscoverMode::Repos => {
                        plan.push(SkillRow::RepoManager);
                        plan.push(SkillRow::RepoResultsHeader);
                        plan.extend((0..self.discoverable.len()).map(SkillRow::Discoverable));
                    }
                }
            }
        }
        if self.list_state.item_count() != plan.len() {
            self.list_state.reset(plan.len());
        }

        let list = gpui::list(
            self.list_state.clone(),
            cx.processor(move |this, ix: usize, _window, cx| {
                // 每行自带 pb_3 作为行距（list 本身不画间距）。
                let block = div().w_full().pb_3();
                match plan.get(ix).copied() {
                    Some(SkillRow::Tabs) => block.child(this.render_tabs(cx)).into_any_element(),
                    Some(SkillRow::Stats) => block.child(this.render_stats()).into_any_element(),
                    Some(SkillRow::InstalledToolbar) => block
                        .child(this.render_installed_toolbar(cx))
                        .into_any_element(),
                    Some(SkillRow::Installed(six)) => match this.skills.get(six) {
                        Some(skill) => {
                            let skill = skill.clone();
                            block
                                .child(this.render_installed_card(&skill, cx))
                                .into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    },
                    Some(SkillRow::DiscoverToolbar) => block
                        .child(this.render_discover_toolbar(cx))
                        .into_any_element(),
                    Some(SkillRow::MarketBar) => {
                        block.child(this.render_market_bar(cx)).into_any_element()
                    }
                    Some(SkillRow::Market(mix)) => match this.market_results.get(mix) {
                        Some(skill) => {
                            let skill = skill.clone();
                            block
                                .child(this.render_market_card(&skill, cx))
                                .into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    },
                    Some(SkillRow::MarketMore) => {
                        block.child(this.render_market_more(cx)).into_any_element()
                    }
                    Some(SkillRow::RepoManager) => {
                        block.child(this.render_repo_manager(cx)).into_any_element()
                    }
                    Some(SkillRow::RepoResultsHeader) => block
                        .child(this.render_repo_results_header())
                        .into_any_element(),
                    Some(SkillRow::Discoverable(dix)) => match this.discoverable.get(dix) {
                        Some(skill) => {
                            let skill = skill.clone();
                            block
                                .child(this.render_discoverable_card(&skill, cx))
                                .into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    },
                    None => gpui::Empty.into_any_element(),
                }
            }),
        );

        layout::page()
            .relative()
            .child(layout::page_header(
                t(k::SKILLS_HEADER_TITLE),
                Some(t(k::SKILLS_HEADER_SUBTITLE)),
            ))
            .child(layout::virtual_body(
                "skills-list-body",
                list,
                &self.list_state,
            ))
            .when_some(self.confirm.clone(), |root, action| {
                let (title, message, confirm_label) = match &action {
                    ConfirmAction::Uninstall { name, .. } => (
                        raw(k::SKILLS_CONFIRM_UNINSTALL_TITLE),
                        tf!(k::SKILLS_CONFIRM_UNINSTALL_MESSAGE, name = name),
                        raw(k::SKILLS_ACTION_UNINSTALL),
                    ),
                    ConfirmAction::DeleteRepo { owner, name } => (
                        raw(k::SKILLS_CONFIRM_DELETE_REPO_TITLE),
                        tf!(
                            k::SKILLS_CONFIRM_DELETE_REPO_MESSAGE,
                            owner = owner,
                            name = name,
                        ),
                        raw(k::SKILLS_ACTION_DELETE),
                    ),
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(title))
                        .child(
                            components::modal_body().child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(message)),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "skill-confirm-cancel",
                                t(k::SKILLS_ACTION_CANCEL),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "skill-confirm-ok",
                                confirm_label,
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm = None;
                                match &action {
                                    ConfirmAction::Uninstall { id, .. } => {
                                        this.do_uninstall(id.clone(), cx)
                                    }
                                    ConfirmAction::DeleteRepo { owner, name } => {
                                        this.do_delete_repo(owner.clone(), name.clone(), cx)
                                    }
                                }
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

fn is_remote(skill: &InstalledSkill) -> bool {
    skill.repo_owner.is_some() && skill.repo_name.is_some()
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Unix 秒时间戳 → 本地时间字符串；无效（<=0）返回 None。
fn format_ts(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(ts, 0).map(|dt| {
        dt.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
}

/// 解析仓库输入：`owner/repo`、`owner/repo@branch`、GitHub 链接
/// （含 `/tree/<branch>` 形式）。
fn parse_repo_input(raw: &str) -> Option<SkillRepo> {
    let mut rest = raw.trim();
    for prefix in ["https://github.com/", "http://github.com/", "github.com/"] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped;
            break;
        }
    }
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");

    let (path, mut branch) = match rest.split_once('@') {
        Some((path, branch)) => (path, Some(branch.trim().to_string())),
        None => (rest, None),
    };
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let owner = segments.next()?.to_string();
    let name = segments.next()?.to_string();
    // GitHub 链接的 /tree/<branch> 形式。
    let tail: Vec<&str> = segments.collect();
    if branch.is_none() && tail.len() >= 2 && tail[0] == "tree" {
        branch = Some(tail[1].to_string());
    }
    if owner.is_empty() || name.is_empty() || owner.contains('.') {
        return None;
    }
    Some(SkillRepo {
        owner,
        name,
        branch: branch
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "main".to_string()),
        enabled: true,
    })
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    cmd.spawn().map(|_| ()).map_err(|err| err.to_string())
}

fn open_path(path: &Path) -> Result<(), String> {
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

    cmd.spawn().map(|_| ()).map_err(|err| err.to_string())
}

crate::notifications::impl_status_toasts_leveled!(SkillsView);
