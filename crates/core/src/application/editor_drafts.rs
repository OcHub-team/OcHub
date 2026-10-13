//! Device-local editing documents. Saving one never changes a provider or live file.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Application, ApplicationError, ApplicationResult, ProviderDetails};
use crate::{AppId, Provider};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorDraftInput {
    pub provider: Provider,
    #[serde(default)]
    pub original_provider_id: Option<String>,
    #[serde(default)]
    pub base_provider_hash: Option<String>,
    #[serde(default)]
    pub editor_state: Value,
    /// Unmerged native documents, in codec preview order. Invalid text is valid draft data.
    #[serde(default)]
    pub raw_documents: Option<Vec<String>>,
    #[serde(default)]
    pub common_config: Option<EditorCommonConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorCommonConfig {
    pub snippet: String,
    pub base_snippet: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorDraft {
    pub app: String,
    pub id: String,
    pub revision: u64,
    pub updated_at: i64,
    pub input: EditorDraftInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorDraftSummary {
    pub app: String,
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub original_provider_id: Option<String>,
    pub revision: u64,
    pub updated_at: i64,
}

impl From<&EditorDraft> for EditorDraftSummary {
    fn from(draft: &EditorDraft) -> Self {
        Self {
            app: draft.app.clone(),
            id: draft.id.clone(),
            provider_id: draft.input.provider.id.clone(),
            name: draft.input.provider.name.clone(),
            original_provider_id: draft.input.original_provider_id.clone(),
            revision: draft.revision,
            updated_at: draft.updated_at,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DraftApplyState {
    Applied,
    ExternalConfirmation,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DraftApplyStage {
    Validation,
    Conflict,
    Prepare,
    Write,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftApplyFailure {
    pub stage: DraftApplyStage,
    pub code: String,
    pub message: String,
    pub recovery_required: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorDraftApplyResult {
    pub state: DraftApplyState,
    pub draft_id: String,
    pub draft_revision: u64,
    pub provider: Option<ProviderDetails>,
    pub receipt_id: Option<String>,
    pub undo_available: bool,
    pub warnings: Vec<String>,
    pub failure: Option<DraftApplyFailure>,
}

/// A deterministic revision of the full private record, never a substitute for a secret.
pub fn provider_revision_hash(provider: &Provider) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .map(|(k, v)| (k.clone(), canonical(v)))
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    let mut provider = provider.clone();
    provider.meta.get_or_insert_with(Default::default);
    let value = serde_json::to_value(provider).expect("Provider serialization is infallible");
    let bytes =
        serde_json::to_vec(&canonical(&value)).expect("JSON Value serialization is infallible");
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Application {
    pub fn list_editor_drafts(&self, app: &AppId) -> ApplicationResult<Vec<EditorDraftSummary>> {
        Ok(self.state.db.list_editor_drafts(app.as_str())?)
    }

    pub fn get_editor_draft(
        &self,
        app: &AppId,
        id: &str,
    ) -> ApplicationResult<Option<EditorDraft>> {
        Ok(self.state.db.get_editor_draft(app.as_str(), id)?)
    }

    pub fn upsert_editor_draft(
        &self,
        app: &AppId,
        id: &str,
        expected_revision: Option<u64>,
        input: EditorDraftInput,
    ) -> ApplicationResult<EditorDraft> {
        if id.trim().is_empty() || id.len() > 512 {
            return Err(ApplicationError::InvalidInput(
                "draft id must contain 1–512 bytes".into(),
            ));
        }
        // No codec validation here: incomplete forms and invalid raw text must survive.
        self.state
            .db
            .upsert_editor_draft(app.as_str(), id, expected_revision, input)
    }

    pub fn delete_editor_draft(
        &self,
        app: &AppId,
        id: &str,
        expected_revision: u64,
    ) -> ApplicationResult<()> {
        self.state
            .db
            .delete_editor_draft(app.as_str(), id, expected_revision)
    }
}
