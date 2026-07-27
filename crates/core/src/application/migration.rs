use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::db::import_ccswitch::{detect_source, DetectedSource, ImportReport};

impl Application {
    pub fn detect_ccswitch(&self) -> ApplicationResult<Option<DetectedSource>> {
        Ok(detect_source())
    }

    pub fn plan_ccswitch_import(&self) -> ApplicationResult<serde_json::Value> {
        let source = detect_source().ok_or_else(|| ApplicationError::NotFound {
            kind: "ccswitch-source",
            id: crate::paths::get_legacy_ccswitch_dir()
                .to_string_lossy()
                .into_owned(),
        })?;
        let apps = self.list_apps()?;
        let existing_providers = apps
            .iter()
            .map(|app| {
                crate::AppId::parse(&app.id)
                    .map_err(ApplicationError::Core)
                    .and_then(|id| self.list_providers(&id).map(|providers| providers.len()))
            })
            .collect::<ApplicationResult<Vec<_>>>()?
            .into_iter()
            .sum::<usize>();
        let existing_mcp = self.list_mcp_servers(false)?.len();
        let existing_skills = self.list_installed_skills()?.len();
        Ok(serde_json::json!({
            "source": source,
            "target": {
                "dataDir": crate::paths::get_app_config_dir(),
                "database": crate::paths::get_database_path(),
                "existingProviders": existing_providers,
                "existingMcpServers": existing_mcp,
                "existingSkills": existing_skills
            },
            "strategy": "insert-or-replace-by-stable-id",
            "createsSafetyBackup": existing_providers > 0,
            "writesSource": false,
            "wouldImport": {
                "providers": source.providers,
                "mcpServers": source.mcp_servers,
                "skillRepos": source.skill_repos
            }
        }))
    }

    pub fn import_ccswitch(&self) -> ApplicationResult<ImportReport> {
        let source = detect_source().ok_or_else(|| ApplicationError::NotFound {
            kind: "ccswitch-source",
            id: crate::paths::get_legacy_ccswitch_dir()
                .to_string_lossy()
                .into_owned(),
        })?;
        let report = self.state.db.import_from_ccswitch_source(&source)?;
        crate::settings::reload_settings()?;
        Ok(report)
    }
}
