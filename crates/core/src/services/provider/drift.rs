//! External-edit detection for live configuration files.
//!
//! OcHub's source of truth is the database, but the files it writes are shared
//! with whoever else touches them — the user in an editor, and the tools
//! themselves. Recording what OcHub *last wrote* turns the useless question
//! "does the file differ from the database?" into the two that matter: what did
//! somebody else change, and does that collide with what we are about to write?
//!
//! Everything that did not collide is carried onto the next configuration, so a
//! hand-added `hooks` block survives a provider switch instead of being absorbed
//! into the outgoing provider's stored settings.
//!
//! The snapshot store is device-local (`~/.ochub/live_snapshots.json`) on
//! purpose: a live file belongs to one machine, so a baseline must never ride DB
//! cloud sync to another machine and be mistaken for that machine's state.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::app_type::AppType;
use crate::error::AppError;

/// Snapshot file name under the app config dir.
const STORE_FILE: &str = "live_snapshots.json";

/// Subtrees that belong to the *outgoing* provider rather than to the file, and
/// so must never be carried forward onto the next provider.
///
/// Codex `auth` holds account login material: a token refreshed by Codex itself
/// belongs to the account that was active, not to whatever provider is being
/// switched to. Carrying it forward would hand one account's credentials to
/// another provider.
fn protected_paths(app_type: &AppType) -> &'static [&'static str] {
    match app_type {
        AppType::Codex => &["auth"],
        _ => &[],
    }
}

/// Whether this app participates in drift tracking.
///
/// Additive-mode apps already merge per provider key, so an external edit
/// elsewhere in their config file is never at risk. Claude Desktop has no
/// readable generic live config (`read_live_settings` rejects it).
pub(crate) fn tracks_live_drift(app_type: &AppType) -> bool {
    matches!(
        app_type,
        AppType::Claude | AppType::Codex | AppType::GrokBuild
    )
}

/// The live config file an external edit would have landed in, abbreviated for
/// display. A drift report is meaningless without saying *which* file drifted.
pub fn live_config_label(app_type: &AppType) -> String {
    let path = match app_type {
        AppType::Codex => crate::apps::codex::get_codex_config_path(),
        AppType::GrokBuild => crate::apps::grokbuild::get_grok_config_path(),
        _ => crate::paths::get_claude_settings_path(),
    };
    crate::paths::abbreviate_home(&path)
}

/// What OcHub last wrote to one app's live config, shaped exactly as
/// [`read_live_settings`](super::live::read_live_settings) reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    /// Provider the snapshot was written for.
    pub provider_id: String,
    /// The effective settings as read back from disk.
    pub settings: Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SnapshotStore {
    #[serde(default)]
    apps: HashMap<String, LiveSnapshot>,
}

/// One key the user changed that the incoming configuration also sets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriftConflict {
    /// Dotted path into the settings object, e.g. `env.ANTHROPIC_BASE_URL`.
    pub path: String,
    /// What the user's edit left on disk.
    pub live: Value,
    /// What OcHub is about to write, which wins.
    pub incoming: Value,
}

/// What an external edit contained, relative to what OcHub last wrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveDrift {
    /// Paths the user added or changed that were carried onto the new config.
    pub preserved: Vec<String>,
    /// Paths the user deleted that stayed deleted.
    pub removed: Vec<String>,
    /// Paths where the user's edit lost to the incoming configuration.
    pub conflicts: Vec<DriftConflict>,
}

impl LiveDrift {
    /// Whether anything was changed outside OcHub at all.
    pub fn is_empty(&self) -> bool {
        self.preserved.is_empty() && self.removed.is_empty() && self.conflicts.is_empty()
    }

    /// Whether the user has to be asked: their edit and ours disagree.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Three-way merge
// ---------------------------------------------------------------------------

/// Merge an external edit onto the configuration OcHub is about to write.
///
/// - `base` — what OcHub last wrote (the shared ancestor)
/// - `live` — what is on disk now
/// - `incoming` — what OcHub wants to write
///
/// Keys the user touched that `incoming` leaves alone are carried over; keys
/// both sides changed are reported as conflicts and resolved in favour of
/// `incoming`, because writing it is the action the user just asked for.
pub(crate) fn merge_user_edits(
    app_type: &AppType,
    base: &Value,
    live: &Value,
    incoming: &Value,
) -> (Value, LiveDrift) {
    let mut drift = LiveDrift::default();

    let (Some(base_map), Some(live_map)) = (base.as_object(), live.as_object()) else {
        // Without two comparable objects there is no ancestor to diff against;
        // writing `incoming` unchanged is the only defined answer.
        return (incoming.clone(), drift);
    };
    let incoming_map = incoming.as_object().cloned().unwrap_or_default();

    let merged = merge_objects(
        base_map,
        live_map,
        &incoming_map,
        "",
        protected_paths(app_type),
        &mut drift,
    );

    (Value::Object(merged), drift)
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn is_protected(path: &str, protected: &[&str]) -> bool {
    protected
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}.")))
}

fn merge_objects(
    base: &Map<String, Value>,
    live: &Map<String, Value>,
    incoming: &Map<String, Value>,
    prefix: &str,
    protected: &[&str],
    drift: &mut LiveDrift,
) -> Map<String, Value> {
    let mut out = incoming.clone();

    // Every key either side of the ancestor knows about. `incoming`-only keys
    // need no decision: nobody edited them, so they pass through untouched.
    let mut keys: Vec<&String> = base.keys().collect();
    for key in live.keys() {
        if !base.contains_key(key) {
            keys.push(key);
        }
    }

    for key in keys {
        let path = join_path(prefix, key);
        if is_protected(&path, protected) {
            continue;
        }

        let base_value = base.get(key);
        let live_value = live.get(key);
        let incoming_value = incoming.get(key);

        match (base_value, live_value) {
            // Added outside OcHub.
            (None, Some(live_value)) => match incoming_value {
                None => {
                    out.insert(key.clone(), live_value.clone());
                    drift.preserved.push(path);
                }
                Some(incoming_value) if incoming_value == live_value => {}
                Some(incoming_value) => drift.conflicts.push(DriftConflict {
                    path,
                    live: live_value.clone(),
                    incoming: incoming_value.clone(),
                }),
            },

            // Deleted outside OcHub.
            (Some(base_value), None) => match incoming_value {
                None => {}
                Some(incoming_value) if incoming_value == base_value => {
                    out.remove(key);
                    drift.removed.push(path);
                }
                Some(incoming_value) => drift.conflicts.push(DriftConflict {
                    path,
                    live: Value::Null,
                    incoming: incoming_value.clone(),
                }),
            },

            (Some(base_value), Some(live_value)) if base_value != live_value => {
                let nested = base_value
                    .as_object()
                    .zip(live_value.as_object())
                    .filter(|_| incoming_value.is_none_or(Value::is_object));

                if let Some((base_child, live_child)) = nested {
                    // Recurse so an edit deep inside `env` does not have to
                    // fight the whole object for ownership.
                    let incoming_child = incoming_value
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let merged = merge_objects(
                        base_child,
                        live_child,
                        &incoming_child,
                        &path,
                        protected,
                        drift,
                    );

                    // An empty result means the subtree existed only to hold the
                    // outgoing provider's values; do not leave a husk behind.
                    if merged.is_empty() && incoming_value.is_none() {
                        out.remove(key);
                    } else {
                        out.insert(key.clone(), Value::Object(merged));
                    }
                    continue;
                }

                match incoming_value {
                    // The new configuration says nothing here, or says exactly
                    // what the old one did — either way the user's edit stands.
                    None => {
                        out.insert(key.clone(), live_value.clone());
                        drift.preserved.push(path);
                    }
                    Some(incoming_value) if incoming_value == base_value => {
                        out.insert(key.clone(), live_value.clone());
                        drift.preserved.push(path);
                    }
                    Some(incoming_value) if incoming_value == live_value => {}
                    Some(incoming_value) => drift.conflicts.push(DriftConflict {
                        path,
                        live: live_value.clone(),
                        incoming: incoming_value.clone(),
                    }),
                }
            }

            // Untouched by the user: `incoming` already owns it.
            _ => {}
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Snapshot store
// ---------------------------------------------------------------------------

/// Serializes read-modify-write of the store file across in-process callers.
fn store_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

fn store_path() -> std::path::PathBuf {
    crate::paths::get_app_config_dir().join(STORE_FILE)
}

/// The store is read fresh on every access rather than cached: the config dir is
/// resolved per call (tests relocate `HOME`), and live writes are user-initiated,
/// never a hot path.
fn load_store() -> SnapshotStore {
    let path = store_path();
    if !path.exists() {
        return SnapshotStore::default();
    }
    match crate::paths::read_json_file::<SnapshotStore>(&path) {
        Ok(store) => store,
        Err(err) => {
            // A corrupt baseline must not block writing live configs; losing it
            // only costs one round of drift detection.
            log::warn!("failed to read live snapshot store, starting empty: {err}");
            SnapshotStore::default()
        }
    }
}

fn save_store(store: &SnapshotStore) -> Result<(), AppError> {
    let path = store_path();
    crate::paths::write_json_file(&path, store)?;

    // Snapshots mirror live configs, which carry API keys.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

/// Read the recorded baseline for an app, if any.
pub(crate) fn load_snapshot(app_type: &AppType) -> Option<LiveSnapshot> {
    let _guard = store_lock();
    load_store().apps.remove(app_type.as_str())
}

/// Record what is now on disk as the baseline for future comparisons.
///
/// Reading the file back rather than remembering what we handed to the writer
/// keeps the baseline in the same shape a later comparison will see, including
/// every per-app transformation the writers apply on the way down.
pub(crate) fn record_snapshot(app_type: &AppType, provider_id: &str) {
    if !tracks_live_drift(app_type) {
        return;
    }

    let settings = match super::live::read_live_settings(*app_type) {
        Ok(settings) => settings,
        Err(err) => {
            log::debug!("skipped live snapshot for {}: {err}", app_type.as_str());
            return;
        }
    };

    let _guard = store_lock();
    let mut store = load_store();
    store.apps.insert(
        app_type.as_str().to_string(),
        LiveSnapshot {
            provider_id: provider_id.to_string(),
            settings,
        },
    );

    if let Err(err) = save_store(&store) {
        log::warn!(
            "failed to record live snapshot for {}: {err}",
            app_type.as_str()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merge_claude(base: Value, live: Value, incoming: Value) -> (Value, LiveDrift) {
        merge_user_edits(&AppType::Claude, &base, &live, &incoming)
    }

    #[test]
    fn hand_added_keys_survive_a_switch() {
        let base = json!({ "env": { "ANTHROPIC_BASE_URL": "https://a.example" } });
        let live = json!({
            "env": { "ANTHROPIC_BASE_URL": "https://a.example" },
            "hooks": { "PreToolUse": [{ "matcher": "Bash" }] }
        });
        let incoming = json!({ "env": { "ANTHROPIC_BASE_URL": "https://b.example" } });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert_eq!(merged["hooks"]["PreToolUse"][0]["matcher"], json!("Bash"));
        assert_eq!(
            merged["env"]["ANTHROPIC_BASE_URL"],
            json!("https://b.example")
        );
        assert_eq!(drift.preserved, vec!["hooks".to_string()]);
        assert!(!drift.has_conflicts());
    }

    #[test]
    fn provider_fields_win_over_a_hand_edit_of_the_same_key() {
        let base = json!({ "env": { "ANTHROPIC_BASE_URL": "https://a.example" } });
        let live = json!({ "env": { "ANTHROPIC_BASE_URL": "https://hand.example" } });
        let incoming = json!({ "env": { "ANTHROPIC_BASE_URL": "https://b.example" } });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert_eq!(
            merged["env"]["ANTHROPIC_BASE_URL"],
            json!("https://b.example")
        );
        assert_eq!(
            drift.conflicts,
            vec![DriftConflict {
                path: "env.ANTHROPIC_BASE_URL".to_string(),
                live: json!("https://hand.example"),
                incoming: json!("https://b.example"),
            }]
        );
        assert!(drift.preserved.is_empty());
    }

    #[test]
    fn a_hand_edit_the_new_provider_does_not_touch_is_kept() {
        // Both providers leave `includeCoAuthoredBy` alone, so the user owns it.
        let base = json!({ "includeCoAuthoredBy": true, "model": "a" });
        let live = json!({ "includeCoAuthoredBy": false, "model": "a" });
        let incoming = json!({ "includeCoAuthoredBy": true, "model": "b" });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert_eq!(merged["includeCoAuthoredBy"], json!(false));
        assert_eq!(merged["model"], json!("b"));
        assert_eq!(drift.preserved, vec!["includeCoAuthoredBy".to_string()]);
    }

    #[test]
    fn nested_edits_merge_key_by_key_instead_of_fighting_for_the_object() {
        let base = json!({ "env": { "ANTHROPIC_BASE_URL": "https://a.example" } });
        let live = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://a.example",
                "HTTPS_PROXY": "http://127.0.0.1:7890"
            }
        });
        let incoming = json!({ "env": { "ANTHROPIC_BASE_URL": "https://b.example" } });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert_eq!(merged["env"]["HTTPS_PROXY"], json!("http://127.0.0.1:7890"));
        assert_eq!(
            merged["env"]["ANTHROPIC_BASE_URL"],
            json!("https://b.example")
        );
        assert_eq!(drift.preserved, vec!["env.HTTPS_PROXY".to_string()]);
    }

    #[test]
    fn a_hand_deletion_stays_deleted_when_the_new_config_agrees() {
        let base = json!({ "model": "a", "includeCoAuthoredBy": true });
        let live = json!({ "model": "a" });
        let incoming = json!({ "model": "b", "includeCoAuthoredBy": true });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert!(merged.get("includeCoAuthoredBy").is_none());
        assert_eq!(drift.removed, vec!["includeCoAuthoredBy".to_string()]);
    }

    #[test]
    fn an_untouched_file_reports_no_drift() {
        let base = json!({ "env": { "ANTHROPIC_BASE_URL": "https://a.example" } });
        let incoming = json!({ "env": { "ANTHROPIC_BASE_URL": "https://b.example" } });

        let (merged, drift) = merge_claude(base.clone(), base, incoming.clone());

        assert_eq!(merged, incoming);
        assert!(drift.is_empty());
    }

    #[test]
    fn codex_auth_is_never_carried_onto_the_next_account() {
        // Codex refreshes its own OAuth token in auth.json. That belongs to the
        // account that was active, not to whatever provider comes next.
        let base = json!({
            "auth": { "tokens": { "access_token": "old" } },
            "config": "model = \"gpt-5\"\n"
        });
        let live = json!({
            "auth": { "tokens": { "access_token": "refreshed" } },
            "config": "model = \"gpt-5\"\n"
        });
        let incoming = json!({
            "auth": { "OPENAI_API_KEY": "sk-other" },
            "config": "model = \"gpt-5-codex\"\n"
        });

        let (merged, drift) = merge_user_edits(&AppType::Codex, &base, &live, &incoming);

        assert_eq!(merged["auth"], json!({ "OPENAI_API_KEY": "sk-other" }));
        assert!(drift.is_empty());
    }

    #[test]
    fn codex_config_text_is_reported_whole_because_toml_is_not_merged_yet() {
        let base = json!({ "auth": {}, "config": "model = \"gpt-5\"\n" });
        let live = json!({
            "auth": {},
            "config": "model = \"gpt-5\"\n\n[mcp_servers.local]\ncommand = \"x\"\n"
        });
        let incoming = json!({ "auth": {}, "config": "model = \"gpt-5-codex\"\n" });

        let (merged, drift) = merge_user_edits(&AppType::Codex, &base, &live, &incoming);

        assert_eq!(merged["config"], json!("model = \"gpt-5-codex\"\n"));
        assert_eq!(drift.conflicts.len(), 1);
        assert_eq!(drift.conflicts[0].path, "config");
    }

    #[test]
    fn a_subtree_the_new_provider_drops_keeps_only_the_users_half() {
        let base = json!({ "env": { "OWNED": "1" } });
        let live = json!({ "env": { "OWNED": "1", "MINE": "2" } });
        let incoming = json!({ "model": "b" });

        let (merged, drift) = merge_claude(base, live, incoming);

        assert_eq!(merged["env"], json!({ "MINE": "2" }));
        assert_eq!(drift.preserved, vec!["env.MINE".to_string()]);
    }
}
