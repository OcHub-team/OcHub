//! Pure preparation of ordinary built-in configuration files.
use super::config_commit::{PreparedConfigWrite, PreparedFileWrite, read_optional};
use crate::{AppError, AppType, Provider};
use serde_json::{Value, json};

pub(crate) fn builtin_plan(
    app: AppType,
    provider: &Provider,
) -> Result<Option<PreparedConfigWrite>, AppError> {
    // Bind all transformations to one initial byte image. Several legacy pure
    // builders still reread their input; a concurrent edit must fail CAS, never
    // become a newer before-image for output generated from older bytes.
    let targets = match app {
        AppType::Claude => vec![crate::paths::get_claude_settings_path()],
        AppType::Codex => vec![
            crate::apps::codex::get_codex_auth_path(),
            crate::apps::codex::get_codex_config_path(),
            crate::apps::codex::get_codex_model_catalog_path(),
        ],
        AppType::GrokBuild => vec![crate::apps::grokbuild::get_grok_config_path()],
        AppType::KimiCode => vec![crate::apps::kimi_code::get_kimi_code_config_path()],
        AppType::ClaudeDesktop => crate::apps::claude_desktop::prepared_target_paths()?,
        AppType::OpenCode if matches!(provider.category.as_deref(), Some("omo" | "omo-slim")) => {
            super::omo::OmoService::prepared_target_paths()
        }
        AppType::OpenCode => vec![crate::apps::opencode::get_opencode_config_path()],
        AppType::OpenClaw => vec![crate::apps::openclaw::get_openclaw_config_path()],
        AppType::Hermes => vec![crate::apps::hermes::get_hermes_config_path()],
        AppType::CherryStudio => Vec::new(),
    };
    let before = targets
        .into_iter()
        .map(|path| read_optional(&path).map(|bytes| (path, bytes)))
        .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
    let bind =
        |mut plan: Option<PreparedConfigWrite>| -> Result<Option<PreparedConfigWrite>, AppError> {
            if let Some(plan) = plan.as_mut() {
                for file in &mut plan.files {
                    file.before = before.get(&file.path).cloned().ok_or_else(|| {
                        AppError::InvalidInput(
                            "configuration target changed while preparing".into(),
                        )
                    })?;
                }
            }
            Ok(plan)
        };
    let one = |file| Some(PreparedConfigWrite { files: vec![file] });
    bind(match app {
        AppType::Claude => one(PreparedFileWrite::replace(
            crate::paths::get_claude_settings_path(),
            json_bytes(&super::provider::live::sanitize_claude_settings_for_live(
                &provider.settings_config,
            ))?,
        )?),
        AppType::Codex => Some(crate::apps::codex::prepare_provider_files(provider)?),
        AppType::GrokBuild => {
            let text = provider
                .settings_config
                .get("config")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::Config("Grok configuration is missing config".into()))?;
            crate::apps::grokbuild::validate_config_toml_syntax(text)?;
            one(PreparedFileWrite::replace(
                crate::apps::grokbuild::get_grok_config_path(),
                text.as_bytes().to_vec(),
            )?)
        }
        AppType::KimiCode => one(PreparedFileWrite::replace(
            crate::apps::kimi_code::get_kimi_code_config_path(),
            crate::apps::kimi_code::prepare_provider_document(provider)?.into_bytes(),
        )?),
        AppType::ClaudeDesktop => Some(crate::apps::claude_desktop::prepare_provider_files(
            provider,
        )?),
        AppType::OpenCode => {
            if matches!(provider.category.as_deref(), Some("omo" | "omo-slim")) {
                return bind(Some(super::omo::OmoService::prepare_provider_files(
                    provider,
                )?));
            }
            let mut full = crate::apps::opencode::read_opencode_config()?;
            let mut fragment = provider.settings_config.clone();
            if fragment.get("$schema").is_some() || fragment.get("provider").is_some() {
                fragment = fragment
                    .get("provider")
                    .and_then(|p| p.get(&provider.id))
                    .cloned()
                    .unwrap_or(fragment);
            }
            if let Ok(typed) =
                serde_json::from_value::<crate::model::OpenCodeProviderConfig>(fragment.clone())
            {
                fragment = serde_json::to_value(typed)
                    .map_err(|source| AppError::JsonSerialize { source })?;
            }
            let root = full.as_object_mut().ok_or_else(|| {
                AppError::Config("OpenCode configuration root must be an object".into())
            })?;
            let providers = root
                .entry("provider")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| {
                    AppError::Config("OpenCode provider section must be an object".into())
                })?;
            providers.insert(provider.id.clone(), fragment);
            one(PreparedFileWrite::replace(
                crate::apps::opencode::get_opencode_config_path(),
                json_bytes(&full)?,
            )?)
        }
        AppType::OpenClaw => one(crate::apps::openclaw::prepare_provider_file(provider)?),
        AppType::Hermes => one(crate::apps::hermes::prepare_provider_file(provider)?),
        AppType::CherryStudio => None,
    })
}

pub(crate) fn json_bytes(value: &Value) -> Result<Vec<u8>, AppError> {
    serde_json::to_vec_pretty(value).map_err(|source| AppError::JsonSerialize { source })
}
