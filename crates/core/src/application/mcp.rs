use crate::application::{Application, ApplicationError, ApplicationResult, redact_json};
use crate::db::McpServer;
use crate::mcp::validate_server_spec;
use crate::services::McpService;
use crate::{AppId, AppType};

impl Application {
    /// List every managed MCP server. Secret-shaped values are redacted unless
    /// the caller explicitly opts into revealing them.
    pub fn list_mcp_servers(
        &self,
        show_secrets: bool,
    ) -> ApplicationResult<Vec<serde_json::Value>> {
        McpService::get_all_servers(&self.state)?
            .into_values()
            .map(|server| self.mcp_server_value(server, show_secrets))
            .collect()
    }

    pub fn get_mcp_server(
        &self,
        id: &str,
        show_secrets: bool,
    ) -> ApplicationResult<serde_json::Value> {
        let server = McpService::get_all_servers(&self.state)?
            .shift_remove(id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "mcp-server",
                id: id.to_string(),
            })?;
        self.mcp_server_value(server, show_secrets)
    }

    pub fn validate_mcp_server(&self, server: &McpServer) -> ApplicationResult<()> {
        if server.id.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "MCP server id cannot be empty".to_string(),
            ));
        }
        if server.name.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "MCP server name cannot be empty".to_string(),
            ));
        }
        validate_server_spec(&server.server)?;
        Ok(())
    }

    pub fn upsert_mcp_server(&self, server: McpServer) -> ApplicationResult<serde_json::Value> {
        self.validate_mcp_server(&server)?;
        let id = server.id.clone();
        McpService::upsert_server(&self.state, server)?;
        self.get_mcp_server(&id, false)
    }

    pub fn delete_mcp_server(&self, id: &str) -> ApplicationResult<()> {
        if !McpService::delete_server(&self.state, id)? {
            return Err(ApplicationError::NotFound {
                kind: "mcp-server",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    pub fn set_mcp_app_enabled(
        &self,
        id: &str,
        app: &AppId,
        enabled: bool,
    ) -> ApplicationResult<serde_json::Value> {
        // Resolve the record first because the legacy service intentionally
        // treats an unknown id as a no-op.
        self.get_mcp_server(id, false)?;
        let app_type = self.require_builtin_mcp_app(app)?;
        McpService::toggle_app(&self.state, id, app_type, enabled)?;
        self.get_mcp_server(id, false)
    }

    /// Re-apply one MCP server to the requested apps. With an empty app list,
    /// all apps currently enabled on that server are synchronized.
    pub fn sync_mcp_server(
        &self,
        id: &str,
        apps: &[AppId],
    ) -> ApplicationResult<serde_json::Value> {
        let servers = McpService::get_all_servers(&self.state)?;
        let server = servers.get(id).ok_or_else(|| ApplicationError::NotFound {
            kind: "mcp-server",
            id: id.to_string(),
        })?;

        let targets = if apps.is_empty() {
            server.apps.enabled_apps()
        } else {
            apps.iter()
                .map(|app| self.require_builtin_mcp_app(app))
                .collect::<ApplicationResult<Vec<_>>>()?
        };

        for app in &targets {
            McpService::toggle_app(&self.state, id, *app, true)?;
        }

        Ok(serde_json::json!({
            "id": id,
            "syncedApps": targets.iter().map(AppType::as_str).collect::<Vec<_>>()
        }))
    }

    pub fn sync_all_mcp_servers(&self) -> ApplicationResult<usize> {
        let count = McpService::get_all_servers(&self.state)?.len();
        McpService::sync_all_enabled(&self.state)?;
        Ok(count)
    }

    pub fn import_mcp_from_app(&self, app: &AppId) -> ApplicationResult<usize> {
        let app_type = self.require_builtin_mcp_app(app)?;
        match app_type {
            AppType::Claude => Ok(McpService::import_from_claude(&self.state)?),
            AppType::Codex => Ok(McpService::import_from_codex(&self.state)?),
            AppType::GrokBuild => Ok(McpService::import_from_grokbuild(&self.state)?),
            AppType::OpenCode => Ok(McpService::import_from_opencode(&self.state)?),
            AppType::Hermes => Ok(McpService::import_from_hermes(&self.state)?),
            AppType::ClaudeDesktop
            | AppType::CherryStudio
            | AppType::OpenClaw
            | AppType::KimiCode => Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "mcp.import",
            }),
        }
    }

    fn require_builtin_mcp_app(&self, app: &AppId) -> ApplicationResult<AppType> {
        let summary = self.get_app(app)?;
        if !summary.supports_mcp {
            return Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "mcp",
            });
        }
        AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
            app: app.to_string(),
            capability: "mcp.live-sync",
        })
    }

    fn mcp_server_value(
        &self,
        server: McpServer,
        show_secrets: bool,
    ) -> ApplicationResult<serde_json::Value> {
        let value = serde_json::to_value(server)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        Ok(if show_secrets {
            value
        } else {
            redact_json(&value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{McpApps, McpServer};
    use crate::{AppState, Database};
    use std::sync::Arc;

    #[test]
    fn validates_and_redacts_mcp_servers() {
        let state = Arc::new(AppState::new(Arc::new(Database::memory().unwrap())));
        let app = Application::from_state(state);
        app.upsert_mcp_server(McpServer {
            id: "test".into(),
            name: "Test".into(),
            server: serde_json::json!({
                "command": "example",
                "env": {"API_KEY": "secret"}
            }),
            apps: McpApps::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: vec![],
        })
        .unwrap();

        let redacted = app.get_mcp_server("test", false).unwrap();
        assert_eq!(redacted["server"]["env"]["API_KEY"], "******");
        let clear = app.get_mcp_server("test", true).unwrap();
        assert_eq!(clear["server"]["env"]["API_KEY"], "secret");
    }
}
