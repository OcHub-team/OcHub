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

/// Keys whose value is an entire TOML document carried as one string.
///
/// Codex and Grok keep `config.toml` verbatim under `config`, so to a plain
/// object diff the whole file is a single scalar: any byte of external editing
/// collides with any byte we write, and the collision is unresolvable because
/// there is no smaller unit to keep or drop. Parsing them turns that one useless
/// conflict into the handful of keys that actually disagree.
fn toml_text_paths(app_type: &AppType) -> &'static [&'static str] {
    match app_type {
        AppType::Codex | AppType::GrokBuild => &["config"],
        _ => &[],
    }
}

/// Per-app rules the merge consults as it walks down a settings tree.
#[derive(Clone, Copy)]
struct MergeRules<'a> {
    protected: &'a [&'a str],
    toml_text: &'a [&'a str],
}

impl MergeRules<'_> {
    fn is_protected(&self, path: &str) -> bool {
        matches_path(path, self.protected)
    }

    fn is_toml_text(&self, path: &str) -> bool {
        matches_path(path, self.toml_text)
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

    let rules = MergeRules {
        protected: protected_paths(app_type),
        toml_text: toml_text_paths(app_type),
    };
    let merged = merge_objects(base_map, live_map, &incoming_map, "", rules, &mut drift);

    (Value::Object(merged), drift)
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn matches_path(path: &str, roots: &[&str]) -> bool {
    roots
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}.")))
}

fn merge_objects(
    base: &Map<String, Value>,
    live: &Map<String, Value>,
    incoming: &Map<String, Value>,
    prefix: &str,
    rules: MergeRules<'_>,
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
        if rules.is_protected(&path) {
            continue;
        }

        let base_value = base.get(key);
        let live_value = live.get(key);
        let incoming_value = incoming.get(key);

        // An embedded TOML document is merged as a document, not compared as a
        // string. Only the both-sides-present case needs it: with no ancestor or
        // nothing on disk there is no edit to isolate, and the rules below
        // already say the right thing.
        //
        // A non-string on our side would be a shape error rather than an edit,
        // so that case is left to the generic rules too.
        if rules.is_toml_text(&path)
            && let (Some(Value::String(base_text)), Some(Value::String(live_text))) =
                (base_value, live_value)
            && base_text != live_text
            && incoming_value.is_none_or(Value::is_string)
        {
            let incoming_text = incoming_value.and_then(Value::as_str);
            match merge_toml_documents(base_text, live_text, incoming_text, &path, drift) {
                Ok(merged) => {
                    out.insert(key.clone(), Value::String(merged));
                    continue;
                }
                // Unparsable TOML on either side: fall through and treat the
                // document as one opaque value. Coarse, but never wrong.
                Err(err) => {
                    log::warn!("live config at '{path}' is not valid TOML, diffing it whole: {err}")
                }
            }
        }

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
                    let merged =
                        merge_objects(base_child, live_child, &incoming_child, &path, rules, drift);

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
// Embedded TOML documents
// ---------------------------------------------------------------------------

/// Three-way merge of one embedded `config.toml`, reported per key.
///
/// The result is built on top of `incoming` so the outgoing document's own
/// comments and layout survive, and the keys carried over from `live` arrive as
/// the user wrote them — `toml_edit` keeps each item's formatting when it is
/// moved between documents.
///
/// `incoming` is `None` when the new configuration has no document of its own:
/// then nothing of ours can collide, and the file as edited stands.
fn merge_toml_documents(
    base: &str,
    live: &str,
    incoming: Option<&str>,
    prefix: &str,
    drift: &mut LiveDrift,
) -> Result<String, toml_edit::TomlError> {
    use toml_edit::DocumentMut;

    let base_doc: DocumentMut = base.parse()?;
    let live_doc: DocumentMut = live.parse()?;
    let Some(incoming) = incoming else {
        drift.preserved.push(prefix.to_string());
        return Ok(live.to_string());
    };
    let mut out_doc: DocumentMut = incoming.parse()?;

    merge_toml_tables(
        base_doc.as_table(),
        live_doc.as_table(),
        out_doc.as_table_mut(),
        prefix,
        drift,
    );

    Ok(out_doc.to_string())
}

/// The object merge's rules, applied to TOML tables instead of JSON maps.
///
/// `out` arrives as a copy of the incoming table and is edited in place, so
/// reading a key from it before touching it reads what the incoming
/// configuration says about that key.
///
/// Values are compared through their JSON projection: two spellings of the same
/// value (`a.b = 1` versus `[a] b = 1`, or a re-quoted string) are not an edit,
/// and reporting them as one is how a merge starts fighting the formatter.
fn merge_toml_tables(
    base: &dyn toml_edit::TableLike,
    live: &dyn toml_edit::TableLike,
    out: &mut dyn toml_edit::TableLike,
    prefix: &str,
    drift: &mut LiveDrift,
) {
    use toml_edit::{Item, Table};

    let mut keys: Vec<String> = base.iter().map(|(key, _)| key.to_string()).collect();
    for (key, _) in live.iter() {
        if base.get(key).is_none() {
            keys.push(key.to_string());
        }
    }

    // Carry a key over from the live document with its own key, so the comment
    // written above `notify = [...]` — which TOML hangs off the key, not off the
    // value — arrives with it.
    fn carry_over(out: &mut dyn toml_edit::TableLike, live: &dyn toml_edit::TableLike, key: &str) {
        let Some((live_key, live_item)) = live.get_key_value(key) else {
            return;
        };
        match out.entry_format(live_key) {
            toml_edit::Entry::Occupied(mut occupied) => {
                occupied.insert(live_item.clone());
            }
            toml_edit::Entry::Vacant(vacant) => {
                vacant.insert(live_item.clone());
            }
        }
    }

    for key in keys {
        let path = join_path(prefix, &key);
        let base_item = base.get(&key);
        let live_item = live.get(&key);

        // Everything the incoming document has to say about this key, read out
        // before `out` is borrowed mutably below.
        let incoming_json = out.get(&key).map(toml_item_to_json);
        let incoming_is_table = out.get(&key).is_some_and(|item| item.is_table_like());

        match (base_item, live_item) {
            // Added outside OcHub.
            (None, Some(live_item)) => match &incoming_json {
                None => {
                    carry_over(out, live, &key);
                    drift.preserved.push(path);
                }
                Some(incoming) if *incoming == toml_item_to_json(live_item) => {}
                Some(incoming) => drift.conflicts.push(DriftConflict {
                    path,
                    live: toml_item_to_json(live_item),
                    incoming: incoming.clone(),
                }),
            },

            // Deleted outside OcHub.
            (Some(base_item), None) => match &incoming_json {
                None => {}
                Some(incoming) if *incoming == toml_item_to_json(base_item) => {
                    out.remove(&key);
                    drift.removed.push(path);
                }
                Some(incoming) => drift.conflicts.push(DriftConflict {
                    path,
                    live: Value::Null,
                    incoming: incoming.clone(),
                }),
            },

            (Some(base_item), Some(live_item))
                if toml_item_to_json(base_item) != toml_item_to_json(live_item) =>
            {
                let nested = base_item
                    .as_table_like()
                    .zip(live_item.as_table_like())
                    .filter(|_| incoming_json.is_none() || incoming_is_table);

                if let Some((base_child, live_child)) = nested {
                    // Recurse so an edit inside `[model_providers.x]` does not
                    // have to fight the whole table for ownership.
                    if incoming_json.is_none() {
                        out.insert(&key, Item::Table(Table::new()));
                    }
                    let Some(out_child) = out.get_mut(&key).and_then(Item::as_table_like_mut)
                    else {
                        continue;
                    };
                    merge_toml_tables(base_child, live_child, out_child, &path, drift);

                    // An empty result means the table existed only to hold the
                    // outgoing provider's values; do not leave a husk behind.
                    if incoming_json.is_none()
                        && out.get(&key).is_some_and(|item| {
                            item.as_table_like().is_some_and(|table| table.is_empty())
                        })
                    {
                        out.remove(&key);
                    }
                    continue;
                }

                let base_json = toml_item_to_json(base_item);
                let live_json = toml_item_to_json(live_item);
                match &incoming_json {
                    // The new configuration says nothing here, or says exactly
                    // what the old one did — either way the user's edit stands.
                    None => {
                        carry_over(out, live, &key);
                        drift.preserved.push(path);
                    }
                    Some(incoming) if *incoming == base_json => {
                        carry_over(out, live, &key);
                        drift.preserved.push(path);
                    }
                    Some(incoming) if *incoming == live_json => {}
                    Some(incoming) => drift.conflicts.push(DriftConflict {
                        path,
                        live: live_json,
                        incoming: incoming.clone(),
                    }),
                }
            }

            // Untouched by the user: `incoming` already owns it.
            _ => {}
        }
    }
}

/// A TOML item as the JSON the drift report and the UI speak.
///
/// Dates have no JSON counterpart; their TOML spelling is what a user would
/// recognise anyway.
fn toml_item_to_json(item: &toml_edit::Item) -> Value {
    use toml_edit::Item;

    match item {
        Item::None => Value::Null,
        Item::Value(value) => toml_value_to_json(value),
        Item::Table(table) => toml_table_to_json(table),
        Item::ArrayOfTables(tables) => {
            Value::Array(tables.iter().map(toml_table_to_json).collect())
        }
    }
}

fn toml_value_to_json(value: &toml_edit::Value) -> Value {
    use toml_edit::Value as TomlValue;

    match value {
        TomlValue::String(text) => Value::String(text.value().clone()),
        TomlValue::Integer(number) => Value::from(*number.value()),
        TomlValue::Float(number) => serde_json::Number::from_f64(*number.value())
            .map(Value::Number)
            .unwrap_or(Value::Null),
        TomlValue::Boolean(flag) => Value::Bool(*flag.value()),
        TomlValue::Datetime(stamp) => Value::String(stamp.value().to_string()),
        TomlValue::Array(array) => Value::Array(array.iter().map(toml_value_to_json).collect()),
        TomlValue::InlineTable(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.to_string(), toml_value_to_json(value)))
                .collect(),
        ),
    }
}

fn toml_table_to_json(table: &toml_edit::Table) -> Value {
    Value::Object(
        table
            .iter()
            .map(|(key, item)| (key.to_string(), toml_item_to_json(item)))
            .collect(),
    )
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

/// Re-record the baseline after OcHub wrote the live config from somewhere
/// other than the provider flow.
///
/// MCP sync rewrites the very files a switch just wrote, and it runs *after* the
/// switch has taken its baseline. Without this the next switch reads OcHub's own
/// `[mcp_servers]` block as an external edit — and no answer to the prompt could
/// clear it, because resolving it wrote the file again and the sync re-applied
/// itself again right behind. The provider is carried over from the existing
/// baseline: a side write changes the file, never which provider is current.
pub(crate) fn rebaseline_after_side_write(app_type: &AppType) {
    if !tracks_live_drift(app_type) {
        return;
    }
    // No baseline yet means nothing to invalidate: the first switch records one.
    let Some(snapshot) = load_snapshot(app_type) else {
        return;
    };
    record_snapshot(app_type, &snapshot.provider_id);
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

    fn merge_codex_config(base: &str, live: &str, incoming: &str) -> (String, LiveDrift) {
        let (merged, drift) = merge_user_edits(
            &AppType::Codex,
            &json!({ "auth": {}, "config": base }),
            &json!({ "auth": {}, "config": live }),
            &json!({ "auth": {}, "config": incoming }),
        );
        (
            merged["config"]
                .as_str()
                .expect("config stays a TOML string")
                .to_string(),
            drift,
        )
    }

    #[test]
    fn a_hand_added_codex_table_survives_a_switch() {
        // The whole of config.toml arrives as one string. Comparing it as one
        // string is what used to make every switch a whole-file conflict.
        let (merged, drift) = merge_codex_config(
            "model = \"gpt-5\"\n",
            "model = \"gpt-5\"\n\n[mcp_servers.local]\ncommand = \"x\"\n",
            "model = \"gpt-5-codex\"\n",
        );

        assert!(merged.contains("model = \"gpt-5-codex\""));
        assert!(merged.contains("[mcp_servers.local]"));
        assert!(merged.contains("command = \"x\""));
        assert_eq!(drift.preserved, vec!["config.mcp_servers".to_string()]);
        assert!(!drift.has_conflicts());
    }

    #[test]
    fn a_codex_conflict_names_the_key_rather_than_the_file() {
        let (merged, drift) = merge_codex_config(
            "model_provider = \"a\"\napproval_policy = \"never\"\n",
            "model_provider = \"local-gateway\"\napproval_policy = \"on-request\"\n",
            "model_provider = \"b\"\napproval_policy = \"never\"\n",
        );

        // Only the key both sides set is a conflict; the other edit is kept.
        assert_eq!(drift.conflicts.len(), 1);
        assert_eq!(drift.conflicts[0].path, "config.model_provider");
        assert_eq!(drift.conflicts[0].live, json!("local-gateway"));
        assert_eq!(drift.conflicts[0].incoming, json!("b"));
        assert!(merged.contains("model_provider = \"b\""));
        assert!(merged.contains("approval_policy = \"on-request\""));
        assert_eq!(drift.preserved, vec!["config.approval_policy".to_string()]);
    }

    #[test]
    fn codex_comments_and_hand_written_layout_survive_the_merge() {
        let (merged, _) = merge_codex_config(
            "model = \"gpt-5\"\n",
            "model = \"gpt-5\"\n\n# mine, hands off\nnotify = [\"script\"]\n",
            "# provider header\nmodel = \"gpt-5-codex\"\n",
        );

        assert!(merged.contains("# provider header"));
        assert!(merged.contains("# mine, hands off"));
        assert!(merged.contains("notify = [\"script\"]"));
    }

    #[test]
    fn a_reformatted_codex_file_is_not_an_edit() {
        // Same values, different spelling: an inline table versus a section.
        let (_, drift) = merge_codex_config(
            "[model_providers.a]\nbase_url = \"https://a.example\"\n",
            "model_providers = { a = { base_url = \"https://a.example\" } }\n",
            "[model_providers.a]\nbase_url = \"https://a.example\"\n",
        );

        assert!(drift.is_empty(), "unexpected drift: {drift:?}");
    }

    #[test]
    fn a_hand_deleted_codex_key_stays_deleted() {
        let (merged, drift) = merge_codex_config(
            "model = \"gpt-5\"\nsandbox_mode = \"read-only\"\n",
            "model = \"gpt-5\"\n",
            "model = \"gpt-5-codex\"\nsandbox_mode = \"read-only\"\n",
        );

        assert!(!merged.contains("sandbox_mode"));
        assert_eq!(drift.removed, vec!["config.sandbox_mode".to_string()]);
    }

    #[test]
    fn unparsable_codex_toml_falls_back_to_the_whole_document() {
        let (merged, drift) = merge_codex_config(
            "model = \"gpt-5\"\n",
            "model = \"gpt-5\"\nthis is not toml\n",
            "model = \"gpt-5-codex\"\n",
        );

        assert_eq!(merged, "model = \"gpt-5-codex\"\n");
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
