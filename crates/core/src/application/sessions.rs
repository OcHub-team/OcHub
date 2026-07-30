use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::session_index::{IndexStats, MaintenanceOutcome, SearchHit, SessionIndex, SyncOutcome};
use crate::session_manager::{
    DeleteSessionOutcome, DeleteSessionRequest, SessionMessage, SessionMeta,
    ToolInstallationReport, ToolVersion,
};
use crate::{AppId, AppType};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

impl Application {
    pub fn list_sessions(
        &self,
        apps: &[AppId],
        query: Option<&str>,
    ) -> ApplicationResult<Vec<SessionMeta>> {
        let app_ids = apps
            .iter()
            .map(|app| {
                let app_type = AppType::from_app_id(app).ok_or_else(|| {
                    ApplicationError::CapabilityUnsupported {
                        app: app.to_string(),
                        capability: "sessions",
                    }
                })?;
                if matches!(app_type, AppType::ClaudeDesktop) {
                    return Err(ApplicationError::CapabilityUnsupported {
                        app: app.to_string(),
                        capability: "sessions",
                    });
                }
                Ok(app_type.as_str())
            })
            .collect::<ApplicationResult<Vec<_>>>()?;
        let query = query
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);

        Ok(crate::session_manager::scan_sessions()
            .into_iter()
            .filter(|session| app_ids.is_empty() || app_ids.contains(&session.provider_id.as_str()))
            .filter(|session| {
                let Some(query) = query.as_deref() else {
                    return true;
                };
                [
                    Some(session.session_id.as_str()),
                    session.title.as_deref(),
                    session.summary.as_deref(),
                    session.project_dir.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|value| value.to_lowercase().contains(query))
            })
            .collect())
    }

    pub fn get_session(&self, app: &AppId, id: &str) -> ApplicationResult<SessionMeta> {
        self.list_sessions(std::slice::from_ref(app), None)?
            .into_iter()
            .find(|session| session.session_id == id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "session",
                id: format!("{app}/{id}"),
            })
    }

    pub fn get_session_messages(
        &self,
        app: &AppId,
        id: &str,
    ) -> ApplicationResult<(SessionMeta, Vec<SessionMessage>)> {
        let session = self.get_session(app, id)?;
        let source_path = session.source_path.as_deref().ok_or_else(|| {
            ApplicationError::InvalidInput(format!(
                "session {app}/{id} has no readable source path"
            ))
        })?;
        let messages = crate::session_manager::load_messages(&session.provider_id, source_path)
            .map_err(ApplicationError::OperationFailed)?;
        Ok((session, messages))
    }

    pub fn delete_session(&self, app: &AppId, id: &str) -> ApplicationResult<DeleteSessionOutcome> {
        let session = self.get_session(app, id)?;
        let source_path = session.source_path.ok_or_else(|| {
            ApplicationError::InvalidInput(format!(
                "session {app}/{id} has no deletable source path"
            ))
        })?;
        let request = DeleteSessionRequest {
            provider_id: session.provider_id,
            session_id: session.session_id,
            source_path,
        };
        let outcome = crate::session_manager::delete_sessions(&[request])
            .into_iter()
            .next()
            .expect("one delete request always produces one outcome");
        if outcome.success {
            Ok(outcome)
        } else {
            Err(ApplicationError::OperationFailed(
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "session was not deleted".to_string()),
            ))
        }
    }

    pub fn delete_sessions(
        &self,
        requests: &[DeleteSessionRequest],
    ) -> ApplicationResult<Vec<DeleteSessionOutcome>> {
        if requests.is_empty() {
            return Err(ApplicationError::InvalidInput(
                "session delete batch cannot be empty".to_string(),
            ));
        }
        let outcomes = crate::session_manager::delete_sessions(requests);
        if outcomes.iter().all(|outcome| outcome.success) {
            Ok(outcomes)
        } else {
            let details =
                serde_json::to_value(&outcomes).unwrap_or_else(|_| serde_json::Value::Null);
            Err(ApplicationError::PartialFailure {
                message: "one or more sessions could not be deleted".to_string(),
                details,
            })
        }
    }

    pub fn session_index_status(&self) -> ApplicationResult<Option<IndexStats>> {
        if !crate::session_index::index_exists() {
            return Ok(None);
        }
        SessionIndex::open()
            .and_then(|index| index.stats())
            .map(Some)
            .map_err(ApplicationError::OperationFailed)
    }

    pub fn search_session_index(
        &self,
        query: &str,
        limit: usize,
    ) -> ApplicationResult<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ApplicationError::InvalidInput(
                "session search query cannot be empty".to_string(),
            ));
        }
        if !(1..=10_000).contains(&limit) {
            return Err(ApplicationError::InvalidInput(
                "session search limit must be between 1 and 10000".to_string(),
            ));
        }
        SessionIndex::open()
            .and_then(|index| index.search(query, limit))
            .map_err(ApplicationError::OperationFailed)
    }

    pub fn sync_session_index(&self) -> ApplicationResult<SyncOutcome> {
        let sessions = self.list_sessions(&[], None)?;
        let cancel = AtomicBool::new(false);
        let index = SessionIndex::open().map_err(ApplicationError::OperationFailed)?;
        let outcome = index
            .sync(&sessions, |_done, _total| {}, &cancel)
            .map_err(ApplicationError::OperationFailed)?;
        if crate::settings::get_settings().session_index_auto_reclaim
            && !outcome.cancelled
            && index
                .needs_maintenance()
                .map_err(ApplicationError::OperationFailed)?
        {
            index
                .maintain(Duration::from_secs(20))
                .map_err(ApplicationError::OperationFailed)?;
        }
        Ok(outcome)
    }

    pub fn maintain_session_index(
        &self,
        budget: Duration,
    ) -> ApplicationResult<MaintenanceOutcome> {
        SessionIndex::open()
            .and_then(|index| index.maintain(budget))
            .map_err(ApplicationError::OperationFailed)
    }

    pub fn delete_session_index(&self) -> ApplicationResult<serde_json::Value> {
        SessionIndex::delete_files(&crate::session_index::default_index_path());
        Ok(serde_json::json!({ "deleted": true }))
    }

    pub async fn tool_versions(
        &self,
        tools: Option<Vec<String>>,
    ) -> ApplicationResult<Vec<ToolVersion>> {
        validate_tools(tools.as_deref())?;
        crate::session_manager::get_tool_versions(tools, None)
            .await
            .map_err(map_tool_error)
    }

    pub fn probe_tools(
        &self,
        tools: Vec<String>,
    ) -> ApplicationResult<Vec<ToolInstallationReport>> {
        validate_tools(Some(&tools))?;
        crate::session_manager::probe_tool_installations(tools).map_err(map_tool_error)
    }

    pub async fn run_tool_lifecycle(
        &self,
        tools: Vec<String>,
        action: &str,
    ) -> ApplicationResult<()> {
        validate_tools(Some(&tools))?;
        let action = action.to_string();
        tokio::task::spawn_blocking(move || {
            crate::session_manager::run_tool_lifecycle_action(tools, action, None)
        })
        .await
        .map_err(|error| ApplicationError::OperationFailed(error.to_string()))?
        .map_err(map_tool_error)
    }
}

fn validate_tools(tools: Option<&[String]>) -> ApplicationResult<()> {
    const SUPPORTED: &[&str] = &["claude", "codex", "opencode", "openclaw", "hermes"];
    if let Some(tools) = tools {
        if tools.is_empty() {
            return Err(ApplicationError::InvalidInput(
                "at least one tool is required".to_string(),
            ));
        }
        if let Some(tool) = tools
            .iter()
            .find(|tool| !SUPPORTED.contains(&tool.as_str()))
        {
            return Err(ApplicationError::InvalidInput(format!(
                "unsupported tool: {tool}"
            )));
        }
    }
    Ok(())
}

fn map_tool_error(message: String) -> ApplicationError {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("not installed")
        || normalized.contains("not found")
        || normalized.contains("missing")
    {
        ApplicationError::DependencyMissing(message)
    } else if normalized.contains("unsupported") || normalized.contains("only supported") {
        ApplicationError::PlatformUnsupported(message)
    } else {
        ApplicationError::OperationFailed(message)
    }
}
