//! Keep a redacted placeholder from being saved over a real credential.
//!
//! Provider reads that leave the process are redacted: every secret-looking
//! field comes back as `******` (see `application::providers::redact_json`).
//! That is the right default for a list, but it makes the redacted copy unsafe
//! to save back — a caller that seeds an edit form from one and writes the whole
//! record returns the placeholder to storage, and the credential is gone.
//!
//! The damage is quiet, because the placeholder is not what the user sees next.
//! Codex, for one, has its key in two places: `auth.json` and the
//! `experimental_bearer_token` inside `config.toml`. Switching away from the
//! provider copies the live `auth.json` back over the stored one
//! (`capture_outgoing_account_state`), so the `auth` half heals itself and only
//! the `config` half stays masked. The key looks fine right up until something
//! reads the half that did not heal.
//!
//! So rather than trust every caller to hold an unredacted record, the save path
//! restores a masked field from the record it is replacing. A remote node cannot
//! hold one — it is never sent the secret — and its edits go through the same
//! seam.
//!
//! Codex and Grok carry a whole `config.toml` as a single string, where a masked
//! secret is invisible to a JSON walk. Those fields are parsed and walked as
//! TOML, which is also what the drift merge does with them and for the same
//! reason.

use serde_json::Value;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::app_type::AppType;
use crate::application::is_secret_key;
use crate::model::Provider;

/// Whether a string is a redaction placeholder rather than a credential.
///
/// The redactor writes a fixed run of asterisks, but matching any all-asterisk
/// string keeps this working if that width ever changes. An empty string is not
/// a placeholder: clearing a field is a real edit.
fn is_masked(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|character| character == '*')
}

fn is_masked_value(value: &Value) -> bool {
    value.as_str().is_some_and(is_masked)
}

/// Restore every masked secret in `incoming` from `existing`.
///
/// Returns whether anything was restored, so a caller can log it.
pub(crate) fn restore_masked_secrets(
    app_type: &AppType,
    incoming: &mut Provider,
    existing: &Provider,
) -> bool {
    let toml_text = super::drift::toml_text_paths(app_type);
    let mut restored = restore_json(
        &mut incoming.settings_config,
        Some(&existing.settings_config),
        toml_text,
        "",
    );

    // `meta.usage_script` credentials are redacted field by field rather than
    // through the JSON walk, so they need the same treatment here.
    if let (Some(incoming_meta), Some(existing_meta)) =
        (incoming.meta.as_mut(), existing.meta.as_ref())
        && let (Some(incoming_script), Some(existing_script)) = (
            incoming_meta.usage_script.as_mut(),
            existing_meta.usage_script.as_ref(),
        )
    {
        for (target, source) in [
            (
                &mut incoming_script.api_key,
                existing_script.api_key.as_deref(),
            ),
            (
                &mut incoming_script.access_token,
                existing_script.access_token.as_deref(),
            ),
        ] {
            if target.as_deref().is_some_and(is_masked)
                && let Some(prior) = source.filter(|value| !is_masked(value))
            {
                *target = Some(prior.to_string());
                restored = true;
            }
        }
    }

    restored
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn is_toml_text(path: &str, roots: &[&str]) -> bool {
    roots
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}.")))
}

fn restore_json(
    incoming: &mut Value,
    existing: Option<&Value>,
    toml_text: &[&str],
    path: &str,
) -> bool {
    match incoming {
        Value::Object(map) => {
            let existing_map = existing.and_then(Value::as_object);
            let mut restored = false;
            for (key, value) in map.iter_mut() {
                let child_path = join_path(path, key);
                let existing_child = existing_map.and_then(|map| map.get(key));

                if is_secret_key(key) && is_masked_value(value) {
                    if let Some(prior) = existing_child.filter(|prior| !is_masked_value(prior)) {
                        *value = prior.clone();
                        restored = true;
                    }
                    continue;
                }

                if is_toml_text(&child_path, toml_text) {
                    if let (Some(text), Some(prior)) =
                        (value.as_str(), existing_child.and_then(Value::as_str))
                        && let Some(merged) = restore_toml(text, prior)
                    {
                        *value = Value::String(merged);
                        restored = true;
                    }
                    continue;
                }

                restored |= restore_json(value, existing_child, toml_text, &child_path);
            }
            restored
        }
        // `extraHeaders`-style `["x-api-key", "******"]` pairs: the key is the
        // first element rather than an object key, so match the prior pair by it.
        Value::Array(items) => {
            let existing_items = existing.and_then(Value::as_array);
            let mut restored = false;
            for (index, item) in items.iter_mut().enumerate() {
                let existing_item = existing_items.and_then(|items| items.get(index));
                if let Value::Array(pair) = item
                    && pair.len() == 2
                    && pair[0].as_str().is_some_and(is_secret_key)
                    && is_masked_value(&pair[1])
                {
                    let name = pair[0].as_str().unwrap_or_default();
                    let prior = existing_items
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_array)
                        .find(|candidate| {
                            candidate.len() == 2 && candidate[0].as_str() == Some(name)
                        })
                        .map(|candidate| &candidate[1])
                        .filter(|prior| !is_masked_value(prior));
                    if let Some(prior) = prior {
                        pair[1] = prior.clone();
                        restored = true;
                        continue;
                    }
                }
                restored |= restore_json(item, existing_item, toml_text, path);
            }
            restored
        }
        _ => false,
    }
}

/// Restore masked secrets inside a `config.toml` carried as one string.
///
/// Returns `None` when nothing changed, so an unaffected document keeps its
/// original bytes — comments and layout included — instead of being round-tripped
/// through the parser for no reason.
fn restore_toml(incoming: &str, existing: &str) -> Option<String> {
    if !incoming.contains('*') {
        return None;
    }
    let mut document = incoming.parse::<DocumentMut>().ok()?;
    let prior = existing.parse::<DocumentMut>().ok()?;
    restore_toml_table(document.as_table_mut(), prior.as_table()).then(|| document.to_string())
}

fn restore_toml_table(target: &mut dyn TableLike, source: &dyn TableLike) -> bool {
    let keys = target
        .iter()
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    let mut restored = false;

    for key in keys {
        let Some(item) = target.get_mut(&key) else {
            continue;
        };

        if is_secret_key(&key) {
            if item.as_str().is_some_and(is_masked)
                && let Some(prior) = source
                    .get(&key)
                    .and_then(Item::as_str)
                    .filter(|prior| !is_masked(prior))
            {
                *item = toml_edit::value(prior);
                restored = true;
            }
            continue;
        }

        if let Some(table) = item.as_table_like_mut()
            && let Some(prior) = source.get(&key).and_then(Item::as_table_like)
        {
            restored |= restore_toml_table(table, prior);
        }
    }

    restored
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(settings: Value) -> Provider {
        Provider::with_id("p".to_string(), "p".to_string(), settings, None)
    }

    fn restore(app_type: AppType, incoming: Value, existing: Value) -> (Value, bool) {
        let mut incoming = provider(incoming);
        let restored = restore_masked_secrets(&app_type, &mut incoming, &provider(existing));
        (incoming.settings_config, restored)
    }

    #[test]
    fn a_masked_env_key_is_restored_from_the_stored_provider() {
        let (settings, restored) = restore(
            AppType::Claude,
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "******", "ANTHROPIC_MODEL": "opus" } }),
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "sk-real", "ANTHROPIC_MODEL": "sonnet" } }),
        );
        assert!(restored);
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-real");
        // A real edit beside the masked field still lands.
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "opus");
    }

    #[test]
    fn a_real_new_key_is_never_overwritten_by_the_stored_one() {
        let (settings, restored) = restore(
            AppType::Claude,
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "sk-new" } }),
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "sk-old" } }),
        );
        assert!(!restored);
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-new");
    }

    #[test]
    fn clearing_a_key_is_a_real_edit_rather_than_a_placeholder() {
        let (settings, restored) = restore(
            AppType::Claude,
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "" } }),
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "sk-old" } }),
        );
        assert!(!restored);
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "");
    }

    /// The shape that shipped the bug: the key lives inside the `config.toml`
    /// text, where a JSON walk cannot see it.
    #[test]
    fn a_masked_codex_bearer_token_inside_config_toml_is_restored() {
        let incoming = "model = \"gpt-5.6\"\n\n[model_providers.relay]\nbase_url = \"https://a.example/v1\"\nexperimental_bearer_token = \"******\"\n";
        let existing = "model = \"gpt-5.5\"\n\n[model_providers.relay]\nbase_url = \"https://a.example/v1\"\nexperimental_bearer_token = \"sk-real\"\n";

        let (settings, restored) = restore(
            AppType::Codex,
            json!({ "auth": { "OPENAI_API_KEY": "******" }, "config": incoming }),
            json!({ "auth": { "OPENAI_API_KEY": "sk-real" }, "config": existing }),
        );

        assert!(restored);
        assert_eq!(settings["auth"]["OPENAI_API_KEY"], "sk-real");
        let config = settings["config"].as_str().unwrap();
        assert!(
            config.contains("experimental_bearer_token = \"sk-real\""),
            "{config}"
        );
        // The rest of the document is the caller's edit, not the stored one.
        assert!(config.contains("model = \"gpt-5.6\""), "{config}");
    }

    #[test]
    fn a_codex_config_without_a_placeholder_keeps_its_exact_bytes() {
        let text = "# hand-written\nmodel = \"gpt-5.6\"\n\n[model_providers.relay]\nexperimental_bearer_token = \"sk-new\"\n";
        let (settings, restored) = restore(
            AppType::Codex,
            json!({ "config": text }),
            json!({ "config": "model = \"gpt-5.5\"\n" }),
        );
        assert!(!restored);
        assert_eq!(settings["config"].as_str().unwrap(), text);
    }

    #[test]
    fn a_masked_header_pair_is_restored_by_name_rather_than_position() {
        let (settings, restored) = restore(
            AppType::Claude,
            json!({ "extraHeaders": [["x-trace", "on"], ["x-api-key", "******"]] }),
            json!({ "extraHeaders": [["x-api-key", "sk-real"], ["x-trace", "off"]] }),
        );
        assert!(restored);
        assert_eq!(settings["extraHeaders"][1][1], "sk-real");
    }

    #[test]
    fn a_masked_field_with_nothing_stored_behind_it_is_left_alone() {
        let (settings, restored) = restore(
            AppType::Claude,
            json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "******" } }),
            json!({ "env": {} }),
        );
        assert!(!restored);
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "******");
    }
}
