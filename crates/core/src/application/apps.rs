use crate::application::{
    AppModeDto, AppSummary, Application, ApplicationError, ApplicationResult, ConfigFieldDto,
    ConfigFieldKindDto, ConfigSchemaDto, ConfigSectionDto,
};
use crate::plugin::AppMode;
use crate::provider_config::{FieldKind, GridCellKind};
use crate::{AppId, AppType};

impl Application {
    pub fn list_apps(&self) -> ApplicationResult<Vec<AppSummary>> {
        Ok(crate::plugin::all_plugins()
            .into_iter()
            .map(|plugin| {
                let (config_dir, config_error) = match plugin.config_dir() {
                    Ok(path) => (Some(path.to_string_lossy().into_owned()), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                AppSummary {
                    id: plugin.id().to_string(),
                    display_name: plugin.display_name().to_string(),
                    enabled: crate::plugin::is_app_enabled(plugin.as_ref()),
                    mode: match plugin.mode() {
                        AppMode::Switch => AppModeDto::Switch,
                        AppMode::Additive => AppModeDto::Additive,
                    },
                    config_dir,
                    config_error,
                    supports_provider: plugin.provider_config().is_some(),
                    supports_mcp: plugin.supports_mcp(),
                    supports_skills: plugin.supports_skills(),
                    user_manifest: plugin.is_user_manifest(),
                }
            })
            .collect())
    }

    pub fn get_app(&self, id: &AppId) -> ApplicationResult<AppSummary> {
        self.list_apps()?
            .into_iter()
            .find(|app| app.id == id.as_str())
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "app",
                id: id.to_string(),
            })
    }

    pub async fn set_app_enabled(
        &self,
        id: &AppId,
        enabled: bool,
    ) -> ApplicationResult<AppSummary> {
        crate::services::apps::set_app_enabled(&self.state, id, enabled).await?;
        self.get_app(id)
    }

    pub fn app_schema(&self, id: &AppId) -> ApplicationResult<ConfigSchemaDto> {
        let plugin = crate::plugin::get_plugin(id).ok_or_else(|| ApplicationError::NotFound {
            kind: "app",
            id: id.to_string(),
        })?;
        let codec =
            plugin
                .provider_config()
                .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                    app: id.to_string(),
                    capability: "provider.schema",
                })?;
        let sections = codec
            .schema()
            .into_iter()
            .map(|section| ConfigSectionDto {
                title: section.title,
                advanced: section.advanced,
                fields: section
                    .fields
                    .into_iter()
                    .map(|field| ConfigFieldDto {
                        id: field.id,
                        label: field.label,
                        help: field.help,
                        required: field.required,
                        visible_when: field.visible_when,
                        kind: match field.kind {
                            FieldKind::Text { placeholder } => {
                                ConfigFieldKindDto::Text { placeholder }
                            }
                            FieldKind::Secret { placeholder } => {
                                ConfigFieldKindDto::Secret { placeholder }
                            }
                            FieldKind::Select { options } => ConfigFieldKindDto::Select {
                                options: options
                                    .into_iter()
                                    .map(|option| {
                                        serde_json::json!({
                                            "value": option.value,
                                            "label": option.label,
                                            "hint": option.hint,
                                        })
                                    })
                                    .collect(),
                            },
                            FieldKind::Toggle => ConfigFieldKindDto::Toggle,
                            FieldKind::KeyValue {
                                key_placeholder,
                                value_placeholder,
                            } => ConfigFieldKindDto::KeyValue {
                                key_placeholder,
                                value_placeholder,
                            },
                            FieldKind::ModelGrid { columns } => ConfigFieldKindDto::ModelGrid {
                                columns: columns
                                    .into_iter()
                                    .map(|column| {
                                        let kind = match column.kind {
                                            GridCellKind::Text { placeholder } => {
                                                serde_json::json!({
                                                    "type": "text",
                                                    "placeholder": placeholder
                                                })
                                            }
                                            GridCellKind::Toggle => {
                                                serde_json::json!({ "type": "toggle" })
                                            }
                                        };
                                        serde_json::json!({
                                            "key": column.key,
                                            "label": column.label,
                                            "kind": kind,
                                        })
                                    })
                                    .collect(),
                            },
                        },
                    })
                    .collect(),
            })
            .collect();
        Ok(ConfigSchemaDto {
            app: id.to_string(),
            sections,
        })
    }

    pub fn set_app_config_dir(
        &self,
        id: &AppId,
        path: Option<String>,
    ) -> ApplicationResult<AppSummary> {
        self.get_app(id)?;
        let normalized = path
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty());
        let builtin = AppType::from_app_id(id);
        if matches!(
            builtin,
            Some(AppType::ClaudeDesktop | AppType::CherryStudio)
        ) {
            return Err(ApplicationError::CapabilityUnsupported {
                app: id.to_string(),
                capability: "app.config-dir-override",
            });
        }
        crate::settings::mutate_settings(|settings| match builtin {
            Some(AppType::Claude) => settings.claude_config_dir = normalized.clone(),
            Some(AppType::CherryStudio) => {}
            Some(AppType::Codex) => settings.codex_config_dir = normalized.clone(),
            Some(AppType::GrokBuild) => settings.grokbuild_config_dir = normalized.clone(),
            Some(AppType::KimiCode) => settings.kimi_code_config_dir = normalized.clone(),
            Some(AppType::OpenCode) => settings.opencode_config_dir = normalized.clone(),
            Some(AppType::OpenClaw) => settings.openclaw_config_dir = normalized.clone(),
            Some(AppType::Hermes) => settings.hermes_config_dir = normalized.clone(),
            Some(AppType::ClaudeDesktop) => {}
            None => {
                let dirs = settings
                    .app_config_dirs
                    .get_or_insert_with(Default::default);
                if let Some(path) = &normalized {
                    dirs.insert(id.to_string(), path.clone());
                } else {
                    dirs.remove(id.as_str());
                }
            }
        })?;
        self.get_app(id)
    }
}
