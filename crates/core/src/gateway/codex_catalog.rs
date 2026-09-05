//! Codex-native model catalog served at `/backend-api/codex/models`.
//!
//! Codex in ChatGPT-login mode fetches its model picker from
//! `{base_url}/models`. The response must be the native catalog shape
//! (`{"models": [...]}`, each entry carrying `base_instructions` or
//! `model_messages.instructions_template`), not the OpenAI-style `/v1/models`
//! list. Entries are synthesized from a bundled gpt-5.5 template, optionally
//! using the official catalog as per-slug templates when the operator
//! configured a pooled OAuth token, with per-model overrides from
//! [`GatewayConfig::codex_model_overrides`] applied last.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::gateway::pipeline::GatewayState;
use crate::gateway::types::{CodexModelOverride, GatewayConfig, GatewayKey};

/// Bundled real Codex catalog entry used as the synthesis template. Its
/// `base_instructions` are the default system prompt for entries that have
/// no upstream template.
fn fallback_template() -> &'static Value {
    static TEMPLATE: Lazy<Value> = Lazy::new(|| {
        serde_json::from_str(include_str!("../resources/gpt5_5_template.json"))
            .expect("bundled gpt-5.5 template must be valid JSON")
    });
    &TEMPLATE
}

fn fallback_base_instructions() -> &'static str {
    fallback_template()
        .get("base_instructions")
        .and_then(Value::as_str)
        .expect("bundled gpt-5.5 template must carry base_instructions")
}

// ---------------------------------------------------------------------------
// Optional official-catalog template source (pooled OAuth token)
// ---------------------------------------------------------------------------

const UPSTREAM_CACHE_SUCCESS_TTL: Duration = Duration::from_secs(600);
const UPSTREAM_CACHE_FAILURE_TTL: Duration = Duration::from_secs(60);

struct UpstreamCacheEntry {
    token: String,
    fetched_at: Instant,
    /// `None` records a failed fetch so a flapping upstream is not retried on
    /// every request.
    catalog: Option<Value>,
}

static UPSTREAM_CACHE: Lazy<Mutex<Option<UpstreamCacheEntry>>> = Lazy::new(|| Mutex::new(None));

/// Fetch the official Codex catalog when a pooled token is configured.
/// Failures are cached briefly and never fail the local catalog.
async fn upstream_catalog(config: &GatewayConfig) -> Option<Value> {
    let token = config
        .codex_catalog_upstream_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())?;
    {
        let cache = UPSTREAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.as_ref()
            && entry.token == token
        {
            let ttl = if entry.catalog.is_some() {
                UPSTREAM_CACHE_SUCCESS_TTL
            } else {
                UPSTREAM_CACHE_FAILURE_TTL
            };
            if entry.fetched_at.elapsed() < ttl {
                return entry.catalog.clone();
            }
        }
    }
    let fetched = crate::services::codex_oauth_models::fetch_catalog_with_token(
        token,
        config.codex_catalog_upstream_account_id.as_deref(),
    )
    .await;
    let catalog = match fetched {
        Ok(value) if value.get("models").and_then(Value::as_array).is_some() => Some(value),
        Ok(_) => {
            log::warn!("[gateway] codex upstream catalog response has no models array");
            None
        }
        Err(error) => {
            log::warn!("[gateway] codex upstream catalog fetch failed: {error}");
            None
        }
    };
    let mut cache = UPSTREAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some(UpstreamCacheEntry {
        token: token.to_string(),
        fetched_at: Instant::now(),
        catalog: catalog.clone(),
    });
    catalog
}

// ---------------------------------------------------------------------------
// Catalog synthesis
// ---------------------------------------------------------------------------

fn find_upstream_entry<'a>(upstream: Option<&'a Value>, slug: &str) -> Option<&'a Value> {
    upstream?
        .get("models")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            entry
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(slug))
        })
}

fn take_string(value: Option<&String>) -> Option<String> {
    value
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build one catalog entry for a client-visible model. `upstream_entry` is the
/// official catalog entry for the same slug when available; otherwise the
/// bundled gpt-5.5 entry is the template. Local overrides always win.
fn catalog_entry(
    model: &str,
    index: usize,
    upstream_entry: Option<&Value>,
    overrides: Option<&CodexModelOverride>,
) -> Value {
    let mut entry = upstream_entry
        .cloned()
        .unwrap_or_else(|| fallback_template().clone());
    let exact_template = upstream_entry.is_some();
    let Some(obj) = entry.as_object_mut() else {
        return json!({});
    };

    let display_name = overrides
        .and_then(|o| take_string(o.display_name.as_ref()))
        .or_else(|| {
            exact_template
                .then(|| obj.get("display_name").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
        .unwrap_or_else(|| model.to_string());

    obj.insert("slug".to_string(), json!(model));
    obj.insert("display_name".to_string(), json!(display_name));
    if let Some(description) = overrides.and_then(|o| take_string(o.description.as_ref())) {
        obj.insert("description".to_string(), json!(description));
    } else if !exact_template {
        obj.insert("description".to_string(), json!(display_name));
    }
    if !exact_template {
        // A third-party model cloned from the fallback entry must not inherit
        // OpenAI product capabilities or onboarding nudges.
        obj.insert("priority".to_string(), json!(index as i64));
        obj.insert("additional_speed_tiers".to_string(), json!([]));
        obj.insert("service_tiers".to_string(), json!([]));
    }
    obj.insert("shell_type".to_string(), json!("unified_exec"));
    obj.insert("supported_in_api".to_string(), json!(true));
    obj.insert("use_responses_lite".to_string(), json!(false));
    obj.insert("availability_nux".to_string(), Value::Null);
    obj.insert("upgrade".to_string(), Value::Null);
    if let Some(visibility) = overrides.and_then(|o| take_string(o.visibility.as_ref())) {
        obj.insert("visibility".to_string(), json!(visibility));
    }

    if let Some(overrides) = overrides {
        if let Some(context_window) = overrides.context_window.filter(|value| *value > 0) {
            obj.insert("context_window".to_string(), json!(context_window));
            obj.insert("max_context_window".to_string(), json!(context_window));
        }
        if let Some(levels) = &overrides.supported_reasoning_levels {
            obj.insert(
                "supported_reasoning_levels".to_string(),
                json!(levels
                    .iter()
                    .map(|level| json!({ "effort": level.effort, "description": level.description }))
                    .collect::<Vec<_>>()),
            );
        }
        if let Some(default) = take_string(overrides.default_reasoning_level.as_ref()) {
            obj.insert("default_reasoning_level".to_string(), json!(default));
        }
        if let Some(token_budget) = &overrides.token_budget {
            let messages = obj
                .entry("model_messages".to_string())
                .or_insert_with(|| json!({}));
            if let Some(messages) = messages.as_object_mut() {
                messages.insert(
                    "token_budget".to_string(),
                    serde_json::to_value(token_budget).unwrap_or(Value::Null),
                );
            }
        }
    }

    // Codex rejects entries missing both instruction sources.
    let has_instructions = obj.contains_key("base_instructions")
        || obj
            .get("model_messages")
            .and_then(|messages| messages.get("instructions_template"))
            .is_some_and(|template| !template.is_null());
    if !has_instructions {
        obj.insert(
            "base_instructions".to_string(),
            json!(fallback_base_instructions()),
        );
    }

    entry
}

/// Serialize the full catalog body for the given client-visible models.
/// Pure so tests can exercise it without a running gateway.
pub(crate) fn catalog_body(
    models: &[String],
    config: &GatewayConfig,
    upstream: Option<&Value>,
) -> String {
    let entries: Vec<Value> = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            catalog_entry(
                model,
                index,
                find_upstream_entry(upstream, model),
                config.codex_model_overrides.get(model),
            )
        })
        .collect();
    json!({ "models": entries }).to_string()
}

/// Stable content hash for the catalog body, quoted per HTTP ETag convention.
/// The same value is sent as `ETag` on `/backend-api/codex/models` and as
/// `X-Models-Etag` on `/backend-api/codex/responses` replies.
pub(crate) fn catalog_etag(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("\"{hex}\"")
}

/// Build the catalog body + ETag for one authorized caller.
pub(crate) async fn build_catalog(
    state: &GatewayState,
    key: Option<&GatewayKey>,
) -> Result<(String, String), axum::response::Response> {
    let models = crate::gateway::server::visible_models(state, key).map_err(|resp| *resp)?;
    let config = state.config.read().await.clone();
    let upstream = upstream_catalog(&config).await;
    let body = catalog_body(&models, &config, upstream.as_ref());
    let etag = catalog_etag(&body);
    Ok((body, etag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::CodexTokenBudgetConfig;

    fn catalog_models(body: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(body)
            .unwrap()
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap()
    }

    #[test]
    fn synthesized_entries_carry_all_required_fields() {
        let config = GatewayConfig::default();
        let body = catalog_body(&["gpt-x".to_string(), "gpt-y".to_string()], &config, None);
        let models = catalog_models(&body);
        assert_eq!(models.len(), 2);
        let entry = &models[0];
        assert_eq!(entry["slug"], "gpt-x");
        assert_eq!(entry["display_name"], "gpt-x");
        assert_eq!(entry["shell_type"], "unified_exec");
        assert_eq!(entry["visibility"], "list");
        assert_eq!(entry["supported_in_api"], true);
        assert_eq!(entry["priority"], 0);
        assert!(entry["description"].is_string());
        assert!(entry["default_reasoning_level"].is_string());
        assert!(
            entry["supported_reasoning_levels"]
                .as_array()
                .is_some_and(|levels| levels.iter().all(|level| level.get("effort").is_some()))
        );
        assert_eq!(entry["truncation_policy"]["mode"], "tokens");
        assert!(entry["truncation_policy"]["limit"].is_u64());
        assert!(entry["context_window"].is_u64());
        assert!(
            entry["base_instructions"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        // OpenAI product capabilities must not leak onto arbitrary models.
        assert_eq!(entry["additional_speed_tiers"], json!([]));
        assert_eq!(entry["use_responses_lite"], json!(false));
        assert!(entry["availability_nux"].is_null());
    }

    #[test]
    fn overrides_win_and_token_budget_lands_under_model_messages() {
        let mut config = GatewayConfig::default();
        config.codex_model_overrides.insert(
            "gpt-x".to_string(),
            CodexModelOverride {
                display_name: Some("Custom X".to_string()),
                description: Some("custom description".to_string()),
                default_reasoning_level: Some("high".to_string()),
                supported_reasoning_levels: Some(vec![
                    crate::gateway::types::CodexReasoningLevel {
                        effort: "high".to_string(),
                        description: "deep".to_string(),
                    },
                ]),
                context_window: Some(64_000),
                visibility: Some("hidden".to_string()),
                token_budget: Some(CodexTokenBudgetConfig {
                    enabled: true,
                    reminder_threshold_tokens: 8_000,
                    reminder_message_template: "{n_remaining} left".to_string(),
                    ..Default::default()
                }),
            },
        );
        let body = catalog_body(&["gpt-x".to_string()], &config, None);
        let entry = &catalog_models(&body)[0];
        assert_eq!(entry["display_name"], "Custom X");
        assert_eq!(entry["description"], "custom description");
        assert_eq!(entry["default_reasoning_level"], "high");
        assert_eq!(
            entry["supported_reasoning_levels"],
            json!([{ "effort": "high", "description": "deep" }])
        );
        assert_eq!(entry["context_window"], 64_000);
        assert_eq!(entry["max_context_window"], 64_000);
        assert_eq!(entry["visibility"], "hidden");
        assert_eq!(entry["model_messages"]["token_budget"]["enabled"], true);
        assert_eq!(
            entry["model_messages"]["token_budget"]["reminder_threshold_tokens"],
            8_000
        );
    }

    #[test]
    fn upstream_entry_is_used_as_template_and_local_override_wins() {
        let upstream = json!({
            "models": [{
                "slug": "gpt-x",
                "display_name": "GPT X Official",
                "description": "official description",
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [{ "effort": "low", "description": "low" }],
                "shell_type": "unified_exec",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 7,
                "support_verbosity": false,
                "truncation_policy": { "mode": "tokens", "limit": 10_000 },
                "context_window": 400_000,
                "experimental_supported_tools": [],
                "model_messages": { "instructions_template": "official instructions" }
            }]
        });
        let mut config = GatewayConfig::default();
        config.codex_model_overrides.insert(
            "gpt-x".to_string(),
            CodexModelOverride {
                display_name: Some("Local Name".to_string()),
                ..Default::default()
            },
        );
        let body = catalog_body(&["gpt-x".to_string()], &config, Some(&upstream));
        let entry = &catalog_models(&body)[0];
        assert_eq!(entry["display_name"], "Local Name");
        assert_eq!(entry["description"], "official description");
        assert_eq!(entry["priority"], 7);
        assert_eq!(entry["context_window"], 400_000);
        assert_eq!(
            entry["model_messages"]["instructions_template"],
            "official instructions"
        );
    }

    #[test]
    fn etag_is_stable_and_content_addressed() {
        let config = GatewayConfig::default();
        let models = vec!["gpt-x".to_string()];
        let first = catalog_body(&models, &config, None);
        let second = catalog_body(&models, &config, None);
        assert_eq!(first, second);
        assert_eq!(catalog_etag(&first), catalog_etag(&second));

        let other = catalog_body(&["gpt-z".to_string()], &config, None);
        assert_ne!(catalog_etag(&first), catalog_etag(&other));
    }
}
