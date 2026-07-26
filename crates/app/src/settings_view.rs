//! Device-level settings.
//!
//! # Shape
//!
//! Three pages, not one scroll. The **root** page is six groups of
//! purpose-built rows ([`layout::switch_row`], [`select_row`](layout::select_row),
//! [`navigate_row`](layout::navigate_row), [`action_row`](layout::action_row)) —
//! it holds no text input and no Save button, and every row writes the moment
//! it is operated. Two sub-pages hang off it: **apps** (one switch per plugin)
//! and **sync** (the app's only batched form, ending in a
//! [`components::commit_bar`]).
//!
//! # The commit rule
//!
//! > The root page commits immediately and has no Save anywhere. The one form
//! > in the app lives on a sub-page and owns exactly one commit bar. The two
//! > never appear on the same screen.
//!
//! That is the rule [`crate::components::commit_bar`] already states as
//! doctrine, and after the native directory picker and the preset selects it is
//! enforceable by construction: the root page has no text inputs at all, and
//! the sync sub-page has no immediately-applied control at all. The destination
//! selector and the auto-sync switch live on the *root* page precisely so the
//! sub-page's bar is unambiguously page-scoped.
//!
//! # Writes
//!
//! Every write goes through [`settings::mutate_settings`], which
//! read-modify-writes under the lock. `update_settings(self.settings.clone())`
//! is banned here: `self.settings` is a display snapshot, and the sync services
//! write `status` into the same struct while a sync runs, so writing the
//! snapshot back clobbers them.

mod apps;
mod options;
mod root;
mod rows;
mod search;
mod sync;

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    div, point, prelude::*, px, Context, Entity, FontWeight, ListAlignment, ListState,
    ScrollHandle, SharedString, Window,
};
use ochub_core::db::import_ccswitch::{self, DetectedSource};
use ochub_core::plugin::{AppPlugin, ManifestLoadError};
use ochub_core::settings::{self, AppSettings};
use ochub_core::AppState;

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, t};
use crate::icons::IconName;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

use search::RowId;
use sync::{SyncDraft, SyncTarget};

/// Events the settings view emits for the app shell.
pub enum SettingsEvent {
    /// The set of enabled apps changed (sidebar/views must re-derive).
    AppsChanged,
    /// The interface language changed. Repainting covers rendered text, but
    /// strings captured at construction time (text-input placeholders) and
    /// memoized list-item heights have to be refreshed explicitly.
    LocaleChanged,
    /// A cc-switch import rewrote providers, MCP servers and skill repos. The
    /// shell has to re-read all of them; nothing here can do that for it.
    DataImported,
}

impl gpui::EventEmitter<SettingsEvent> for SettingsView {}

/// Which of the three pages is on screen. Sub-pages are reached only through
/// their parent [`layout::navigate_row`], so a text field is never operated
/// outside the form whose commit bar owns it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Root,
    Apps,
    Sync,
}

/// A pending confirmation. All three are modal because all three are
/// irreversible from the user's point of view: two rewrite database records,
/// the other throws away typing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirm {
    Restore,
    DiscardDraft,
    /// Importing cc-switch data over an install that already has records.
    CcswitchImport,
}

pub struct SettingsView {
    app: Arc<AppState>,
    /// Display cache. Written only by `= settings::get_settings()`, never read
    /// back into the store — see the module doc.
    settings: AppSettings,
    plugins: Arc<[Arc<dyn AppPlugin>]>,
    plugin_load_errors: Arc<[ManifestLoadError]>,
    page: Page,
    /// The six root blocks, or one block when a search query is active.
    root_list: ListState,
    /// The three apps blocks. The plugin count is unbounded, so this page keeps
    /// virtualization.
    apps_list: ListState,
    sync_scroll: ScrollHandle,
    search: Entity<TextInput>,
    query: SharedString,
    /// At most one adaptive select row may have its dropdown open.
    open_select_row: Option<RowId>,
    /// Per-app in-flight set, so only the row being toggled goes inert.
    toggling: HashSet<String>,
    /// `Some` only while `page == Page::Sync`.
    draft: Option<SyncDraft>,
    sync_busy: bool,
    /// Serializes settings-file and OS integration writes without blocking the
    /// GPUI thread.
    settings_busy: bool,
    /// The login-item error belongs to two specific rows, so it renders inline
    /// in the startup group rather than as a toast. Stores the raw OS message;
    /// the sentence around it is built at paint time.
    startup_error: Option<SharedString>,
    /// The "both destinations enabled" warning is shown once per process.
    warned_dual_target: bool,
    pending_nav: Option<Page>,
    confirm: Option<Confirm>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    /// cc-switch data found on disk, or `None` when there is nothing to import.
    /// The row only exists while this does — offering an import with no source
    /// would be a permanently dead control.
    ccswitch_source: Option<DetectedSource>,
    ccswitch_busy: bool,
}

/// Root blocks when no search query is active. 关于 is its own section
/// now (see `crate::about_view`), so the root stops at the sync group.
const ROOT_BLOCK_COUNT: usize = 5;
/// Apps sub-page blocks: enabled apps, manifest errors, user plugins.
const APPS_BLOCK_COUNT: usize = 3;

impl SettingsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, t(k::SETTINGS_SEARCH_PLACEHOLDER)).compact());
        let mut this = Self {
            app,
            settings: settings::get_settings(),
            plugins: ochub_core::plugin::all_plugins_snapshot(),
            plugin_load_errors: ochub_core::plugin::manifest_load_errors().into(),
            page: Page::Root,
            root_list: ListState::new(ROOT_BLOCK_COUNT, ListAlignment::Top, px(600.)),
            apps_list: ListState::new(APPS_BLOCK_COUNT, ListAlignment::Top, px(600.)),
            sync_scroll: ScrollHandle::new(),
            search,
            query: SharedString::default(),
            open_select_row: None,
            toggling: HashSet::new(),
            draft: None,
            sync_busy: false,
            settings_busy: false,
            startup_error: None,
            warned_dual_target: false,
            pending_nav: None,
            confirm: None,
            status: None,
            status_level: None,
            ccswitch_source: None,
            ccswitch_busy: false,
        };

        // Filtering re-lays the list; ignore the notifications that carry no
        // content change (caret blink, focus).
        cx.observe(&this.search, |this, input, cx| {
            let content = input.read(cx).content();
            if content != this.query {
                this.query = content;
                this.open_select_row = None;
                this.root_list.reset(this.root_block_count());
                cx.notify();
            }
        })
        .detach();

        this.detect_ccswitch_source(cx);
        this
    }

    fn detect_ccswitch_source(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let source = cx
                .background_spawn(async {
                    import_ccswitch::detect_source().filter(|source| !source.is_empty())
                })
                .await;
            this.update(cx, |this, cx| {
                this.ccswitch_source = source;
                this.root_list.reset(this.root_block_count());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the store so the page cannot show state a background write or a
    /// restore has already replaced.
    ///
    /// A clean draft is re-seeded from the fresh values; a dirty one is left
    /// exactly as typed. That can never collide with a restore, because
    /// 还原 is disabled while the draft is dirty.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.settings = settings::get_settings();
        let reseed = self.draft.is_some() && !self.draft_dirty(cx);
        if reseed {
            self.reseed_draft(cx);
        }
        self.warn_if_dual_target(cx);
        self.root_list.remeasure();
        self.apps_list.remeasure();
        cx.notify();
    }

    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, and a
    /// `TextInput` captures its placeholder when it is constructed.
    ///
    /// Field errors need nothing here: they are stored as an enum and resolved
    /// through `t()` at paint time, so a language change re-renders them
    /// correctly without this method having to remember any of them.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(t(k::SETTINGS_SEARCH_PLACEHOLDER), cx)
        });
        self.relocalize_draft(cx);
        self.root_list.remeasure();
        self.apps_list.remeasure();
        cx.notify();
    }

    /// The current page *is* the answer to "save what?", so the bell only ever
    /// means "nothing here to save" — never "I guessed which group you meant
    /// from where the caret is and got it wrong".
    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            window.play_system_bell();
            return;
        }
        match self.page {
            Page::Sync if self.draft_dirty(cx) && !self.draft_saving() => self.save_sync(cx),
            _ => window.play_system_bell(),
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Esc while the discard modal is up means "keep editing", and that is
        // an answer rather than a dead end — no bell.
        if self.confirm.take().is_some() {
            self.pending_nav = None;
            cx.notify();
            return;
        }
        if self.open_select_row.take().is_some() {
            cx.notify();
            return;
        }
        if self.page == Page::Root && !self.query.is_empty() {
            self.clear_search(cx);
            return;
        }
        if self.page != Page::Root {
            self.go(Page::Root, cx);
            return;
        }
        window.play_system_bell();
    }

    /// The only navigation entry point. Leaving a dirty draft parks the request
    /// and asks first, which closes the "type the WebDAV password, navigate
    /// away, lose it silently" path.
    pub(crate) fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == Page::Sync && page != Page::Sync && self.draft_dirty(cx) {
            self.pending_nav = Some(page);
            self.confirm = Some(Confirm::DiscardDraft);
            cx.notify();
            return;
        }
        self.enter(page, cx);
    }

    fn enter(&mut self, page: Page, cx: &mut Context<Self>) {
        self.open_select_row = None;
        if page != Page::Sync {
            self.close_sync();
        } else if self.draft.is_none() {
            self.open_sync(cx);
        }
        self.page = page;
        match page {
            Page::Root => self.root_list.reset(self.root_block_count()),
            Page::Apps => self.apps_list.reset(APPS_BLOCK_COUNT),
            Page::Sync => self.sync_scroll.set_offset(point(px(0.), px(0.))),
        }
        cx.notify();
    }

    fn root_block_count(&self) -> usize {
        if self.query.trim().is_empty() {
            ROOT_BLOCK_COUNT
        } else {
            1
        }
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search
            .update(cx, |input, cx| input.set_content("", cx));
        self.query = SharedString::default();
        self.root_list.reset(ROOT_BLOCK_COUNT);
        cx.notify();
    }

    /// Every status toast carries its severity explicitly. Inferring it from
    /// the wording mis-reads several of these messages and stops working
    /// entirely once the copy is translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(text.into());
        self.status_level = Some(level);
        cx.notify();
    }

    fn write(
        &mut self,
        mutator: impl FnOnce(&mut AppSettings) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.write_then(mutator, |_this, _cx| {}, cx);
    }

    /// One targeted read-modify-write off the UI thread, followed by an
    /// authoritative re-read and an optional UI-side effect.
    fn write_then(
        &mut self,
        mutator: impl FnOnce(&mut AppSettings) + Send + 'static,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.settings_busy {
            return;
        }
        self.settings_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    settings::mutate_settings(mutator).map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.settings_busy = false;
                match result {
                    Ok(()) => {
                        this.reload(cx);
                        on_success(this, cx);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_STATUS_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    /// A sub-page header: back button, then the same title/subtitle column
    /// [`layout::page_header`] draws. Esc returns as well.
    fn sub_page_header(
        &self,
        title: SharedString,
        subtitle: SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_6()
            .py_4()
            .border_b_1()
            .border_color(theme::border())
            .child(
                components::icon_button_tone(
                    "settings-back",
                    t(k::SETTINGS_BACK_LABEL),
                    IconName::ChevronLeft,
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.go(Page::Root, cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text())
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(subtitle),
                    ),
            )
    }

    fn render_confirm(&self, confirm: Confirm, cx: &mut Context<Self>) -> gpui::Div {
        match confirm {
            Confirm::Restore => self.render_restore_confirm(cx),
            Confirm::DiscardDraft => self.render_discard_confirm(cx),
            Confirm::CcswitchImport => self.render_ccswitch_confirm(cx),
        }
    }

    fn render_ccswitch_confirm(&self, cx: &mut Context<Self>) -> gpui::Div {
        let path = self
            .ccswitch_source
            .as_ref()
            .map(|source| ochub_core::paths::abbreviate_home(&source.path))
            .unwrap_or_default();
        components::modal_overlay(
            components::modal_card()
                .child(components::modal_header(t(k::SETTINGS_DATA_CCSWITCH_LABEL)))
                .child(
                    components::modal_body()
                        .child(
                            div()
                                .text_color(theme::subtext())
                                .text_sm()
                                .child(t(k::SETTINGS_DATA_CCSWITCH_CONFIRM_BODY)),
                        )
                        .child(div().text_color(theme::muted()).text_xs().child(path)),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "settings-ccswitch-cancel",
                        t(k::SETTINGS_ACTION_CANCEL),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                    components::button(
                        "settings-ccswitch-ok",
                        t(k::SETTINGS_DATA_CCSWITCH_ACTION),
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        this.start_ccswitch_import(cx);
                    }))
                    .into_any_element(),
                ])),
        )
    }

    fn render_restore_confirm(&self, cx: &mut Context<Self>) -> gpui::Div {
        let provider = self
            .sync_target()
            .map(SyncTarget::provider)
            .unwrap_or_default();
        components::modal_overlay(
            components::modal_card()
                .child(components::modal_header(SharedString::from(tf!(
                    k::SETTINGS_CONFIRM_DOWNLOAD_TITLE,
                    provider = provider
                ))))
                .child(
                    components::modal_body().child(
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child(SharedString::from(tf!(
                                k::SETTINGS_CONFIRM_DOWNLOAD_BODY,
                                provider = provider
                            ))),
                    ),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "settings-restore-cancel",
                        t(k::SETTINGS_ACTION_CANCEL),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                    components::button(
                        "settings-restore-ok",
                        t(k::SETTINGS_CONFIRM_DOWNLOAD_CONFIRM),
                        ButtonTone::Danger,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        this.start_restore(cx);
                    }))
                    .into_any_element(),
                ])),
        )
    }

    fn render_discard_confirm(&self, cx: &mut Context<Self>) -> gpui::Div {
        components::modal_overlay(
            components::modal_card()
                .child(components::modal_header(t(k::SETTINGS_DISCARD_TITLE)))
                .child(
                    components::modal_body().child(
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .child(t(k::SETTINGS_DISCARD_BODY)),
                    ),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "settings-discard-cancel",
                        t(k::SETTINGS_DISCARD_CANCEL),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        this.pending_nav = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                    components::button(
                        "settings-discard-ok",
                        t(k::SETTINGS_DISCARD_CONFIRM),
                        ButtonTone::Danger,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.confirm = None;
                        let target = this.pending_nav.take().unwrap_or(Page::Root);
                        this.discard_sync(cx);
                        this.enter(target, cx);
                    }))
                    .into_any_element(),
                ])),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = match self.page {
            Page::Root => self.render_root(cx),
            Page::Apps => self.render_apps(cx),
            Page::Sync => self.render_sync(cx),
        };
        if let Some(confirm) = self.confirm {
            page = page.child(self.render_confirm(confirm, cx));
        }
        page
    }
}

crate::notifications::impl_status_toasts_leveled!(SettingsView);
