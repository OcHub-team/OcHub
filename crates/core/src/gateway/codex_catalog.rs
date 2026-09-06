//! Codex-native model catalog served at `/backend-api/codex/models`.
//!
//! Codex in ChatGPT-login mode fetches its model picker from
//! `{base_url}/models`. The response must be the native catalog shape
//! (`{"models": [...]}`, each entry carrying `base_instructions` or
//! `model_messages.instructions_template`), not the OpenAI-style `/v1/models`
//! list. Unknown models use conservative capabilities and neutral instructions.
//! The bundled gpt-5.5 template is used only for that exact model. An optional
//! official catalog supplies exact-slug templates, with per-model overrides from
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
    // Hash the complete credential identity; do not retain raw tokens in cache.
    identity: [u8; 32],
    fetched_at: Option<Instant>,
    failed: bool,
    catalog: Option<Value>,
    refresh: Option<tokio::task::JoinHandle<()>>,
}

impl UpstreamCacheEntry {
    fn needs_refresh(&self) -> bool {
        if self
            .refresh
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return false;
        }
        let ttl = if self.failed {
            UPSTREAM_CACHE_FAILURE_TTL
        } else {
            UPSTREAM_CACHE_SUCCESS_TTL
        };
        self.fetched_at.is_none_or(|time| time.elapsed() >= ttl)
    }
}

static UPSTREAM_CACHE: Lazy<Mutex<Option<UpstreamCacheEntry>>> = Lazy::new(|| Mutex::new(None));

fn credential_identity(token: &str, account: Option<&str>) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(token.len().to_be_bytes());
    hash.update(token.as_bytes());
    hash.update(account.unwrap_or_default().as_bytes());
    hash.finalize().into()
}

/// Read the last usable catalog and schedule at most one refresh. Neither a
/// cold cache nor an expired cache puts network I/O on the response path.
/// Failed refreshes retain the previous catalog and retry after a short TTL.
async fn upstream_catalog(config: &GatewayConfig) -> Option<Value> {
    let token = config
        .codex_catalog_upstream_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())?;
    let account = config
        .codex_catalog_upstream_account_id
        .as_deref()
        .map(str::trim)
        .filter(|account| !account.is_empty());
    let identity = credential_identity(token, account);
    let mut cache = UPSTREAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache
        .as_ref()
        .is_none_or(|entry| entry.identity != identity)
    {
        if let Some(task) = cache.as_mut().and_then(|entry| entry.refresh.take()) {
            task.abort();
        }
        *cache = Some(UpstreamCacheEntry {
            identity,
            fetched_at: None,
            failed: false,
            catalog: None,
            refresh: None,
        });
    }
    let entry = cache.as_mut().expect("cache initialized above");
    if entry.needs_refresh() {
        let token = token.to_owned();
        let account = account.map(str::to_owned);
        entry.refresh = Some(tokio::spawn(async move {
            let result = crate::services::codex_oauth_models::fetch_catalog_with_token(
                &token,
                account.as_deref(),
            )
            .await;
            let catalog = match result {
                Ok(value) if value.get("models").and_then(Value::as_array).is_some() => Some(value),
                _ => {
                    log::warn!(
                        "[gateway] codex catalog refresh failed; retaining cached templates"
                    );
                    None
                }
            };
            let mut cache = UPSTREAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.as_mut().filter(|entry| entry.identity == identity) {
                entry.failed = catalog.is_none();
                entry.fetched_at = Some(Instant::now());
                if let Some(catalog) = catalog {
                    entry.catalog = Some(catalog);
                }
            }
        }));
    }
    entry.catalog.clone()
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
/// official catalog entry for the same slug when available. Only gpt-5.5
/// uses the bundled template; other models start neutral. Local overrides win.
fn catalog_entry(
    model: &str,
    index: usize,
    upstream_entry: Option<&Value>,
    overrides: Option<&CodexModelOverride>,
) -> Value {
    let template = upstream_entry.or_else(|| (model == "gpt-5.5").then(fallback_template));
    let mut entry = template.cloned().unwrap_or_else(|| json!({
        "base_instructions": "You are a coding assistant. Use the available tools to complete the user's task.",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [],
        "visibility": "list",
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "apply_patch_tool_type": null,
        "web_search_tool_type": "text",
        "truncation_policy": { "mode": "tokens", "limit": 10000 },
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 32768,
        "max_context_window": 32768,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text"],
        "supports_search_tool": false
    }));
    let exact_template = template.is_some();
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

    if let Some(budget) = obj
        .get_mut("model_messages")
        .and_then(|m| m.get_mut("token_budget"))
        .and_then(Value::as_object_mut)
    {
        budget.insert("enabled".into(), json!(false));
        budget.insert("use_history_notes_extension".into(), json!(false));
    }
    obj.insert("slug".to_string(), json!(model));
    obj.insert("display_name".to_string(), json!(display_name));
    if let Some(description) = overrides.and_then(|o| take_string(o.description.as_ref())) {
        obj.insert("description".to_string(), json!(description));
    } else if !exact_template {
        obj.insert("description".to_string(), json!(display_name));
    }
    if !exact_template {
        // Unknown models must not advertise OpenAI product capabilities.
        obj.insert("priority".to_string(), json!(index as i64));
        obj.insert("additional_speed_tiers".to_string(), json!([]));
        obj.insert("service_tiers".to_string(), json!([]));
    }
    obj.insert("shell_type".to_string(), json!("unified_exec"));
    obj.insert("supported_in_api".to_string(), json!(true));
    obj.insert(
        "use_responses_lite".to_string(),
        json!(
            overrides
                .and_then(|o| o.use_responses_lite)
                .unwrap_or(false)
        ),
    );
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
            if !obj.get("model_messages").is_some_and(Value::is_object) {
                let instructions = obj
                    .get("base_instructions")
                    .and_then(Value::as_str)
                    .unwrap_or("You are a coding assistant.")
                    .to_owned();
                obj.insert(
                    "model_messages".to_string(),
                    json!({
                        "instructions_template": instructions, "instructions_variables": {}
                    }),
                );
            }
            let messages = obj
                .get_mut("model_messages")
                .expect("object inserted above");
            if let Some(messages) = messages.as_object_mut() {
                messages.insert(
                    "token_budget".to_string(),
                    serde_json::to_value(token_budget).unwrap_or(Value::Null),
                );
            }
        }
    }

    // Codex rejects entries missing both instruction sources.
    let has_instructions = obj
        .get("base_instructions")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
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
    let mut config = state.config.read().await.clone();
    if let Some(route_id) = key.and_then(|key| key.route_id.as_deref())
        && let Ok(Some(route)) = state.db.get_gateway_route_by_id(route_id)
    {
        // Capability declarations belong to the selected upstream model, including aliases.
        for model in &models {
            let policy = key.and_then(|key| key.model_policy.as_ref());
            let rule = crate::gateway::pipeline::request_model_rule(policy, Some(&route), model);
            let upstream_model =
                crate::gateway::pipeline::request_model_override(rule, policy, Some(&route))
                    .unwrap_or(model);
            if let Some(capabilities) = route.model_capabilities.get(upstream_model) {
                config
                    .codex_model_overrides
                    .insert(model.clone(), capabilities.clone());
            } else {
                config.codex_model_overrides.remove(model);
            }
        }
    }
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
    fn unknown_models_use_neutral_capabilities_and_credentials_include_account() {
        let body = catalog_body(&["third-party".into()], &GatewayConfig::default(), None);
        let models = catalog_models(&body);
        assert_eq!(models[0]["input_modalities"], json!(["text"]));
        assert_eq!(models[0]["context_window"], 32768);
        assert_eq!(models[0]["supports_reasoning_summaries"], false);
        assert!(models[0]["apply_patch_tool_type"].is_null());
        assert_ne!(
            credential_identity("token", Some("a")),
            credential_identity("token", Some("b"))
        );
    }

    #[tokio::test]
    async fn refresh_is_single_flight_and_retries_after_failure_or_cancellation() {
        let task = tokio::spawn(std::future::pending::<()>());
        let mut entry = UpstreamCacheEntry {
            identity: credential_identity("test", None),
            fetched_at: None,
            failed: false,
            catalog: Some(json!({"models": []})),
            refresh: Some(task),
        };
        assert!(!entry.needs_refresh());
        let task = entry.refresh.take().unwrap();
        task.abort();
        let _ = task.await;
        assert!(entry.needs_refresh());
        entry.failed = true;
        entry.fetched_at = Some(Instant::now());
        assert!(!entry.needs_refresh());
        entry.fetched_at = Some(Instant::now() - UPSTREAM_CACHE_FAILURE_TTL);
        assert!(entry.needs_refresh());
        assert!(entry.catalog.is_some());
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
                use_responses_lite: None,
                remote_compaction: None,
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
                use_responses_lite: None,
                remote_compaction: None,
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
