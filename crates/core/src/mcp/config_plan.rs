//! Include this application's MCP changes in the same prepared file image.
use crate::services::config_commit::{PreparedConfigWrite, PreparedFileWrite, read_optional};
use crate::{AppError, AppType, Database};
use serde_json::{Value, json};

pub(crate) fn prepare_config_sync(
    db: &Database,
    app: AppType,
    plan: &mut PreparedConfigWrite,
) -> Result<(), AppError> {
    let servers = db.get_all_mcp_servers()?;
    if servers.is_empty() {
        return Ok(());
    }
    let path = match app {
        AppType::Claude => crate::paths::get_claude_mcp_path(),
        AppType::Codex => crate::apps::codex::get_codex_config_path(),
        AppType::GrokBuild => crate::apps::grokbuild::get_grok_config_path(),
        AppType::OpenCode => crate::apps::opencode::get_opencode_config_path(),
        AppType::Hermes => crate::apps::hermes::get_hermes_config_path(),
        _ => return Ok(()),
    };
    let index = match plan.files.iter().position(|f| f.path == path) {
        Some(index) => index,
        None => {
            let before = read_optional(&path)?;
            if before.is_none() && !servers.values().any(|s| s.apps.is_enabled_for(&app)) {
                return Ok(());
            }
            plan.files.push(PreparedFileWrite {
                path: path.clone(),
                before: before.clone(),
                after: before,
                file_mode: None,
                dir_mode: None,
            });
            plan.files.len() - 1
        }
    };
    let file = &mut plan.files[index];
    let bytes = file.after.as_deref().unwrap_or_default();
    let source = std::str::from_utf8(bytes).map_err(|e| AppError::Config(e.to_string()))?;
    let after = match app {
        AppType::Codex | AppType::GrokBuild => {
            let mut doc = source
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| AppError::Config(e.to_string()))?;
            for server in servers.values() {
                if server.apps.is_enabled_for(&app) {
                    if !doc.contains_key("mcp_servers") {
                        doc["mcp_servers"] = toml_edit::table();
                    }
                    let table = if app == AppType::Codex {
                        super::codex::json_server_to_toml_table(&server.server)?
                    } else {
                        super::grokbuild::json_server_to_grokbuild_toml_table(&server.server)?
                    };
                    doc["mcp_servers"][&server.id] = toml_edit::Item::Table(table);
                } else if let Some(table) = doc
                    .get_mut("mcp_servers")
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    table.remove(&server.id);
                }
            }
            if app == AppType::Codex
                && let Some(mcp) = doc
                    .get_mut("mcp")
                    .and_then(toml_edit::Item::as_table_like_mut)
            {
                mcp.remove("servers");
            }
            doc.to_string().into_bytes()
        }
        AppType::Claude | AppType::OpenCode => {
            let mut full: Value = if source.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(source).map_err(|e| AppError::json(&path, e))?
            };
            let root = full.as_object_mut().ok_or_else(|| {
                AppError::Config("MCP configuration root must be an object".into())
            })?;
            let key = if app == AppType::Claude {
                "mcpServers"
            } else {
                "mcp"
            };
            let table = root
                .entry(key)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| AppError::Config("MCP section must be an object".into()))?;
            for server in servers.values() {
                if server.apps.is_enabled_for(&app) {
                    table.insert(
                        server.id.clone(),
                        if app == AppType::Claude {
                            server.server.clone()
                        } else {
                            super::opencode::convert_to_opencode_format(&server.server)?
                        },
                    );
                } else {
                    table.remove(&server.id);
                }
            }
            serde_json::to_vec_pretty(&full).map_err(|source| AppError::JsonSerialize { source })?
        }
        AppType::Hermes => {
            let full: serde_norway::Value =
                serde_norway::from_str(source).map_err(|e| AppError::Config(e.to_string()))?;
            let mut table = full
                .get("mcp_servers")
                .and_then(serde_norway::Value::as_mapping)
                .cloned()
                .unwrap_or_default();
            for server in servers.values() {
                let key = serde_norway::Value::String(server.id.clone());
                if server.apps.is_enabled_for(&app) {
                    let mut value = super::hermes::convert_to_hermes_format(&server.server)?;
                    if let Some(existing) = table.get(&key) {
                        value = super::hermes::merge_hermes_spec(
                            &crate::apps::hermes::yaml_to_json(existing)?,
                            &value,
                        );
                    }
                    table.insert(key, crate::apps::hermes::json_to_yaml(&value)?);
                } else {
                    table.remove(&key);
                }
            }
            crate::apps::hermes::replace_yaml_section(
                source,
                "mcp_servers",
                &serde_norway::Value::Mapping(table),
            )?
            .into_bytes()
        }
        _ => unreachable!(),
    };
    file.after = Some(after);
    Ok(())
}
