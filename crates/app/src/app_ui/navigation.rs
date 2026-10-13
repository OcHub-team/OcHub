//! Shell-owned navigation and editor leave policy. Product views keep their
//! drafts; this module is the only place that parks and resumes destinations.
use super::*;

#[derive(Clone)]
pub(super) enum PendingNavigation {
    App(AppId),
    Section(Section),
    Local,
    Remote(String),
    AppSettings,
    CloseEditor,
    CloseForm(FormOwner),
    CloseWindow,
    Quit,
    ModelProviderImport(Box<ochub_core::ModelProviderImportManifest>),
}

impl AppRoot {
    fn form_states(&self, cx: &App) -> [(FormOwner, LeaveState); 8] {
        [
            (
                FormOwner::Settings,
                self.settings_view.read(cx).leave_state(cx),
            ),
            (
                FormOwner::AppSettings,
                self.app_settings_view.read(cx).leave_state(cx),
            ),
            (
                FormOwner::Network,
                self.network_view.read(cx).leave_state(cx),
            ),
            (FormOwner::Mcp, self.mcp_view.read(cx).leave_state(cx)),
            (
                FormOwner::Gateway,
                self.gateway_view.read(cx).leave_state(cx),
            ),
            (FormOwner::Themes, self.theme_view.read(cx).leave_state(cx)),
            (FormOwner::Tools, self.tools_view.read(cx).leave_state(cx)),
            (FormOwner::Remote, self.remote_view.read(cx).leave_state(cx)),
        ]
    }

    pub(super) fn request_form_discard(&mut self, owner: FormOwner, cx: &mut Context<Self>) {
        self.pending_form_leave = Some(owner);
        self.pending_navigation = Some(PendingNavigation::CloseForm(owner));
        cx.notify();
    }

    fn return_to_form(&mut self, owner: FormOwner, cx: &mut Context<Self>) {
        self.pending_navigation = None;
        self.pending_form_leave = None;
        self.section = match owner {
            FormOwner::Settings => Section::Settings,
            FormOwner::AppSettings => {
                self.showing_app_settings = true;
                Section::Providers
            }
            FormOwner::Network => Section::Network,
            FormOwner::Mcp => Section::Mcp,
            FormOwner::Gateway => Section::Gateway,
            FormOwner::Themes => Section::Themes,
            FormOwner::Tools => Section::Tools,
            FormOwner::Remote => Section::Remote,
        };
        cx.notify();
    }

    fn discard_form_and_continue(
        &mut self,
        owner: FormOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .form_states(cx)
            .iter()
            .any(|(id, state)| *id == owner && *state == LeaveState::Busy)
        {
            return;
        }
        match owner {
            FormOwner::Settings => self
                .settings_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::AppSettings => self
                .app_settings_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::Network => self
                .network_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::Mcp => self
                .mcp_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::Gateway => self
                .gateway_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::Themes => self
                .theme_view
                .update(cx, |view, cx| view.discard_changes(window, cx)),
            FormOwner::Tools => self
                .tools_view
                .update(cx, |view, cx| view.discard_changes(cx)),
            FormOwner::Remote => self
                .remote_view
                .update(cx, |view, cx| view.discard_changes(cx)),
        }
        self.pending_form_leave = None;
        self.complete_navigation(cx);
    }

    fn render_form_leave(
        &self,
        owner: FormOwner,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        ochub_ui::modal::overlay(
            "leave-form-modal",
            t(k::INTERACTION_LEAVE_TITLE),
            &self.navigation_focus,
            ochub_ui::modal::card(window, px(520.))
                .child(components::modal_header(t(k::INTERACTION_LEAVE_TITLE)))
                .child(
                    ochub_ui::modal::body("leave-form-body").child(t(k::INTERACTION_LEAVE_MESSAGE)),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "leave-form-discard",
                        t(k::INTERACTION_LEAVE_DISCARD),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.discard_form_and_continue(owner, window, cx)
                    }))
                    .into_any_element(),
                    components::button(
                        "leave-form-return",
                        t(k::INTERACTION_LEAVE_RETURN),
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.return_to_form(owner, cx)))
                    .into_any_element(),
                ])),
        )
        .on_action(
            cx.listener(move |this, _: &ochub_ui::modal::Dismiss, _, cx| {
                this.return_to_form(owner, cx)
            }),
        )
        .into_any_element()
    }

    pub(super) fn save_active(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if self.notifications.read(cx).is_center_open() || self.pending_form_leave.is_some() {
            window.play_system_bell();
            return;
        }
        if self.pending_navigation.is_some() {
            if let Some(editor) = self.editor.clone() {
                editor.update(cx, |editor, cx| editor.request_save_and_leave(cx));
            }
            return;
        }
        if self.confirm_delete.is_some()
            || self.pending_drift.is_some()
            || self.provider_quota_detail.is_some()
        {
            window.play_system_bell();
            return;
        }
        if self.show_first_run_notice {
            window.play_system_bell();
            return;
        }
        match self.section {
            Section::Providers if self.showing_app_settings => {
                self.app_settings_view
                    .update(cx, |view, cx| view.shortcut_save(window, cx));
            }
            Section::Providers => {
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.shortcut_save(cx));
                } else {
                    window.play_system_bell();
                }
            }
            Section::Gateway => self
                .gateway_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Mcp => self
                .mcp_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Tools => self
                .tools_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Themes => self
                .theme_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Settings => self
                .settings_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Network => self
                .network_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            _ => window.play_system_bell(),
        }
    }

    pub(super) fn cancel_active(
        &mut self,
        _: &Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.notifications.read(cx).is_center_open() {
            self.notifications
                .update(cx, |host, cx| host.close_center(cx));
            return;
        }
        if self.pending_navigation.take().is_some() {
            self.pending_form_leave = None;
            cx.notify();
            return;
        }
        // The workspace popover owns Escape while open. Never forward its
        // dismissal to an editor with an unsaved draft underneath.
        if std::mem::take(&mut self.workspace_scope_open) {
            cx.notify();
            return;
        }
        if self.show_first_run_notice {
            window.play_system_bell();
            return;
        }
        if cx.stop_active_drag(window) {
            self.provider_drag_state = None;
            cx.notify();
            return;
        }
        if self.confirm_delete.take().is_some() {
            cx.notify();
            return;
        }
        // Read-only, so it closes before anything with unsaved state below it.
        if self.provider_quota_detail.take().is_some() {
            cx.notify();
            return;
        }
        // Escape leaves the file exactly as the user left it.
        if self.pending_drift.take().is_some() {
            cx.notify();
            return;
        }
        match self.section {
            Section::Providers if self.showing_app_settings => {
                self.app_settings_view
                    .update(cx, |view, cx| view.shortcut_cancel(cx));
            }
            Section::Providers => {
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.shortcut_cancel(cx));
                } else {
                    window.play_system_bell();
                }
            }
            Section::Gateway => self
                .gateway_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Mcp => self
                .mcp_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Tools => self
                .tools_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Themes => self
                .theme_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Settings => self
                .settings_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Remote => self
                .remote_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            _ => window.play_system_bell(),
        }
    }

    pub(super) fn allow_navigation(
        &mut self,
        target: PendingNavigation,
        cx: &mut Context<Self>,
    ) -> bool {
        self.notifications
            .update(cx, |host, cx| host.close_center(cx));
        if self.provider_action_in_flight {
            self.notify_info(t(k::WORKBENCH_BUSY), cx);
            return false;
        }
        let forms = self.form_states(cx);
        if forms.iter().any(|(_, state)| *state == LeaveState::Busy)
            || (self.about_view.read(cx).is_installing()
                && matches!(
                    target,
                    PendingNavigation::Local
                        | PendingNavigation::Remote(_)
                        | PendingNavigation::CloseWindow
                        | PendingNavigation::Quit
                ))
        {
            self.notify_info(t(k::WORKBENCH_BUSY), cx);
            return false;
        }
        if let Some((owner, _)) = forms.iter().find(|(_, state)| *state == LeaveState::Dirty) {
            self.pending_form_leave = Some(*owner);
            self.pending_navigation = Some(target);
            cx.notify();
            return false;
        }
        self.pending_form_leave = None;
        self.pending_drift = None;
        let Some(editor) = self.editor.as_ref() else {
            return true;
        };
        if editor.read(cx).is_busy() {
            self.notify_info(t(k::WORKBENCH_BUSY), cx);
            return false;
        }
        if editor.read(cx).is_dirty(cx) {
            self.pending_navigation = Some(target);
            cx.notify();
            return false;
        }
        true
    }

    pub(super) fn open_model_provider_import(
        &mut self,
        manifest: ochub_core::ModelProviderImportManifest,
        cx: &mut Context<Self>,
    ) {
        if !self.allow_navigation(
            PendingNavigation::ModelProviderImport(Box::new(manifest.clone())),
            cx,
        ) {
            return;
        }
        self.select_section(Section::Gateway, cx);
        self.gateway_view
            .update(cx, |view, cx| view.open_model_provider_import(manifest, cx));
    }

    pub(crate) fn request_close(&mut self, quit: bool, cx: &mut Context<Self>) -> bool {
        self.allow_navigation(
            if quit {
                PendingNavigation::Quit
            } else {
                PendingNavigation::CloseWindow
            },
            cx,
        )
    }

    pub(super) fn complete_navigation(&mut self, cx: &mut Context<Self>) {
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).is_busy())
        {
            return;
        }
        let target = self
            .pending_navigation
            .take()
            .unwrap_or(PendingNavigation::CloseEditor);
        self.editor = None;
        // A second form may have been edited before this destination was
        // requested. Resolve every dirty owner before closing the process.
        if !self.allow_navigation(target.clone(), cx) {
            return;
        }
        match target {
            PendingNavigation::App(app) => self.select_app(app, cx),
            PendingNavigation::Section(section) => self.select_section(section, cx),
            PendingNavigation::Local => self.select_local_scope(cx),
            PendingNavigation::Remote(id) => self.select_remote_scope(id, cx),
            PendingNavigation::AppSettings => self.open_app_settings(cx),
            PendingNavigation::CloseEditor => self.reload(cx),
            PendingNavigation::CloseForm(FormOwner::AppSettings) => {
                self.showing_app_settings = false
            }
            PendingNavigation::CloseForm(owner) => self.return_to_form(owner, cx),
            PendingNavigation::CloseWindow => cx.defer(crate::close_main_window),
            PendingNavigation::Quit => cx.quit(),
            PendingNavigation::ModelProviderImport(manifest) => {
                self.open_model_provider_import(*manifest, cx)
            }
        }
        cx.notify();
    }

    pub(super) fn render_navigation_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        self.pending_navigation.as_ref()?;
        if let Some(owner) = self.pending_form_leave {
            return Some(self.render_form_leave(owner, window, cx));
        }
        let busy = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).is_busy());
        let application_unknown = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).has_pending_apply());
        let can_save = !busy
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.read(cx).can_save_draft())
            && self
                .workspace_backend(cx)
                .is_some_and(|backend| backend.supports_editor_drafts());
        let save = if can_save {
            components::button(
                "leave-editor-save",
                t(k::WORKBENCH_LEAVE_SAVE),
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(editor) = this.editor.clone() {
                    editor.update(cx, |editor, cx| editor.request_save_and_leave(cx));
                }
            }))
        } else {
            components::disabled_button(
                "leave-editor-save",
                t(k::WORKBENCH_LEAVE_SAVE),
                ButtonTone::Primary,
                ButtonSize::Sm,
                true,
            )
        };
        let discard = if busy {
            components::disabled_button(
                "leave-editor-discard",
                t(k::WORKBENCH_LEAVE_DISCARD),
                ButtonTone::Neutral,
                ButtonSize::Sm,
                true,
            )
        } else {
            components::button(
                "leave-editor-discard",
                t(k::WORKBENCH_LEAVE_DISCARD),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| this.complete_navigation(cx)))
        };
        Some(
            ochub_ui::modal::overlay(
                "leave-editor-modal",
                t(k::WORKBENCH_LEAVE_TITLE),
                &self.navigation_focus,
                ochub_ui::modal::card(window, px(560.))
                    .child(components::modal_header(t(k::WORKBENCH_LEAVE_TITLE)))
                    .child(ochub_ui::modal::body("leave-editor-body").child(t(
                        if application_unknown {
                            k::WORKBENCH_LEAVE_PENDING
                        } else {
                            k::WORKBENCH_LEAVE_MESSAGE
                        },
                    )))
                    .child(components::modal_footer(vec![
                        components::button(
                            "leave-editor-continue",
                            t(k::WORKBENCH_LEAVE_CONTINUE),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pending_navigation = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                        discard.into_any_element(),
                        save.into_any_element(),
                    ])),
            )
            .into_any_element(),
        )
    }
}
