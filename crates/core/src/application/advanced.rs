use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::application::{Application, ApplicationError, ApplicationResult, redact_json};
use crate::services::env::checker::EnvConflict;

impl Application {
    pub fn scan_environment_conflicts(&self, show_secrets: bool) -> ApplicationResult<Vec<Value>> {
        let mut conflicts = Vec::new();
        for app in ["claude", "codex"] {
            for conflict in crate::services::env::checker::check_env_conflicts(app)
                .map_err(ApplicationError::OperationFailed)?
            {
                let id = environment_conflict_id(&conflict);
                conflicts.push(json!({
                    "id": id,
                    "app": app,
                    "variable": conflict.var_name,
                    "value": if show_secrets { Value::String(conflict.var_value) } else { Value::String("******".to_string()) },
                    "sourceType": conflict.source_type,
                    "sourcePath": conflict.source_path
                }));
            }
        }
        conflicts.sort_by(|left, right| {
            left["id"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["id"].as_str().unwrap_or_default())
        });
        Ok(conflicts)
    }

    pub fn clean_environment_conflict(&self, id: &str) -> ApplicationResult<Value> {
        let conflict = find_environment_conflict(id)?;
        let backup = crate::services::env::manager::delete_env_vars(vec![conflict])
            .map_err(ApplicationError::OperationFailed)?;
        serde_json::to_value(backup)
            .map_err(|source| crate::AppError::JsonSerialize { source }.into())
    }

    pub fn restore_environment_backup(&self, id: &str) -> ApplicationResult<Value> {
        let path = resolve_environment_backup(id)?;
        crate::services::env::manager::restore_env_backup(path.to_string_lossy().into_owned())
            .map_err(ApplicationError::OperationFailed)?;
        Ok(json!({ "restored": true, "backup": path }))
    }

    pub fn claude_plugin_status(&self) -> ApplicationResult<Value> {
        let (exists, path) = crate::apps::claude_plugin::claude_config_status()?;
        Ok(json!({
            "exists": exists,
            "path": path,
            "applied": crate::apps::claude_plugin::is_claude_config_applied()?
        }))
    }

    pub fn claude_plugin_config(&self, show_secrets: bool) -> ApplicationResult<Value> {
        let content = crate::apps::claude_plugin::read_claude_config()?;
        let parsed = content
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|source| {
                crate::AppError::json(
                    crate::apps::claude_plugin::claude_config_path()
                        .unwrap_or_else(|_| PathBuf::from("claude-config")),
                    source,
                )
            })?;
        Ok(json!({
            "path": crate::apps::claude_plugin::claude_config_path()?,
            "config": if show_secrets {
                parsed
            } else {
                parsed.as_ref().map(redact_json)
            }
        }))
    }

    pub fn apply_claude_plugin(&self, official: bool) -> ApplicationResult<Value> {
        let changed = if official {
            crate::apps::claude_plugin::clear_claude_config()?
        } else {
            crate::apps::claude_plugin::write_claude_config()?
        };
        Ok(json!({ "changed": changed, "official": official }))
    }

    pub fn restore_claude_plugin(&self) -> ApplicationResult<Value> {
        Ok(json!({
            "changed": crate::apps::claude_plugin::clear_claude_config()?
        }))
    }

    pub fn claude_mcp_status(&self) -> ApplicationResult<Value> {
        serde_json::to_value(crate::mcp::get_mcp_status()?)
            .map_err(|source| crate::AppError::JsonSerialize { source }.into())
    }

    pub fn claude_mcp_config(&self, show_secrets: bool) -> ApplicationResult<Value> {
        let raw = crate::mcp::read_mcp_json()?;
        let value = raw
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|source| crate::AppError::json(crate::paths::get_claude_mcp_path(), source))?;
        Ok(match (value, show_secrets) {
            (Some(value), true) => value,
            (Some(value), false) => redact_json(&value),
            (None, _) => Value::Null,
        })
    }

    pub fn upsert_claude_mcp_server(&self, id: &str, spec: Value) -> ApplicationResult<Value> {
        Ok(json!({
            "id": id,
            "changed": crate::mcp::upsert_mcp_server(id, spec)?
        }))
    }

    pub fn delete_claude_mcp_server(&self, id: &str) -> ApplicationResult<Value> {
        Ok(json!({
            "id": id,
            "changed": crate::mcp::delete_mcp_server(id)?
        }))
    }

    pub fn validate_claude_mcp_paths(&self) -> ApplicationResult<Value> {
        let servers = crate::mcp::read_mcp_servers_map()?;
        let mut commands = Vec::new();
        for (id, spec) in servers {
            if spec.get("type").and_then(Value::as_str).unwrap_or("stdio") != "stdio" {
                continue;
            }
            let Some(command) = spec.get("command").and_then(Value::as_str) else {
                continue;
            };
            commands.push(json!({
                "id": id,
                "command": command,
                "valid": crate::mcp::validate_command_in_path(command)?
            }));
        }
        let valid = commands
            .iter()
            .all(|command| command["valid"].as_bool() == Some(true));
        Ok(json!({ "valid": valid, "commands": commands }))
    }

    pub fn claude_onboarding_status(&self) -> ApplicationResult<Value> {
        let config = self.claude_mcp_config(true)?;
        Ok(json!({
            "completed": config
                .get("hasCompletedOnboarding")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }))
    }

    pub fn set_claude_onboarding(&self, completed: bool) -> ApplicationResult<Value> {
        let changed = if completed {
            crate::mcp::set_has_completed_onboarding()?
        } else {
            crate::mcp::clear_has_completed_onboarding()?
        };
        Ok(json!({ "completed": completed, "changed": changed }))
    }

    pub fn codex_history_status(&self) -> ApplicationResult<Value> {
        let settings = crate::settings::get_settings();
        Ok(json!({
            "unified": settings.unify_codex_session_history,
            "migrateExistingRequested": settings.unify_codex_migrate_existing,
            "backupExists": crate::services::codex_history_migration::has_codex_official_history_unify_backup(),
            "configDir": crate::apps::codex::get_codex_config_dir()
        }))
    }

    pub fn migrate_codex_history(&self) -> ApplicationResult<Value> {
        let provider_bucket =
            crate::services::codex_history_migration::maybe_migrate_codex_third_party_history_provider_bucket(
                self.state.db.as_ref(),
            )?;
        let provider_templates =
            crate::services::codex_history_migration::maybe_migrate_codex_provider_template_bucket(
                self.state.db.as_ref(),
            )?;
        let official =
            crate::services::codex_history_migration::maybe_migrate_codex_official_history_to_unified_bucket(
            )?;
        Ok(json!({
            "providerBucket": {
                "sourceProviderIds": provider_bucket.source_provider_ids,
                "migratedJsonlFiles": provider_bucket.migrated_jsonl_files,
                "migratedStateRows": provider_bucket.migrated_state_rows,
                "skippedReason": provider_bucket.skipped_reason
            },
            "providerTemplates": {
                "migratedProviderIds": provider_templates.migrated_provider_ids,
                "skippedReason": provider_templates.skipped_reason
            },
            "officialHistory": {
                "sourceProviderIds": official.source_provider_ids,
                "migratedJsonlFiles": official.migrated_jsonl_files,
                "migratedStateRows": official.migrated_state_rows,
                "skippedReason": official.skipped_reason
            }
        }))
    }

    pub fn restore_codex_history(&self) -> ApplicationResult<Value> {
        let outcome =
            crate::services::codex_history_migration::restore_codex_official_history_from_backups(
            )?;
        Ok(json!({
            "restoredJsonlFiles": outcome.restored_jsonl_files,
            "restoredStateRows": outcome.restored_state_rows,
            "skippedReason": outcome.skipped_reason
        }))
    }

    pub fn omo_status(&self, slim: bool) -> ApplicationResult<Value> {
        let variant = omo_variant(slim);
        let current = self
            .state
            .db
            .get_current_omo_provider("opencode", variant.category)?
            .map(|provider| provider.id);
        let local = match crate::services::OmoService::read_local_file(variant) {
            Ok(data) => Some(
                serde_json::to_value(data)
                    .map_err(|source| crate::AppError::JsonSerialize { source })?,
            ),
            Err(crate::AppError::OmoConfigNotFound) => None,
            Err(error) => return Err(error.into()),
        };
        Ok(json!({
            "variant": variant.category,
            "enabled": current.is_some(),
            "currentProviderId": current,
            "localFile": local
        }))
    }

    pub fn omo_current(&self, slim: bool) -> ApplicationResult<Value> {
        let variant = omo_variant(slim);
        let provider = self
            .state
            .db
            .get_current_omo_provider("opencode", variant.category)?;
        Ok(json!({
            "variant": variant.category,
            "provider": provider
        }))
    }

    pub fn omo_local_file(&self, slim: bool) -> ApplicationResult<Value> {
        serde_json::to_value(crate::services::OmoService::read_local_file(omo_variant(
            slim,
        ))?)
        .map_err(|source| crate::AppError::JsonSerialize { source }.into())
    }

    pub fn disable_omo(&self, slim: bool) -> ApplicationResult<Value> {
        let variant = omo_variant(slim);
        let providers = self.state.db.get_all_providers("opencode")?;
        for (id, provider) in providers {
            if provider.category.as_deref() == Some(variant.category) {
                self.state
                    .db
                    .clear_omo_provider_current("opencode", &id, variant.category)?;
            }
        }
        crate::services::OmoService::delete_config_file(variant)?;
        Ok(json!({ "disabled": true, "variant": variant.category }))
    }

    pub fn openclaw_health(&self) -> ApplicationResult<Value> {
        Ok(json!({
            "healthy": crate::apps::openclaw::scan_openclaw_config_health()?.is_empty(),
            "warnings": crate::apps::openclaw::scan_openclaw_config_health()?,
            "path": crate::apps::openclaw::get_openclaw_config_path()
        }))
    }

    pub fn openclaw_default_model(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::openclaw::get_default_model()?)
    }

    pub fn set_openclaw_default_model(&self, value: Value) -> ApplicationResult<Value> {
        let model = serde_json::from_value(value)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        to_value(crate::apps::openclaw::set_default_model(&model)?)
    }

    pub fn openclaw_models(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::openclaw::get_model_catalog()?)
    }

    pub fn openclaw_agent_defaults(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::openclaw::get_agents_defaults()?)
    }

    pub fn set_openclaw_agent_defaults(&self, value: Value) -> ApplicationResult<Value> {
        let defaults = serde_json::from_value(value)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        to_value(crate::apps::openclaw::set_agents_defaults(&defaults)?)
    }

    pub fn openclaw_env(&self, show_secrets: bool) -> ApplicationResult<Value> {
        let value = to_value(crate::apps::openclaw::get_env_config()?)?;
        Ok(if show_secrets {
            value
        } else {
            redact_json(&value)
        })
    }

    pub fn set_openclaw_env(&self, value: Value) -> ApplicationResult<Value> {
        let env = serde_json::from_value(value)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        to_value(crate::apps::openclaw::set_env_config(&env)?)
    }

    pub fn openclaw_tools(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::openclaw::get_tools_config()?)
    }

    pub fn set_openclaw_tools(&self, value: Value) -> ApplicationResult<Value> {
        let tools = serde_json::from_value(value)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        to_value(crate::apps::openclaw::set_tools_config(&tools)?)
    }

    pub fn hermes_models(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::hermes::get_model_config()?)
    }

    pub fn set_hermes_models(&self, value: Value) -> ApplicationResult<Value> {
        let model = serde_json::from_value(value)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        to_value(crate::apps::hermes::set_model_config(&model)?)
    }

    pub fn hermes_memory_status(&self) -> ApplicationResult<Value> {
        let limits = crate::apps::hermes::read_memory_limits()?;
        let memory = crate::apps::hermes::read_memory(crate::apps::hermes::MemoryKind::Memory)?;
        let user = crate::apps::hermes::read_memory(crate::apps::hermes::MemoryKind::User)?;
        Ok(json!({
            "limits": limits,
            "memoryCharacters": memory.chars().count(),
            "userCharacters": user.chars().count(),
            "directory": crate::apps::hermes::get_hermes_dir().join("memories")
        }))
    }

    pub fn hermes_memory_limits(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::hermes::read_memory_limits()?)
    }

    pub fn read_hermes_memory(&self, kind: &str) -> ApplicationResult<Value> {
        let kind = memory_kind(kind)?;
        Ok(json!({
            "kind": kind_name(kind),
            "content": crate::apps::hermes::read_memory(kind)?
        }))
    }

    pub fn write_hermes_memory(&self, kind: &str, content: &str) -> ApplicationResult<Value> {
        let kind = memory_kind(kind)?;
        crate::apps::hermes::write_memory(kind, content)?;
        Ok(json!({
            "kind": kind_name(kind),
            "characters": content.chars().count(),
            "written": true
        }))
    }

    pub fn set_hermes_memory_enabled(&self, kind: &str, enabled: bool) -> ApplicationResult<Value> {
        let kind = memory_kind(kind)?;
        let outcome = crate::apps::hermes::set_memory_enabled(kind, enabled)?;
        Ok(json!({
            "kind": kind_name(kind),
            "enabled": enabled,
            "outcome": outcome
        }))
    }
}

fn environment_conflict_id(conflict: &EnvConflict) -> String {
    let input = format!(
        "{}\0{}\0{}\0{}",
        conflict.var_name, conflict.var_value, conflict.source_type, conflict.source_path
    );
    let digest = Sha256::digest(input.as_bytes());
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn find_environment_conflict(id: &str) -> ApplicationResult<EnvConflict> {
    for app in ["claude", "codex"] {
        for conflict in crate::services::env::checker::check_env_conflicts(app)
            .map_err(ApplicationError::OperationFailed)?
        {
            if environment_conflict_id(&conflict) == id {
                return Ok(conflict);
            }
        }
    }
    Err(ApplicationError::NotFound {
        kind: "environment-conflict",
        id: id.to_string(),
    })
}

fn resolve_environment_backup(id: &str) -> ApplicationResult<PathBuf> {
    let name = Path::new(id)
        .file_name()
        .filter(|name| *name == std::ffi::OsStr::new(id))
        .ok_or_else(|| {
            ApplicationError::InvalidInput("backup id must be a filename".to_string())
        })?;
    let path = crate::paths::get_app_config_dir()
        .join("backups")
        .join(name);
    if !path.is_file() {
        return Err(ApplicationError::NotFound {
            kind: "environment-backup",
            id: id.to_string(),
        });
    }
    Ok(path)
}

fn omo_variant(slim: bool) -> &'static crate::services::omo::OmoVariant {
    if slim {
        &crate::services::omo::SLIM
    } else {
        &crate::services::omo::STANDARD
    }
}

fn memory_kind(kind: &str) -> ApplicationResult<crate::apps::hermes::MemoryKind> {
    match kind {
        "memory" => Ok(crate::apps::hermes::MemoryKind::Memory),
        "user" => Ok(crate::apps::hermes::MemoryKind::User),
        _ => Err(ApplicationError::InvalidInput(
            "memory kind must be memory or user".to_string(),
        )),
    }
}

fn kind_name(kind: crate::apps::hermes::MemoryKind) -> &'static str {
    match kind {
        crate::apps::hermes::MemoryKind::Memory => "memory",
        crate::apps::hermes::MemoryKind::User => "user",
    }
}

fn to_value(value: impl serde::Serialize) -> ApplicationResult<Value> {
    serde_json::to_value(value).map_err(|source| crate::AppError::JsonSerialize { source }.into())
}
