//! The sync sub-page: the app's only batched form.
//!
//! Every control here is part of one commit, so the page can end in a single
//! [`components::commit_bar`] with no doctrine exception — the destination
//! selector and the auto-sync switch stay on the root page precisely so that
//! stays true.
//!
//! Two form rules are worth stating outright:
//!
//! * **Dirty is computed, never cached.** Seven short strings compared against
//!   their baselines costs nothing and cannot desynchronise from the inputs the
//!   way a `dirty` flag maintained by hand can.
//! * **Errors are stored as an enum, rendered through `t()` at paint time.**
//!   `relocalize` therefore has nothing at all to do for them, instead of
//!   having to remember to re-resolve each one.

use std::sync::Arc;

use gpui::{div, prelude::*, App, Context, Entity, SharedString, Window};
use ochub_core::db::Database;
use ochub_core::settings::{self, S3SyncSettings, WebDavSyncSettings};

use crate::components::{self, ButtonTone};
use crate::i18n::{k, t, Key};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::{TextInput, TextInputEvent};
use crate::tf;
use crate::theme;

use super::options;
use super::{Confirm, SettingsView};

// ── The destination ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncTarget {
    WebDav,
    S3,
}

impl SyncTarget {
    /// The product name. Not prose — WebDAV and S3 are spelled the same in
    /// every locale.
    pub(super) fn provider(self) -> &'static str {
        match self {
            Self::WebDav => "WebDAV",
            Self::S3 => "S3",
        }
    }

    fn fields(self) -> &'static [FieldSpec] {
        match self {
            Self::WebDav => WEBDAV_FIELDS,
            Self::S3 => S3_FIELDS,
        }
    }
}

// ── The field descriptor table ──────────────────────────────────────────────

/// A label that is either translated prose or a Latin identifier. The S3 field
/// names stay Latin because they are the names every S3 console and SDK uses.
enum Label {
    Latin(&'static str),
    Text(Key),
}

impl Label {
    fn resolve(&self) -> SharedString {
        match self {
            Self::Latin(text) => SharedString::new_static(text),
            Self::Text(key) => t(*key),
        }
    }
}

/// What makes a value acceptable, and which message says so when it is not.
enum Rule {
    Optional,
    Required(Key),
    RequiredUrl { required: Key, invalid: Key },
    OptionalUrl { invalid: Key },
}

pub(super) struct FieldSpec {
    id: &'static str,
    label: Label,
    help: Key,
    required: bool,
    masked: bool,
    placeholder: Label,
    rule: Rule,
}

/// The two credential forms come from this one table, which is what replaces
/// eighteen hand-written input rows.
const WEBDAV_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        id: "webdav-url",
        label: Label::Text(k::SETTINGS_WEBDAV_URL_LABEL),
        help: k::SETTINGS_WEBDAV_URL_DESC,
        required: true,
        masked: false,
        placeholder: Label::Latin("https://dav.example.com"),
        rule: Rule::RequiredUrl {
            required: k::SETTINGS_SYNC_ERROR_URL_REQUIRED,
            invalid: k::SETTINGS_SYNC_ERROR_URL_INVALID,
        },
    },
    FieldSpec {
        id: "webdav-username",
        label: Label::Text(k::SETTINGS_WEBDAV_USERNAME_LABEL),
        help: k::SETTINGS_WEBDAV_USERNAME_DESC,
        required: true,
        masked: false,
        placeholder: Label::Text(k::SETTINGS_SYNC_USERNAME_PLACEHOLDER),
        rule: Rule::Required(k::SETTINGS_SYNC_ERROR_USERNAME_REQUIRED),
    },
    FieldSpec {
        id: "webdav-password",
        label: Label::Text(k::SETTINGS_WEBDAV_PASSWORD_LABEL),
        help: k::SETTINGS_WEBDAV_PASSWORD_DESC,
        required: false,
        masked: true,
        placeholder: Label::Text(k::SETTINGS_SYNC_PASSWORD_PLACEHOLDER),
        rule: Rule::Optional,
    },
    FieldSpec {
        id: "webdav-remote-root",
        label: Label::Text(k::SETTINGS_SYNC_REMOTE_ROOT_LABEL),
        help: k::SETTINGS_SYNC_REMOTE_ROOT_DESC,
        required: false,
        masked: false,
        placeholder: Label::Latin("ochub-sync"),
        rule: Rule::Optional,
    },
    FieldSpec {
        id: "webdav-profile",
        label: Label::Text(k::SETTINGS_SYNC_PROFILE_LABEL),
        help: k::SETTINGS_SYNC_PROFILE_DESC,
        required: false,
        masked: false,
        placeholder: Label::Latin("default"),
        rule: Rule::Optional,
    },
];

const S3_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        id: "s3-region",
        label: Label::Latin("Region"),
        help: k::SETTINGS_S3_REGION_DESC,
        required: true,
        masked: false,
        placeholder: Label::Latin("auto"),
        rule: Rule::Required(k::SETTINGS_SYNC_ERROR_REGION_REQUIRED),
    },
    FieldSpec {
        id: "s3-bucket",
        label: Label::Latin("Bucket"),
        help: k::SETTINGS_S3_BUCKET_DESC,
        required: true,
        masked: false,
        placeholder: Label::Latin("bucket"),
        rule: Rule::Required(k::SETTINGS_SYNC_ERROR_BUCKET_REQUIRED),
    },
    FieldSpec {
        id: "s3-access-key",
        label: Label::Latin("Access Key ID"),
        help: k::SETTINGS_S3_ACCESS_KEY_DESC,
        required: true,
        masked: false,
        placeholder: Label::Latin("Access Key ID"),
        rule: Rule::Required(k::SETTINGS_SYNC_ERROR_ACCESS_KEY_REQUIRED),
    },
    FieldSpec {
        id: "s3-secret-key",
        label: Label::Latin("Secret Access Key"),
        help: k::SETTINGS_S3_SECRET_KEY_DESC,
        required: true,
        masked: true,
        placeholder: Label::Latin("Secret Access Key"),
        rule: Rule::Required(k::SETTINGS_SYNC_ERROR_SECRET_KEY_REQUIRED),
    },
    FieldSpec {
        id: "s3-endpoint",
        label: Label::Latin("Endpoint"),
        help: k::SETTINGS_S3_ENDPOINT_DESC,
        required: false,
        masked: false,
        placeholder: Label::Latin("https://<account>.r2.cloudflarestorage.com"),
        rule: Rule::OptionalUrl {
            invalid: k::SETTINGS_SYNC_ERROR_ENDPOINT_INVALID,
        },
    },
    FieldSpec {
        id: "s3-remote-root",
        label: Label::Text(k::SETTINGS_SYNC_REMOTE_ROOT_LABEL),
        help: k::SETTINGS_SYNC_REMOTE_ROOT_DESC,
        required: false,
        masked: false,
        placeholder: Label::Latin("ochub-sync"),
        rule: Rule::Optional,
    },
    FieldSpec {
        id: "s3-profile",
        label: Label::Text(k::SETTINGS_SYNC_PROFILE_LABEL),
        help: k::SETTINGS_SYNC_PROFILE_DESC,
        required: false,
        masked: false,
        placeholder: Label::Latin("default"),
        rule: Rule::Optional,
    },
];

/// Which way a value failed. The wording lives in the spec's [`Rule`], so this
/// stays language-free and `relocalize` never has to touch it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldError {
    Required,
    NotHttpUrl,
}

pub(super) struct DraftField {
    spec: &'static FieldSpec,
    input: Entity<TextInput>,
    baseline: SharedString,
    error: Option<FieldError>,
}

pub(super) struct SyncDraft {
    target: SyncTarget,
    fields: Vec<DraftField>,
    saving: bool,
}

/// One manual sync action, carrying everything it needs.
///
/// Test owns the credentials it is going to probe, so "test with no
/// credentials" is not a state this type can be in — there is no impossible
/// case for [`perform`] to invent an error message for.
enum Action {
    /// Draft credentials to probe **without writing them anywhere**.
    Test(Candidate),
    Upload,
    Restore,
}

enum Candidate {
    WebDav(Box<WebDavSyncSettings>),
    S3(Box<S3SyncSettings>),
}

impl Candidate {
    fn target(&self) -> SyncTarget {
        match self {
            Self::WebDav(_) => SyncTarget::WebDav,
            Self::S3(_) => SyncTarget::S3,
        }
    }
}

impl SettingsView {
    // ── Draft lifecycle ─────────────────────────────────────────────────────

    /// Build the draft from a **fresh** read of the store. The inputs exist
    /// only while this page is open, which is what retires the old page's
    /// sixteen startup-built inputs that were snapshotted once and never
    /// resynced.
    pub(super) fn open_sync(&mut self, cx: &mut Context<Self>) {
        let stored = settings::get_settings();
        self.settings = stored;
        let Some(target) = self.sync_target() else {
            return;
        };
        let values = self.stored_field_values(target);
        let mut fields = Vec::with_capacity(values.len());
        for (spec, value) in target.fields().iter().zip(values) {
            let placeholder = spec.placeholder.resolve();
            let masked = spec.masked;
            let seed = value.clone();
            let input = cx.new(|cx| {
                let mut input = TextInput::new(cx, placeholder).masked(masked);
                input.set_content(seed, cx);
                input
            });
            // A corrected field stops shouting immediately; validation itself
            // still runs on Save, so a half-typed URL is never an error.
            let id = spec.id;
            cx.subscribe(&input, move |this, _input, _: &TextInputEvent, cx| {
                this.clear_field_error(id, cx);
            })
            .detach();
            fields.push(DraftField {
                spec,
                input,
                baseline: SharedString::from(value),
                error: None,
            });
        }
        self.draft = Some(SyncDraft {
            target,
            fields,
            saving: false,
        });
    }

    pub(super) fn close_sync(&mut self) {
        self.draft = None;
    }

    pub(super) fn draft_dirty(&self, cx: &App) -> bool {
        let Some(draft) = self.draft.as_ref() else {
            return false;
        };
        draft
            .fields
            .iter()
            .any(|field| field.input.read(cx).content().trim() != field.baseline.trim())
    }

    pub(super) fn draft_saving(&self) -> bool {
        self.draft.as_ref().is_some_and(|draft| draft.saving)
    }

    /// A required field left empty. Cheap enough to run every frame, and it is
    /// all the Test button needs to know before it is worth pressing.
    fn draft_incomplete(&self, cx: &App) -> bool {
        let Some(draft) = self.draft.as_ref() else {
            return true;
        };
        draft
            .fields
            .iter()
            .any(|field| field.spec.required && field.input.read(cx).content().trim().is_empty())
    }

    /// Re-seed a **clean** draft from the store. Called by `reload`; a dirty
    /// draft is left exactly as typed.
    pub(super) fn reseed_draft(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.draft.as_ref().map(|draft| draft.target) else {
            return;
        };
        // The destination may have changed underneath the page (a restore
        // brings its own settings), in which case the form is a different form.
        if self.sync_target() != Some(target) {
            self.close_sync();
            self.open_sync(cx);
            return;
        }
        let values = self.stored_field_values(target);
        self.apply_values(values, cx);
    }

    pub(super) fn relocalize_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let placeholders: Vec<(Entity<TextInput>, SharedString)> = draft
            .fields
            .iter()
            .map(|field| (field.input.clone(), field.spec.placeholder.resolve()))
            .collect();
        for (input, placeholder) in placeholders {
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
    }

    /// Push `values` into the inputs and make them the new baselines, so the
    /// form reads clean afterwards.
    fn apply_values(&mut self, values: Vec<String>, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let updates: Vec<(Entity<TextInput>, String)> = draft
            .fields
            .iter()
            .zip(values.iter())
            .map(|(field, value)| (field.input.clone(), value.clone()))
            .collect();
        for (input, value) in updates {
            input.update(cx, |input, cx| input.set_content(value, cx));
        }
        if let Some(draft) = self.draft.as_mut() {
            for (field, value) in draft.fields.iter_mut().zip(values) {
                field.baseline = SharedString::from(value);
                field.error = None;
            }
        }
    }

    /// Clear the error on the one field that just changed. Typing also moves
    /// the commit bar between clean and dirty, so this repaints either way.
    fn clear_field_error(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.as_mut() {
            for field in &mut draft.fields {
                if field.spec.id == id {
                    field.error = None;
                }
            }
        }
        cx.notify();
    }

    fn stored_field_values(&self, target: SyncTarget) -> Vec<String> {
        match target {
            SyncTarget::WebDav => {
                let sync = self.settings.webdav_sync.clone().unwrap_or_default();
                vec![
                    sync.base_url,
                    sync.username,
                    sync.password,
                    sync.remote_root,
                    sync.profile,
                ]
            }
            SyncTarget::S3 => {
                let sync = self.settings.s3_sync.clone().unwrap_or_default();
                vec![
                    sync.region,
                    sync.bucket,
                    sync.access_key_id,
                    sync.secret_access_key,
                    sync.endpoint,
                    sync.remote_root,
                    sync.profile,
                ]
            }
        }
    }

    fn draft_values(&self, cx: &App) -> Vec<String> {
        self.draft
            .as_ref()
            .map(|draft| {
                draft
                    .fields
                    .iter()
                    .map(|field| field.input.read(cx).content().trim().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Commit ──────────────────────────────────────────────────────────────

    pub(super) fn save_sync(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.draft.as_ref().map(|draft| draft.target) else {
            return;
        };
        if self.draft_saving() || self.sync_busy {
            return;
        }
        let values = self.draft_values(cx);
        let Some(normalized) = self.validate_sync(target, &values, cx) else {
            // Errors are now next to the offending fields; a toast repeating
            // them would be noise.
            return;
        };

        // Guard the write against a second submit arriving in the same frame
        // (Enter and a click both land before the next paint).
        if let Some(draft) = self.draft.as_mut() {
            draft.saving = true;
        }
        // Only the credential fields are written. `enabled`, `auto_sync` and
        // `status` belong to the store, and a sync running right now is writing
        // `status` into the same struct.
        let write = match target {
            SyncTarget::WebDav => {
                let sync = normalized.clone();
                settings::mutate_settings(move |settings| {
                    let stored = settings.webdav_sync.get_or_insert_with(Default::default);
                    stored.base_url = sync[0].clone();
                    stored.username = sync[1].clone();
                    stored.password = sync[2].clone();
                    stored.remote_root = sync[3].clone();
                    stored.profile = sync[4].clone();
                })
            }
            SyncTarget::S3 => {
                let sync = normalized.clone();
                settings::mutate_settings(move |settings| {
                    let stored = settings.s3_sync.get_or_insert_with(Default::default);
                    stored.region = sync[0].clone();
                    stored.bucket = sync[1].clone();
                    stored.access_key_id = sync[2].clone();
                    stored.secret_access_key = sync[3].clone();
                    stored.endpoint = sync[4].clone();
                    stored.remote_root = sync[5].clone();
                    stored.profile = sync[6].clone();
                })
            }
        };

        if let Some(draft) = self.draft.as_mut() {
            draft.saving = false;
        }
        match write {
            Ok(()) => {
                self.settings = settings::get_settings();
                // Re-seed from the *normalized* values, so an empty 远端目录
                // becoming `ochub-sync` lands in the input the user is looking
                // at rather than only on disk.
                self.apply_values(normalized, cx);
                self.set_status(NotificationLevel::Success, t(k::SETTINGS_STATUS_SAVED), cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(
                    k::SETTINGS_SYNC_SETTINGS_SAVE_FAILED,
                    provider = target.provider(),
                    error = err
                ),
                cx,
            ),
        }
    }

    pub(super) fn discard_sync(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let baselines: Vec<(Entity<TextInput>, SharedString)> = draft
            .fields
            .iter()
            .map(|field| (field.input.clone(), field.baseline.clone()))
            .collect();
        for (input, baseline) in baselines {
            input.update(cx, |input, cx| input.set_content(baseline, cx));
        }
        if let Some(draft) = self.draft.as_mut() {
            for field in &mut draft.fields {
                field.error = None;
            }
        }
        cx.notify();
    }

    /// Validate per field, assign the errors, and return the normalized values
    /// when everything passes.
    ///
    /// The rules duplicate about ten lines of core's `validate()` because
    /// `AppError::localized` carries a message but no field id, and an error
    /// with no field cannot be rendered beside one. Core's `validate()` still
    /// runs, as the backstop, immediately before the write.
    fn validate_sync(
        &mut self,
        target: SyncTarget,
        values: &[String],
        cx: &mut Context<Self>,
    ) -> Option<Vec<String>> {
        let errors: Vec<Option<FieldError>> = target
            .fields()
            .iter()
            .zip(values)
            .map(|(spec, value)| check(spec, value))
            .collect();
        if errors.iter().any(Option::is_some) {
            if let Some(draft) = self.draft.as_mut() {
                for (field, error) in draft.fields.iter_mut().zip(errors) {
                    field.error = error;
                }
            }
            cx.notify();
            return None;
        }

        let normalized = match target {
            SyncTarget::WebDav => {
                let mut candidate = webdav_from(values);
                candidate.normalize();
                if let Err(err) = candidate.validate() {
                    self.set_status(NotificationLevel::Error, err.to_string(), cx);
                    return None;
                }
                vec![
                    candidate.base_url,
                    candidate.username,
                    candidate.password,
                    candidate.remote_root,
                    candidate.profile,
                ]
            }
            SyncTarget::S3 => {
                let mut candidate = s3_from(values);
                candidate.normalize();
                if let Err(err) = candidate.validate() {
                    self.set_status(NotificationLevel::Error, err.to_string(), cx);
                    return None;
                }
                vec![
                    candidate.region,
                    candidate.bucket,
                    candidate.access_key_id,
                    candidate.secret_access_key,
                    candidate.endpoint,
                    candidate.remote_root,
                    candidate.profile,
                ]
            }
        };
        Some(normalized)
    }

    // ── Manual actions ──────────────────────────────────────────────────────

    /// Probe what is **typed**, without writing it anywhere.
    ///
    /// Disabling Test while the form is dirty would force save-then-test, which
    /// puts unverified credentials on disk to find out whether they work.
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.draft.as_ref().map(|draft| draft.target) else {
            return;
        };
        let values = self.draft_values(cx);
        let Some(normalized) = self.validate_sync(target, &values, cx) else {
            return;
        };
        let candidate = match target {
            SyncTarget::WebDav => Candidate::WebDav(Box::new(webdav_from(&normalized))),
            SyncTarget::S3 => Candidate::S3(Box::new(s3_from(&normalized))),
        };
        self.start_sync(Action::Test(candidate), cx);
    }

    /// The restore path, entered only through the confirm modal.
    pub(super) fn start_restore(&mut self, cx: &mut Context<Self>) {
        self.start_sync(Action::Restore, cx);
    }

    fn start_sync(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.sync_busy {
            return;
        }
        // Test names its own destination; upload and restore act on whatever
        // is currently selected.
        let target = match &action {
            Action::Test(candidate) => candidate.target(),
            _ => match self.sync_target() {
                Some(target) => target,
                None => return,
            },
        };
        self.sync_busy = true;
        self.set_status(NotificationLevel::Info, start_message(&action, target), cx);

        let db = self.app.db.clone();
        cx.spawn(async move |this, cx| {
            let backup = if matches!(action, Action::Restore) {
                // Copying the database is real file I/O; it does not belong on
                // the render thread. A failed pre-backup **refuses** the
                // restore rather than proceeding, which is what makes the one
                // irreversible action in the app recoverable.
                let db = db.clone();
                match cx
                    .background_spawn(async move { db.create_backup_file() })
                    .await
                {
                    Ok(file) => Some(file),
                    Err(err) => {
                        this.update(cx, |this, cx| {
                            this.sync_busy = false;
                            this.set_status(
                                NotificationLevel::Error,
                                tf!(k::SETTINGS_RESTORE_BACKUP_FAILED, error = err),
                                cx,
                            );
                        })
                        .ok();
                        return;
                    }
                }
            } else {
                None
            };

            let outcome = perform(action, target, &db, backup).await;
            this.update(cx, |this, cx| {
                this.sync_busy = false;
                this.reload(cx);
                match outcome {
                    Ok(message) => this.set_status(NotificationLevel::Success, message, cx),
                    Err(message) => this.set_status(NotificationLevel::Error, message, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Whether the *stored* settings are complete enough to act on. Upload and
    /// restore run against the store, not against the form.
    fn saved_valid(&self) -> bool {
        match self.sync_target() {
            Some(SyncTarget::WebDav) => self
                .settings
                .webdav_sync
                .as_ref()
                .is_some_and(|sync| sync.validate().is_ok()),
            Some(SyncTarget::S3) => self
                .settings
                .s3_sync
                .as_ref()
                .is_some_and(|sync| sync.validate().is_ok()),
            None => false,
        }
    }

    // ── Render ──────────────────────────────────────────────────────────────

    pub(super) fn render_sync(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let dirty = self.draft_dirty(cx);
        // `save_sync` also refuses while a sync action is in flight, so the bar
        // has to show that too — an enabled button that silently does nothing
        // is the defect this page was rebuilt to remove.
        let saving = self.draft_saving() || self.sync_busy;
        let discard = cx.listener(|this, _event: &(), _window: &mut Window, cx| {
            this.discard_sync(cx);
        });
        let save = cx.listener(|this, _event: &(), _window: &mut Window, cx| {
            this.save_sync(cx);
        });

        let column = layout::content_column()
            .child(self.render_sync_status())
            .child(self.render_sync_connection())
            .child(self.render_sync_actions(dirty, cx));

        layout::page()
            .relative()
            .child(self.sub_page_header(
                t(k::SETTINGS_SYNC_PAGE_TITLE),
                t(k::SETTINGS_SYNC_PAGE_DESC),
                cx,
            ))
            .child(layout::scroll_body(
                "settings-sync-body",
                &self.sync_scroll,
                column,
            ))
            // Pinned outside the scroll region: whether there is something
            // unsaved must never be a fact you have to scroll to find.
            .child(components::commit_bar(
                "sync",
                dirty,
                saving,
                move |window, cx| discard(&(), window, cx),
                move |window, cx| save(&(), window, cx),
            ))
    }

    fn render_sync_status(&self) -> gpui::Div {
        let target = self.sync_target();
        let provider = target.map(SyncTarget::provider).unwrap_or_default();
        let status = match target {
            Some(SyncTarget::WebDav) => self
                .settings
                .webdav_sync
                .as_ref()
                .map(|sync| sync.status.clone()),
            Some(SyncTarget::S3) => self
                .settings
                .s3_sync
                .as_ref()
                .map(|sync| sync.status.clone()),
            None => None,
        }
        .unwrap_or_default();

        let line = if self.sync_busy {
            t(k::SETTINGS_SYNC_STATUS_BUSY)
        } else {
            match status.last_sync_at {
                Some(timestamp) => SharedString::from(tf!(
                    k::SETTINGS_SYNC_STATUS_SYNCED_AT,
                    time = options::format_last_sync(timestamp)
                )),
                None => t(k::SETTINGS_SYNC_STATUS_NEVER),
            }
        };

        let mut card = components::card()
            .gap_1()
            .child(
                div()
                    .text_color(theme::text())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::new_static(provider)),
            )
            .child(div().text_xs().text_color(theme::muted()).child(line));
        if let Some(error) = status.last_error.filter(|_| !self.sync_busy) {
            card = card.child(components::field_error(SharedString::from(tf!(
                k::SETTINGS_SYNC_STATUS_ERROR,
                error = error
            ))));
        }
        section(
            t(k::SETTINGS_SYNC_SECTION_STATUS),
            t(k::SETTINGS_SYNC_SECTION_STATUS_DESC),
            card.into_any_element(),
        )
    }

    fn render_sync_connection(&self) -> gpui::Div {
        let Some(draft) = self.draft.as_ref() else {
            return div();
        };
        let mut card = components::card().gap_4();
        for field in &draft.fields {
            card = card.child(components::field_with_error(
                field.spec.label.resolve(),
                field.spec.required,
                Some(t(field.spec.help)),
                field.error.map(|error| error_text(&field.spec.rule, error)),
                div().w_full().child(field.input.clone()),
            ));
        }
        section(
            t(k::SETTINGS_SYNC_SECTION_CONNECTION),
            t(k::SETTINGS_SYNC_SECTION_CONNECTION_DESC),
            card.into_any_element(),
        )
    }

    fn render_sync_actions(&self, dirty: bool, cx: &mut Context<Self>) -> gpui::Div {
        let busy = self.sync_busy;
        let acts_on_stored = busy || dirty || !self.saved_valid();
        let test =
            cx.listener(|this, _event: &(), _window: &mut Window, cx| this.test_connection(cx));
        let upload = cx.listener(|this, _event: &(), _window: &mut Window, cx| {
            this.start_sync(Action::Upload, cx)
        });
        let restore = cx.listener(|this, _event: &(), _window: &mut Window, cx| {
            this.confirm = Some(Confirm::Restore);
            cx.notify();
        });

        let rows = vec![
            layout::action_row(
                "sync-test",
                t(k::SETTINGS_SYNC_TEST_LABEL),
                t(k::SETTINGS_SYNC_TEST_DESC),
                t(k::SETTINGS_SYNC_TEST_ACTION),
                ButtonTone::Neutral,
                busy || self.draft_incomplete(cx),
                move |window, cx| test(&(), window, cx),
            )
            .into_any_element(),
            layout::action_row(
                "sync-upload",
                t(k::SETTINGS_SYNC_UPLOAD_LABEL),
                t(k::SETTINGS_SYNC_UPLOAD_DESC),
                t(k::SETTINGS_SYNC_UPLOAD_ACTION),
                ButtonTone::Neutral,
                acts_on_stored,
                move |window, cx| upload(&(), window, cx),
            )
            .into_any_element(),
            layout::action_row(
                "sync-restore",
                t(k::SETTINGS_SYNC_RESTORE_LABEL),
                t(k::SETTINGS_SYNC_RESTORE_DESC),
                t(k::SETTINGS_SYNC_RESTORE_ACTION),
                ButtonTone::Danger,
                acts_on_stored,
                move |window, cx| restore(&(), window, cx),
            )
            .into_any_element(),
        ];
        section(
            t(k::SETTINGS_SYNC_SECTION_ACTIONS),
            t(k::SETTINGS_SYNC_SECTION_ACTIONS_DESC),
            layout::group(rows).into_any_element(),
        )
    }
}

fn section(title: SharedString, description: SharedString, body: gpui::AnyElement) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w_full()
        .child(layout::section_header(title, description))
        .child(body)
}

fn error_text(rule: &Rule, error: FieldError) -> SharedString {
    match (rule, error) {
        (Rule::Required(key), _) => t(*key),
        (Rule::RequiredUrl { required, .. }, FieldError::Required) => t(*required),
        (Rule::RequiredUrl { invalid, .. }, FieldError::NotHttpUrl) => t(*invalid),
        (Rule::OptionalUrl { invalid }, _) => t(*invalid),
        (Rule::Optional, _) => SharedString::default(),
    }
}

fn check(spec: &FieldSpec, value: &str) -> Option<FieldError> {
    let value = value.trim();
    match &spec.rule {
        Rule::Optional => None,
        Rule::Required(_) => value.is_empty().then_some(FieldError::Required),
        Rule::RequiredUrl { .. } => {
            if value.is_empty() {
                Some(FieldError::Required)
            } else if !is_http_url(value) {
                Some(FieldError::NotHttpUrl)
            } else {
                None
            }
        }
        Rule::OptionalUrl { .. } => {
            (!value.is_empty() && !is_http_url(value)).then_some(FieldError::NotHttpUrl)
        }
    }
}

fn is_http_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    (lower.starts_with("http://") && value.len() > "http://".len())
        || (lower.starts_with("https://") && value.len() > "https://".len())
}

fn webdav_from(values: &[String]) -> WebDavSyncSettings {
    debug_assert_eq!(values.len(), WEBDAV_FIELDS.len());
    WebDavSyncSettings {
        base_url: values[0].clone(),
        username: values[1].clone(),
        password: values[2].clone(),
        remote_root: values[3].clone(),
        profile: values[4].clone(),
        ..Default::default()
    }
}

fn s3_from(values: &[String]) -> S3SyncSettings {
    debug_assert_eq!(values.len(), S3_FIELDS.len());
    S3SyncSettings {
        region: values[0].clone(),
        bucket: values[1].clone(),
        access_key_id: values[2].clone(),
        secret_access_key: values[3].clone(),
        endpoint: values[4].clone(),
        remote_root: values[5].clone(),
        profile: values[6].clone(),
        ..Default::default()
    }
}

fn start_message(action: &Action, target: SyncTarget) -> String {
    let provider = target.provider();
    match action {
        Action::Test(_) => tf!(k::SETTINGS_SYNC_START_TEST, provider = provider),
        Action::Upload => tf!(k::SETTINGS_SYNC_START_UPLOAD, provider = provider),
        Action::Restore => tf!(k::SETTINGS_SYNC_START_DOWNLOAD, provider = provider),
    }
}

/// Run one manual action and phrase the result.
///
/// Test probes `probe` and writes nothing — the old code called
/// `set_webdav_sync_settings` *before* probing, so a failed test still saved
/// the credentials that failed. Upload and restore read the stored settings
/// here, inside the task, so they act on what was actually committed.
async fn perform(
    action: Action,
    target: SyncTarget,
    db: &Arc<Database>,
    backup: Option<String>,
) -> Result<SharedString, SharedString> {
    let provider = target.provider();
    let failed = |err: ochub_core::AppError| -> SharedString {
        SharedString::from(tf!(
            k::SETTINGS_SYNC_FAILED,
            provider = provider,
            error = err
        ))
    };
    let tested = || SharedString::from(tf!(k::SETTINGS_SYNC_TEST_OK, provider = provider));
    let uploaded = || SharedString::from(tf!(k::SETTINGS_SYNC_UPLOAD_OK, provider = provider));

    match action {
        Action::Test(Candidate::WebDav(sync)) => {
            ochub_core::services::webdav_sync::check_connection(&sync)
                .await
                .map(|_| tested())
                .map_err(failed)
        }
        Action::Test(Candidate::S3(sync)) => ochub_core::services::s3_sync::check_connection(&sync)
            .await
            .map(|_| tested())
            .map_err(failed),
        Action::Upload => match target {
            SyncTarget::WebDav => {
                let mut sync = settings::get_webdav_sync_settings().unwrap_or_default();
                ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::upload(db, &mut sync),
                )
                .await
                .map(|_| uploaded())
                .map_err(failed)
            }
            SyncTarget::S3 => {
                let mut sync = settings::get_s3_sync_settings().unwrap_or_default();
                ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::upload(db, &mut sync),
                )
                .await
                .map(|_| uploaded())
                .map_err(failed)
            }
        },
        Action::Restore => match target {
            SyncTarget::WebDav => {
                let mut sync = settings::get_webdav_sync_settings().unwrap_or_default();
                ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::download(db, &mut sync),
                )
                .await
                .map(|_| restored(provider, backup))
                .map_err(failed)
            }
            SyncTarget::S3 => {
                let mut sync = settings::get_s3_sync_settings().unwrap_or_default();
                ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::download(db, &mut sync),
                )
                .await
                .map(|_| restored(provider, backup))
                .map_err(failed)
            }
        },
    }
}

fn restored(provider: &str, backup: Option<String>) -> SharedString {
    SharedString::from(tf!(
        k::SETTINGS_RESTORE_DONE,
        provider = provider,
        file = backup.unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_required_value_is_flagged_as_required() {
        assert_eq!(check(&WEBDAV_FIELDS[0], "   "), Some(FieldError::Required));
        assert_eq!(check(&S3_FIELDS[1], ""), Some(FieldError::Required));
    }

    #[test]
    fn a_url_that_is_not_http_is_flagged_separately_from_a_missing_one() {
        assert_eq!(
            check(&WEBDAV_FIELDS[0], "dav.example.com"),
            Some(FieldError::NotHttpUrl)
        );
        assert_eq!(check(&WEBDAV_FIELDS[0], "https://dav.example.com"), None);
    }

    #[test]
    fn an_optional_url_only_complains_when_it_is_present_and_wrong() {
        assert_eq!(check(&S3_FIELDS[4], ""), None);
        assert_eq!(
            check(&S3_FIELDS[4], "r2.example.com"),
            Some(FieldError::NotHttpUrl)
        );
        assert_eq!(check(&S3_FIELDS[4], "https://r2.example.com"), None);
    }

    #[test]
    fn a_bare_scheme_is_not_a_url() {
        assert!(!is_http_url("https://"));
        assert!(is_http_url("HTTPS://Example.com"));
    }

    #[test]
    fn the_field_tables_match_the_structs_they_build() {
        assert_eq!(WEBDAV_FIELDS.len(), 5);
        assert_eq!(S3_FIELDS.len(), 7);
        let webdav = webdav_from(&vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
        ]);
        assert_eq!(webdav.base_url, "a");
        assert_eq!(webdav.profile, "e");
        // The form never touches these; they belong to the store.
        assert!(!webdav.enabled);
        assert!(!webdav.auto_sync);
    }
}
