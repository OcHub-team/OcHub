//! One application boundary for explicit editor-draft application and exact undo.
use super::{
    Application, ApplicationError, ApplicationResult, DraftApplyFailure, DraftApplyStage,
    DraftApplyState, EditorDraft, EditorDraftApplyResult, ProviderDetails, ProviderSwitchPolicy,
    provider_revision_hash,
};
use crate::plugin::AppMode;
use crate::services::config_commit::{
    ConfigApplyReceipt, ConfigApplyState, ConfigMutationGuard, FileCommitTransaction,
    pending_receipts,
};
use crate::{AppError, AppId, AppType, Provider};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyContext {
    pub app: String,
    pub provider_id: String,
    pub target_id: String,
    pub draft: EditorDraft,
    pub before_providers: Vec<Provider>,
    pub after_providers: Vec<Provider>,
    pub before_db_current: Option<String>,
    pub before_local_current: Option<String>,
    pub previous_head: Option<String>,
    pub common_before: Option<String>,
    pub common_cleared_before: bool,
    pub switch: bool,
    #[serde(default)]
    pub omo: bool,
}

impl Application {
    /// Compatibility bridge for existing CLI/RPC operations. Interactive drafts are never reused.
    pub(crate) fn apply_provider_candidate(
        &self,
        app: &AppId,
        original: Option<&Provider>,
        provider: Provider,
        policy: ProviderSwitchPolicy,
    ) -> ApplicationResult<crate::services::provider::SwitchResult> {
        self.apply_provider_candidate_with_common(app, original, provider, policy, None)
    }

    pub(crate) fn apply_provider_candidate_with_common(
        &self,
        app: &AppId,
        original: Option<&Provider>,
        provider: Provider,
        policy: ProviderSwitchPolicy,
        common_config: Option<super::EditorCommonConfig>,
    ) -> ApplicationResult<crate::services::provider::SwitchResult> {
        self.apply_provider_candidate_inner(app, original, provider, policy, common_config, true)
    }

    pub(crate) fn apply_stored_provider_candidate(
        &self,
        app: &AppId,
        original: &Provider,
        provider: Provider,
        policy: ProviderSwitchPolicy,
    ) -> ApplicationResult<crate::services::provider::SwitchResult> {
        if original.id != provider.id {
            return Err(ApplicationError::InvalidInput(
                "stored-version switching cannot change connection identity".into(),
            ));
        }
        self.apply_provider_candidate_inner(app, Some(original), provider, policy, None, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_provider_candidate_inner(
        &self,
        app: &AppId,
        original: Option<&Provider>,
        provider: Provider,
        policy: ProviderSwitchPolicy,
        common_config: Option<super::EditorCommonConfig>,
        validate_editor: bool,
    ) -> ApplicationResult<crate::services::provider::SwitchResult> {
        let id = format!("operation-{}", uuid::Uuid::new_v4());
        let input = super::EditorDraftInput {
            provider,
            original_provider_id: original.map(|p| p.id.clone()),
            base_provider_hash: original.map(provider_revision_hash),
            editor_state: Value::Null,
            raw_documents: None,
            common_config,
        };
        let draft = self.upsert_editor_draft(app, &id, None, input)?;
        let result =
            self.apply_editor_draft_inner(app, &id, draft.revision, policy, validate_editor);
        match result {
            Ok(result) if result.state == DraftApplyState::Applied => {
                Ok(crate::services::provider::SwitchResult {
                    warnings: result.warnings,
                    drift: None,
                })
            }
            Ok(result) => {
                let _ = self.delete_editor_draft(app, &id, draft.revision);
                let failure =
                    result
                        .failure
                        .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                            app: app.to_string(),
                            capability: "provider.external-import",
                        })?;
                if failure.recovery_required {
                    return Err(ApplicationError::RecoveryRequired(format!(
                        "{}; receipt {}",
                        failure.message,
                        result.receipt_id.unwrap_or_default()
                    )));
                }
                if failure.stage == DraftApplyStage::Conflict {
                    return Err(ApplicationError::ResourceConflict(failure.message));
                }
                Err(ApplicationError::OperationFailed(failure.message))
            }
            Err(error) => {
                let _ = self.delete_editor_draft(app, &id, draft.revision);
                Err(error)
            }
        }
    }

    pub fn apply_editor_draft(
        &self,
        app: &AppId,
        id: &str,
        revision: u64,
        policy: ProviderSwitchPolicy,
    ) -> ApplicationResult<EditorDraftApplyResult> {
        self.apply_editor_draft_inner(app, id, revision, policy, true)
    }

    fn apply_editor_draft_inner(
        &self,
        app: &AppId,
        id: &str,
        revision: u64,
        policy: ProviderSwitchPolicy,
        validate_editor: bool,
    ) -> ApplicationResult<EditorDraftApplyResult> {
        let _guard = ConfigMutationGuard::acquire()?;
        self.recover_pending_config_commits_locked()?;
        let draft = match self.get_editor_draft(app, id)? {
            Some(draft) => draft,
            None => {
                if let Some(transaction) =
                    crate::services::config_commit::completed_draft_transaction(
                        app.as_str(),
                        id,
                        revision,
                    )?
                {
                    let context: ApplyContext =
                        serde_json::from_value(transaction.context().clone())
                            .map_err(|e| ApplicationError::InvalidInput(e.to_string()))?;
                    let provider = context
                        .after_providers
                        .iter()
                        .find(|p| p.id == context.provider_id)
                        .cloned()
                        .ok_or_else(|| {
                            ApplicationError::OperationFailed(
                                "application receipt is missing its provider".into(),
                            )
                        })?;
                    let undo_available = transaction.receipt().undo_available
                        && self.verify_undo_context(&context, transaction.id()).is_ok()
                        && transaction.verify_after().is_ok();
                    return Ok(EditorDraftApplyResult {
                        state: DraftApplyState::Applied,
                        draft_id: id.into(),
                        draft_revision: revision,
                        provider: Some(ProviderDetails {
                            app: app.to_string(),
                            provider,
                        }),
                        receipt_id: Some(transaction.id().into()),
                        undo_available,
                        warnings: Vec::new(),
                        failure: None,
                    });
                }
                return Err(ApplicationError::NotFound {
                    kind: "editor-draft",
                    id: id.into(),
                });
            }
        };
        if draft.revision != revision {
            return Err(ApplicationError::ResourceConflict(
                "draft changed before apply; reload it first".into(),
            ));
        }
        let failed = |stage,
                      error: ApplicationError,
                      receipt_id: Option<String>,
                      recovery_required| EditorDraftApplyResult {
            state: DraftApplyState::Failed,
            draft_id: id.into(),
            draft_revision: revision,
            provider: None,
            receipt_id,
            undo_available: false,
            warnings: Vec::new(),
            failure: Some(DraftApplyFailure {
                stage,
                code: error.code().into(),
                message: error.to_string(),
                recovery_required,
            }),
        };
        let preparation = self.prepare_draft_application(app, &draft, policy, validate_editor);
        let (provider, mut plan, _outgoing, auth_tool, context) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                let stage = if matches!(
                    error,
                    ApplicationError::ResourceConflict(_) | ApplicationError::ConfigDrift { .. }
                ) {
                    DraftApplyStage::Conflict
                } else {
                    DraftApplyStage::Validation
                };
                return Ok(failed(stage, error, None, false));
            }
        };
        if AppType::from_app_id(app) == Some(AppType::CherryStudio) {
            return Ok(EditorDraftApplyResult{state:DraftApplyState::ExternalConfirmation,draft_id:id.into(),draft_revision:revision,provider:Some(ProviderDetails{app:app.to_string(),provider}),receipt_id:None,undo_available:false,warnings:vec!["External application import must be confirmed by the target application; the draft is retained.".into()],failure:None});
        }
        let Some(mut plan) = plan.take() else {
            return Ok(failed(
                DraftApplyStage::Prepare,
                ApplicationError::CapabilityUnsupported {
                    app: app.to_string(),
                    capability: "config.prepared-apply",
                },
                None,
                false,
            ));
        };
        if let Some(builtin) = AppType::from_app_id(app)
            && let Err(error) = crate::mcp::prepare_config_sync(&self.state.db, builtin, &mut plan)
        {
            return Ok(failed(DraftApplyStage::Prepare, error.into(), None, false));
        }
        let outside_side_effects = auth_tool.is_some()
            || provider
                .meta
                .as_ref()
                .is_some_and(|meta| meta.gateway_route_id.is_some());
        let mut transaction = match FileCommitTransaction::prepare(
            &plan,
            serde_json::to_value(&context).map_err(|source| AppError::JsonSerialize { source })?,
            !outside_side_effects,
        ) {
            Ok(transaction) => transaction,
            Err(error) => return Ok(failed(DraftApplyStage::Prepare, error.into(), None, false)),
        };
        let receipt_id = transaction.id().to_owned();
        let mut auth_before = None;
        if let Some(tool) = auth_tool {
            match crate::official_auth::read_live(tool) {
                Ok(value) => auth_before = Some(value),
                Err(error) => {
                    transaction.rollback()?;
                    return Ok(failed(
                        DraftApplyStage::Prepare,
                        error.into(),
                        Some(receipt_id),
                        false,
                    ));
                }
            }
            let result = (|| {
                if let Some(outgoing) = context
                    .before_providers
                    .iter()
                    .find(|p| {
                        Some(p.id.as_str())
                            == context
                                .before_local_current
                                .as_deref()
                                .or(context.before_db_current.as_deref())
                    })
                    .filter(|p| crate::official_auth::is_official_card(p))
                {
                    crate::official_auth::capture_live_to_card(tool, &outgoing.id)?;
                }
                if crate::official_auth::is_official_card(&provider) {
                    crate::official_auth::apply_card_to_live(tool, &provider.id)?;
                }
                Ok::<_, AppError>(())
            })();
            if let Err(error) = result {
                let restored = restore_auth(tool, auth_before.as_ref().and_then(Option::as_ref));
                let files = transaction.rollback();
                let recovery = restored.is_err() || files.is_err();
                if recovery {
                    let _ = transaction.mark_recovery_required();
                }
                return Ok(failed(
                    DraftApplyStage::Write,
                    error.into(),
                    Some(receipt_id),
                    recovery,
                ));
            }
        }
        if let Err(error) = transaction.apply() {
            let mut recovery = transaction.state() == ConfigApplyState::RecoveryRequired;
            if let Some(tool) = auth_tool
                && restore_auth(tool, auth_before.as_ref().and_then(Option::as_ref)).is_err()
            {
                recovery = true;
                let _ = transaction.mark_recovery_required();
            }
            return Ok(failed(
                DraftApplyStage::Write,
                error.into(),
                Some(receipt_id),
                recovery,
            ));
        }
        let commit = (|| {
            if context.switch
                && let Some(builtin) = AppType::from_app_id(app)
            {
                crate::settings::set_current_provider(&builtin, Some(&provider.id))?;
            }
            self.state.db.commit_editor_application(
                &draft,
                &provider,
                &receipt_id,
                context.switch,
                &context.after_providers,
                &context.before_providers,
            )
        })();
        if let Err(error) = commit {
            let files = transaction.rollback();
            let local = if context.switch {
                AppType::from_app_id(app)
                    .map(|builtin| {
                        crate::settings::set_current_provider(
                            &builtin,
                            context.before_local_current.as_deref(),
                        )
                    })
                    .transpose()
            } else {
                Ok(None)
            };
            let auth = auth_tool
                .map(|tool| restore_auth(tool, auth_before.as_ref().and_then(Option::as_ref)))
                .transpose();
            let recovery = files.is_err() || local.is_err() || auth.is_err();
            if recovery {
                let _ = transaction.mark_recovery_required();
            }
            return Ok(failed(
                DraftApplyStage::Commit,
                error,
                Some(receipt_id),
                recovery,
            ));
        }
        let mut warnings = Vec::new();
        if let Err(error) = transaction.complete() {
            warnings.push(format!(
                "Configuration committed; receipt completion needs recovery: {error}"
            ));
        }
        if let Some(builtin) = AppType::from_app_id(app) {
            crate::services::provider::drift::record_snapshot(&builtin, &provider.id);
        }
        if outside_side_effects {
            warnings.push("Automatic undo is unavailable because this operation also uses account or gateway state.".into());
        }
        Ok(EditorDraftApplyResult {
            state: DraftApplyState::Applied,
            draft_id: id.into(),
            draft_revision: revision,
            provider: Some(ProviderDetails {
                app: app.to_string(),
                provider,
            }),
            receipt_id: Some(receipt_id),
            undo_available: !outside_side_effects
                && transaction.state() == ConfigApplyState::Completed,
            warnings,
            failure: None,
        })
    }

    #[allow(clippy::type_complexity)]
    fn prepare_draft_application(
        &self,
        app: &AppId,
        draft: &EditorDraft,
        policy: ProviderSwitchPolicy,
        validate_editor: bool,
    ) -> ApplicationResult<(
        Provider,
        Option<crate::services::config_commit::PreparedConfigWrite>,
        Option<Provider>,
        Option<crate::official_auth::OfficialTool>,
        ApplyContext,
    )> {
        crate::plugin::registry::ensure_app_enabled(app)?;
        let plugin = crate::plugin::get_plugin(app).ok_or_else(|| ApplicationError::NotFound {
            kind: "app",
            id: app.to_string(),
        })?;
        if let Some(expected) = draft
            .input
            .editor_state
            .get("pluginRevision")
            .and_then(Value::as_str)
            && plugin.configuration_revision() != Some(expected)
        {
            return Err(ApplicationError::ResourceConflict(
                "plugin changed while editing; reopen the draft with the updated plugin".into(),
            ));
        }
        let builtin = AppType::from_app_id(app);
        let original = draft
            .input
            .original_provider_id
            .as_deref()
            .map(|id| self.state.db.get_provider_by_id(id, app.as_str()))
            .transpose()?
            .flatten();
        if original.as_ref().map(provider_revision_hash) != draft.input.base_provider_hash {
            return Err(ApplicationError::ResourceConflict(
                "source connection changed while editing; reopen it before applying".into(),
            ));
        }
        let mut provider = draft.input.provider.clone();
        if draft.input.original_provider_id.as_deref() != Some(&provider.id)
            && self.state.db.provider_exists(app.as_str(), &provider.id)?
        {
            return Err(ApplicationError::AlreadyExists {
                kind: "provider",
                id: provider.id,
            });
        }
        let codec =
            plugin
                .provider_config()
                .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                    app: app.to_string(),
                    capability: "provider.edit",
                })?;
        if let Some(documents) = &draft.input.raw_documents {
            let parsed = codec.parse_files(documents).map_err(|message| {
                ApplicationError::ValidationFailed {
                    message,
                    details: json!({"source":"rawDocuments"}),
                }
            })?;
            if plugin.is_user_manifest() {
                if let (Some(prior), Some(parsed)) =
                    (provider.settings_config.as_object_mut(), parsed.as_object())
                {
                    prior.extend(parsed.clone());
                } else {
                    provider.settings_config = parsed;
                }
            } else {
                provider.settings_config = parsed;
            }
        } else if let Some(values) = draft
            .input
            .editor_state
            .get("values")
            .and_then(Value::as_object)
        {
            let mut values = values.clone();
            if builtin == Some(AppType::KimiCode)
                && provider.category.as_deref() == Some("official")
            {
                crate::provider_config::apply_official_defaults(&mut values);
                let encoded =
                    codec.encode(&values, &provider.settings_config, provider.meta.as_ref());
                provider.settings_config = encoded.settings_config;
                provider.meta = encoded.meta;
            }
            let issues = codec
                .validate_for_category(&values, provider.category.as_deref())
                .into_iter()
                .filter(|issue| issue.severity == crate::provider_config::Severity::Error)
                .map(|issue| issue.message)
                .collect::<Vec<_>>();
            if !issues.is_empty() {
                return Err(ApplicationError::ValidationFailed {
                    message: "configuration fields are invalid".into(),
                    details: json!({"issues":issues}),
                });
            }
        }
        if let Some(original) = &original {
            restore_secrets(builtin, codec.as_ref(), &mut provider, original);
        }
        if let Some(builtin) = builtin {
            crate::services::provider::ProviderService::normalize_provider_if_claude(
                &builtin,
                &mut provider,
            );
            if validate_editor {
                crate::services::provider::ProviderService::validate_provider_settings(
                    &builtin, &provider,
                )?;
            }
            crate::services::provider::live::normalize_provider_common_config_for_storage(
                &self.state.db,
                &builtin,
                &mut provider,
            )?;
        }
        if validate_editor {
            super::providers::validate_provider_with_plugin(plugin.as_ref(), &provider)?;
        }
        let common_before = self.state.db.get_config_snippet(app.as_str())?;
        if let Some(common) = &draft.input.common_config {
            if !matches!(builtin, Some(AppType::Claude | AppType::Codex)) {
                return Err(ApplicationError::CapabilityUnsupported {
                    app: app.to_string(),
                    capability: "common-config",
                });
            }
            if common.base_snippet != common_before {
                return Err(ApplicationError::ResourceConflict(
                    "shared configuration changed while editing".into(),
                ));
            }
            crate::services::ConfigService::validate_common_config_snippet(
                app.as_str(),
                &common.snippet,
            )?;
            if provider
                .meta
                .as_ref()
                .and_then(|meta| meta.common_config_enabled)
                .is_none()
                && let Some(builtin) = builtin
            {
                let enabled = crate::services::provider::live::provider_uses_common_config(
                    &builtin,
                    &provider,
                    common_before.as_deref(),
                );
                provider
                    .meta
                    .get_or_insert_with(Default::default)
                    .common_config_enabled = Some(enabled);
                crate::services::provider::live::normalize_provider_common_config_for_storage(
                    &self.state.db,
                    &builtin,
                    &mut provider,
                )?;
            }
        }
        let switch = plugin.mode() == AppMode::Switch;
        if !switch {
            provider
                .meta
                .get_or_insert_with(Default::default)
                .live_config_managed = Some(true);
        }
        let mut effective = provider.clone();
        if let Some(builtin) = builtin {
            let snippet = draft
                .input
                .common_config
                .as_ref()
                .map(|c| c.snippet.as_str())
                .or(common_before.as_deref());
            if crate::services::provider::live::provider_uses_common_config(
                &builtin, &provider, snippet,
            ) && let Some(snippet) = snippet
            {
                effective.settings_config =
                    crate::services::provider::live::apply_common_config_to_settings(
                        &builtin,
                        &provider.settings_config,
                        snippet,
                    )?;
            }
        }
        let plan = match builtin {
            Some(app) => crate::services::config_plan::builtin_plan(app, &effective)?,
            None => plugin.live().prepare_write(&self.state.db, &effective)?,
        };
        let before_db_current = self.state.db.get_current_provider(app.as_str())?;
        let before_local_current =
            builtin.and_then(|app| crate::settings::get_current_provider(&app));
        let current_id = if switch {
            before_local_current
                .clone()
                .or_else(|| before_db_current.clone())
        } else {
            None
        };
        let outgoing = current_id
            .as_deref()
            .map(|id| self.state.db.get_provider_by_id(id, app.as_str()))
            .transpose()?
            .flatten();
        let mut outgoing_saved = None;
        let mut drift = crate::services::provider::LiveDrift::default();
        let mut live = None;
        if switch {
            if let Some(builtin) = builtin {
                let preview = crate::services::provider::live::preview_current_live_change(
                    &self.state.db,
                    &builtin,
                    outgoing.as_ref(),
                )?;
                drift = preview.drift;
                if !drift.is_empty() {
                    live = Some(preview.live);
                }
            } else {
                let current_live = plugin.live().read_live()?;
                let changed = outgoing
                    .as_ref()
                    .map(|p| p.settings_config != current_live)
                    .unwrap_or_else(|| !super::providers::empty_live_config(&current_live));
                if changed {
                    drift
                        .conflicts
                        .push(crate::services::provider::DriftConflict {
                            path: "$".into(),
                            live: current_live.clone(),
                            incoming: outgoing
                                .as_ref()
                                .map(|p| p.settings_config.clone())
                                .unwrap_or(Value::Null),
                        });
                    live = Some(current_live);
                }
            }
        }
        if !drift.is_empty() {
            if policy == ProviderSwitchPolicy::Abort {
                return Err(ApplicationError::ConfigDrift {
                    app: app.to_string(),
                    path: plugin.config_dir()?.display().to_string(),
                    drift: Box::new(drift),
                });
            }
            if policy == ProviderSwitchPolicy::Save
                && let (Some(outgoing), Some(live)) = (outgoing.as_ref(), live)
            {
                let mut saved = outgoing.clone();
                saved.settings_config = live;
                if let Some(builtin) = builtin {
                    crate::services::provider::live::normalize_provider_common_config_for_storage(
                        &self.state.db,
                        &builtin,
                        &mut saved,
                    )?;
                }
                outgoing_saved = Some(saved);
            }
        }
        if let (Some(original), Some(builtin)) = (original.as_ref(), builtin)
            && provider.id != original.id
            && (switch
                || original
                    .meta
                    .as_ref()
                    .and_then(|m| m.live_config_managed)
                    .unwrap_or(false)
                || crate::services::provider::live::provider_exists_in_live_config(
                    &builtin,
                    &original.id,
                )?)
        {
            return Err(ApplicationError::ResourceConflict(
                "an applied provider key cannot be renamed".into(),
            ));
        }
        let auth_tool = builtin
            .and_then(crate::official_auth::OfficialTool::from_app)
            .filter(|_| {
                crate::official_auth::is_official_card(&provider)
                    || outgoing
                        .as_ref()
                        .is_some_and(crate::official_auth::is_official_card)
            });
        let mut before_providers = Vec::new();
        if let Some(original) = original {
            before_providers.push(original);
        }
        if let Some(outgoing) = outgoing
            && !before_providers.iter().any(|p| p.id == outgoing.id)
        {
            before_providers.push(outgoing);
        }
        if let Some(db_current) = before_db_current.as_deref()
            && !before_providers.iter().any(|p| p.id == db_current)
            && let Some(provider) = self.state.db.get_provider_by_id(db_current, app.as_str())?
        {
            before_providers.push(provider);
        }
        let mut after_providers = Vec::new();
        if let Some(outgoing) = &outgoing_saved
            && outgoing.id != provider.id
        {
            after_providers.push(outgoing.clone());
        }
        if draft.input.common_config.is_some()
            && let Some(builtin) = builtin
        {
            for mut legacy in self.state.db.get_all_providers(app.as_str())?.into_values() {
                if legacy.id == provider.id
                    || after_providers.iter().any(|p| p.id == legacy.id)
                    || legacy
                        .meta
                        .as_ref()
                        .and_then(|m| m.common_config_enabled)
                        .is_some()
                {
                    continue;
                }
                if crate::services::provider::live::provider_uses_common_config(
                    &builtin,
                    &legacy,
                    common_before.as_deref(),
                ) {
                    if !before_providers.iter().any(|p| p.id == legacy.id) {
                        before_providers.push(legacy.clone());
                    }
                    legacy
                        .meta
                        .get_or_insert_with(Default::default)
                        .common_config_enabled = Some(true);
                    crate::services::provider::live::normalize_provider_common_config_for_storage(
                        &self.state.db,
                        &builtin,
                        &mut legacy,
                    )?;
                    after_providers.push(legacy);
                }
            }
        }
        after_providers.push(provider.clone());
        let omo = builtin == Some(AppType::OpenCode)
            && matches!(provider.category.as_deref(), Some("omo" | "omo-slim"));
        let target_id = if switch {
            "*".into()
        } else if omo {
            "omo-profiles".into()
        } else {
            provider.id.clone()
        };
        let context = ApplyContext {
            app: app.to_string(),
            provider_id: provider.id.clone(),
            target_id: target_id.clone(),
            draft: draft.clone(),
            before_providers,
            after_providers,
            before_db_current,
            before_local_current,
            previous_head: self.state.db.config_apply_head(app.as_str(), &target_id)?,
            common_before,
            common_cleared_before: self.state.db.is_config_snippet_cleared(app.as_str())?,
            switch,
            omo,
        };
        Ok((provider, plan, outgoing_saved, auth_tool, context))
    }

    pub fn get_config_apply_receipt(&self, id: &str) -> ApplicationResult<ConfigApplyReceipt> {
        Ok(FileCommitTransaction::load(id)?.receipt())
    }

    pub fn undo_config_apply(&self, id: &str) -> ApplicationResult<ConfigApplyReceipt> {
        let _guard = ConfigMutationGuard::acquire()?;
        self.recover_pending_config_commits_locked()?;
        let mut original = FileCommitTransaction::load(id)?;
        if !original.receipt().undo_available {
            return Err(ApplicationError::CapabilityUnsupported {
                app: "config".into(),
                capability: "config.undo",
            });
        }
        let context: ApplyContext = serde_json::from_value(original.context().clone())
            .map_err(|e| ApplicationError::InvalidInput(e.to_string()))?;
        self.verify_undo_context(&context, id)?;
        let plan = original
            .reverse_plan()
            .map_err(|e| ApplicationError::ResourceConflict(e.to_string()))?;
        let undo_context = json!({"undoOf":id,"app":context.app,"providerId":context.provider_id});
        let mut reverse = FileCommitTransaction::prepare(&plan, undo_context, false)?;
        reverse.apply()?;
        let result = (|| {
            if context.switch
                && let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?)
            {
                crate::settings::set_current_provider(
                    &app,
                    context.before_local_current.as_deref(),
                )?;
            }
            self.state.db.restore_editor_application(&context, id)
        })();
        if let Err(error) = result {
            let rollback = reverse.rollback();
            let local_restore = if context.switch
                && let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?)
            {
                crate::settings::set_current_provider(&app, Some(&context.provider_id))
            } else {
                Ok(())
            };
            if rollback.is_err() || local_restore.is_err() {
                let _ = reverse.mark_recovery_required();
                return Err(ApplicationError::RecoveryRequired(format!(
                    "undo receipt {}: {error}",
                    reverse.id()
                )));
            }
            return Err(error);
        }
        reverse.complete()?;
        original.mark_rolled_back()?;
        if let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?) {
            if let Some(current) = context
                .before_local_current
                .as_deref()
                .or(context.before_db_current.as_deref())
            {
                crate::services::provider::drift::record_snapshot(&app, current);
            } else {
                crate::services::provider::drift::clear_snapshot(&app)?;
            }
        }
        Ok(original.receipt())
    }

    fn verify_undo_context(&self, context: &ApplyContext, id: &str) -> ApplicationResult<()> {
        if self
            .state
            .db
            .config_apply_head(&context.app, &context.target_id)?
            .as_deref()
            != Some(id)
        {
            return Err(ApplicationError::ResourceConflict(
                "a newer application replaced this receipt".into(),
            ));
        }
        for expected in &context.after_providers {
            let current = self
                .state
                .db
                .get_provider_by_id(&expected.id, &context.app)?;
            if current.as_ref().map(provider_revision_hash)
                != Some(provider_revision_hash(expected))
            {
                return Err(ApplicationError::ResourceConflict(
                    "connection changed after this application".into(),
                ));
            }
        }
        for before in &context.before_providers {
            if context.after_providers.iter().any(|p| p.id == before.id) {
                continue;
            }
            let current = self.state.db.get_provider_by_id(&before.id, &context.app)?;
            let removed = context.draft.input.original_provider_id.as_deref() == Some(&before.id)
                && before.id != context.provider_id;
            let expected = (!removed).then(|| provider_revision_hash(before));
            if current.as_ref().map(provider_revision_hash) != expected {
                return Err(ApplicationError::ResourceConflict(
                    "a connection that undo would restore has changed".into(),
                ));
            }
        }
        if context.switch || context.omo {
            if self.state.db.get_current_provider(&context.app)?.as_deref()
                != Some(&context.provider_id)
            {
                return Err(ApplicationError::ResourceConflict(
                    "current connection changed after this application".into(),
                ));
            }
            if context.switch
                && let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?)
                && crate::settings::get_current_provider(&app).as_deref()
                    != Some(&context.provider_id)
            {
                return Err(ApplicationError::ResourceConflict(
                    "device current connection changed after this application".into(),
                ));
            }
        }
        if let Some(common) = &context.draft.input.common_config {
            let expected = (!common.snippet.trim().is_empty()).then(|| common.snippet.clone());
            if self.state.db.get_config_snippet(&context.app)? != expected {
                return Err(ApplicationError::ResourceConflict(
                    "shared configuration changed after this application".into(),
                ));
            }
        }
        Ok(())
    }

    /// Finish or roll back interrupted ordinary file commits without guessing about external state.
    pub fn recover_config_commits(&self) -> ApplicationResult<Vec<ConfigApplyReceipt>> {
        let _guard = ConfigMutationGuard::acquire()?;
        self.recover_pending_config_commits_locked()
    }

    fn recover_pending_config_commits_locked(&self) -> ApplicationResult<Vec<ConfigApplyReceipt>> {
        let mut recovered = Vec::new();
        for id in pending_receipts()? {
            let mut transaction = FileCommitTransaction::load(&id)?;
            if transaction.state() == ConfigApplyState::RecoveryRequired {
                return Err(ApplicationError::RecoveryRequired(format!(
                    "configuration receipt {id} requires inspection"
                )));
            }
            if let Some(undo_id) = transaction.context().get("undoOf").and_then(Value::as_str) {
                let mut original = FileCommitTransaction::load(undo_id)?;
                let context: ApplyContext = serde_json::from_value(original.context().clone())
                    .map_err(|e| ApplicationError::InvalidInput(e.to_string()))?;
                if self
                    .state
                    .db
                    .config_apply_head(&context.app, &context.target_id)?
                    .as_deref()
                    == Some(undo_id)
                {
                    transaction.rollback()?;
                    if context.switch
                        && let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?)
                        && let Err(error) =
                            crate::settings::set_current_provider(&app, Some(&context.provider_id))
                    {
                        let _ = transaction.mark_recovery_required();
                        return Err(ApplicationError::RecoveryRequired(format!(
                            "undo recovery {id}: {error}"
                        )));
                    }
                } else {
                    transaction.verify_after()?;
                    transaction.complete()?;
                    original.mark_rolled_back()?;
                }
            } else if let Ok(context) =
                serde_json::from_value::<ApplyContext>(transaction.context().clone())
            {
                if self
                    .state
                    .db
                    .config_apply_head(&context.app, &context.target_id)?
                    .as_deref()
                    == Some(&id)
                {
                    transaction.verify_after()?;
                    transaction.complete()?;
                } else {
                    // Official account operations cannot be safely reconstructed from file bytes alone.
                    if !transaction.journal_undo_capable() {
                        let _ = transaction.mark_recovery_required();
                        return Err(ApplicationError::RecoveryRequired(format!(
                            "account or gateway operation {id} requires inspection"
                        )));
                    }
                    transaction.rollback()?;
                    if context.switch
                        && let Some(app) = AppType::from_app_id(&AppId::parse(&context.app)?)
                    {
                        crate::settings::set_current_provider(
                            &app,
                            context.before_local_current.as_deref(),
                        )?;
                    }
                }
            } else {
                transaction.rollback()?;
            }
            recovered.push(transaction.receipt());
        }
        Ok(recovered)
    }
}

fn restore_auth(
    tool: crate::official_auth::OfficialTool,
    before: Option<&Value>,
) -> Result<(), AppError> {
    match before {
        Some(blob) => crate::official_auth::write_live(tool, blob),
        None => crate::official_auth::clear_live(tool),
    }
}

pub(crate) fn restore_provider_input_secrets(
    builtin: Option<AppType>,
    plugin: &dyn crate::plugin::AppPlugin,
    incoming: &mut Provider,
    existing: &Provider,
) {
    if let Some(codec) = plugin.provider_config() {
        restore_secrets(builtin, codec.as_ref(), incoming, existing);
    }
}

fn restore_secrets(
    builtin: Option<AppType>,
    codec: &dyn crate::provider_config::AppConfig,
    incoming: &mut Provider,
    existing: &Provider,
) {
    if let Some(app) = builtin {
        crate::services::provider::secrets::restore_masked_secrets(&app, incoming, existing);
    } else {
        crate::services::provider::secrets::restore_generic_masked_secrets(incoming, existing);
    }
    for path in codec.secret_paths() {
        let pointer = path
            .iter()
            .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
            .collect::<String>();
        if incoming
            .settings_config
            .pointer(&pointer)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c == '*'))
            && let (Some(target), Some(source)) = (
                incoming.settings_config.pointer_mut(&pointer),
                existing.settings_config.pointer(&pointer),
            )
        {
            *target = source.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{EditorCommonConfig, EditorDraftInput};
    use std::sync::Arc;

    fn application() -> Application {
        Application::from_state(Arc::new(crate::AppState::new(Arc::new(
            crate::Database::memory().unwrap(),
        ))))
    }
    fn input(id: &str) -> EditorDraftInput {
        EditorDraftInput {
            provider: Provider::with_id(
                id.into(),
                format!("Connection {id}"),
                json!({"env":{"ANTHROPIC_BASE_URL":"https://example.invalid","ANTHROPIC_AUTH_TOKEN":"synthetic"}}),
                None,
            ),
            original_provider_id: None,
            base_provider_hash: None,
            editor_state: Value::Null,
            raw_documents: None,
            common_config: None,
        }
    }
    #[test]
    fn invalid_draft_is_saved_without_touching_provider_or_live() {
        let home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        let mut input = input("one");
        input.provider.name.clear();
        input.raw_documents = Some(vec!["{ incomplete".into()]);
        let draft = app.upsert_editor_draft(&id, "one", None, input).unwrap();
        assert_eq!(draft.revision, 1);
        assert_eq!(app.list_editor_drafts(&id).unwrap().len(), 1);
        assert!(!app.state.db.provider_exists("claude", "one").unwrap());
        assert!(!home.path().join(".claude/settings.json").exists());
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(result.state, DraftApplyState::Failed);
        assert_eq!(
            app.get_editor_draft(&id, "one")
                .unwrap()
                .unwrap()
                .input
                .raw_documents
                .unwrap()[0],
            "{ incomplete"
        );
    }
    #[test]
    fn draft_revision_conflict_keeps_newer_content() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        let draft = app
            .upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        let mut next = draft.input.clone();
        next.provider.name = "newer".into();
        app.upsert_editor_draft(&id, "one", Some(1), next).unwrap();
        assert!(
            app.upsert_editor_draft(&id, "one", Some(1), draft.input)
                .is_err()
        );
        assert!(app.delete_editor_draft(&id, "one", 1).is_err());
        assert_eq!(
            app.get_editor_draft(&id, "one")
                .unwrap()
                .unwrap()
                .input
                .provider
                .name,
            "newer"
        );
    }
    #[test]
    fn apply_and_undo_preserve_exact_bytes_and_restore_draft() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(
            result.state,
            DraftApplyState::Applied,
            "{:?}",
            result.failure
        );
        assert!(result.undo_available);
        assert!(app.get_editor_draft(&id, "one").unwrap().is_none());
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("one")
        );
        let receipt = app
            .undo_config_apply(result.receipt_id.as_deref().unwrap())
            .unwrap();
        assert_eq!(receipt.state, ConfigApplyState::RolledBack);
        assert!(!crate::paths::get_claude_settings_path().exists());
        assert!(!app.state.db.provider_exists("claude", "one").unwrap());
        assert!(crate::settings::get_current_provider(&AppType::Claude).is_none());
        assert!(app.get_editor_draft(&id, "one").unwrap().is_some());
    }
    #[test]
    fn undo_refuses_external_change() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(result.state, DraftApplyState::Applied);
        std::fs::write(
            crate::paths::get_claude_settings_path(),
            b"{\"outside\":true}",
        )
        .unwrap();
        assert!(
            app.undo_config_apply(result.receipt_id.as_deref().unwrap())
                .is_err()
        );
        assert_eq!(
            std::fs::read(crate::paths::get_claude_settings_path()).unwrap(),
            b"{\"outside\":true}"
        );
        assert!(app.state.db.provider_exists("claude", "one").unwrap());
    }
    #[test]
    fn database_commit_failure_rolls_back_files_and_current_and_keeps_draft() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        app.state.db.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_apply BEFORE INSERT ON providers BEGIN SELECT RAISE(ABORT,'injected provider failure'); END;").unwrap();
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(result.state, DraftApplyState::Failed);
        assert_eq!(result.failure.unwrap().stage, DraftApplyStage::Commit);
        assert!(!crate::paths::get_claude_settings_path().exists());
        assert!(crate::settings::get_current_provider(&AppType::Claude).is_none());
        assert!(app.get_editor_draft(&id, "one").unwrap().is_some());
    }
    #[test]
    fn shared_snippet_commits_and_undo_restores_absence() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        let mut input = input("one");
        input
            .provider
            .meta
            .get_or_insert_with(Default::default)
            .common_config_enabled = Some(true);
        input.common_config = Some(EditorCommonConfig {
            snippet: "{\"alwaysThinkingEnabled\":true}".into(),
            base_snippet: None,
        });
        app.upsert_editor_draft(&id, "one", None, input).unwrap();
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(
            result.state,
            DraftApplyState::Applied,
            "{:?}",
            result.failure
        );
        assert!(app.state.db.get_config_snippet("claude").unwrap().is_some());
        let live: Value = serde_json::from_slice(
            &std::fs::read(crate::paths::get_claude_settings_path()).unwrap(),
        )
        .unwrap();
        assert_eq!(live["alwaysThinkingEnabled"], true);
        app.undo_config_apply(result.receipt_id.as_deref().unwrap())
            .unwrap();
        assert!(app.state.db.get_config_snippet("claude").unwrap().is_none());
    }
    #[test]
    fn source_revision_conflict_does_not_write_live() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        let original = input("one").provider;
        app.state.db.save_provider("claude", &original).unwrap();
        let original = app
            .state
            .db
            .get_provider_by_id("one", "claude")
            .unwrap()
            .unwrap();
        let mut edit = input("one");
        edit.original_provider_id = Some("one".into());
        edit.base_provider_hash = Some(provider_revision_hash(&original));
        app.upsert_editor_draft(&id, "one", None, edit).unwrap();
        let mut newer = original;
        newer.name = "changed elsewhere".into();
        app.state.db.save_provider("claude", &newer).unwrap();
        let result = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(result.state, DraftApplyState::Failed);
        assert_eq!(result.failure.unwrap().stage, DraftApplyStage::Conflict);
        assert!(!crate::paths::get_claude_settings_path().exists());
    }
    #[test]
    fn local_drafts_are_excluded_from_sync_export() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        let mut input = input("one");
        input.editor_state = json!({"privateDraft":"local-secret-marker-only"});
        app.upsert_editor_draft(&id, "one", None, input).unwrap();
        let exported = app.state.db.export_sql_string_for_sync().unwrap();
        assert!(!exported.contains("local-secret-marker-only"));
        assert!(!exported.contains("INSERT INTO \"editor_drafts\""));
    }

    #[test]
    fn retry_of_completed_apply_reuses_receipt() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        let first = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        let retried = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(retried.state, DraftApplyState::Applied);
        assert_eq!(retried.receipt_id, first.receipt_id);
        assert_eq!(app.state.db.get_all_providers("claude").unwrap().len(), 1);
    }
    #[test]
    fn undo_refuses_to_overwrite_an_inactive_connection_edited_later() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        app.apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        app.upsert_editor_draft(&id, "two", None, input("two"))
            .unwrap();
        let switched = app
            .apply_editor_draft(&id, "two", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        assert_eq!(switched.state, DraftApplyState::Applied);
        let mut one = app
            .state
            .db
            .get_provider_by_id("one", "claude")
            .unwrap()
            .unwrap();
        one.name = "later edit".into();
        app.state.db.save_provider("claude", &one).unwrap();
        assert!(
            app.undo_config_apply(switched.receipt_id.as_deref().unwrap())
                .is_err()
        );
        assert_eq!(
            app.state
                .db
                .get_provider_by_id("one", "claude")
                .unwrap()
                .unwrap()
                .name,
            "later edit"
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("two")
        );
    }
    #[test]
    fn interrupted_undo_restores_files_and_device_current_together() {
        let _home = crate::test_support::TestHome::new();
        let app = application();
        let id = AppType::Claude.app_id();
        app.upsert_editor_draft(&id, "one", None, input("one"))
            .unwrap();
        let applied = app
            .apply_editor_draft(&id, "one", 1, ProviderSwitchPolicy::Abort)
            .unwrap();
        let receipt = FileCommitTransaction::load(applied.receipt_id.as_deref().unwrap()).unwrap();
        let plan = receipt.reverse_plan().unwrap();
        let mut reverse = FileCommitTransaction::prepare(
            &plan,
            json!({"undoOf":receipt.id(),"app":"claude","providerId":"one"}),
            false,
        )
        .unwrap();
        reverse.apply().unwrap();
        crate::settings::set_current_provider(&AppType::Claude, None).unwrap();
        drop(reverse);
        app.recover_config_commits().unwrap();
        assert!(crate::paths::get_claude_settings_path().exists());
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("one")
        );
        assert_eq!(
            app.state
                .db
                .get_current_provider("claude")
                .unwrap()
                .as_deref(),
            Some("one")
        );
    }
}
