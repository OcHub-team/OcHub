use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::application::{Application, ApplicationError, ApplicationResult, ProviderSwitchPolicy};
use crate::db::{McpApps, McpServer};
use crate::plugin::AppMode;
use crate::provider_config::Severity;
use crate::{AppId, AppType, Provider};

const MAX_DECLARATIVE_BYTES: u64 = 1024 * 1024;
const OWNERSHIP_KEY: &str = "cli_managed_resources_v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclarativeDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: DeclarativeMetadata,
    #[serde(default)]
    pub spec: DeclarativeSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclarativeMetadata {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclarativeSpec {
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
    #[serde(default)]
    pub apps: Vec<DesiredApp>,
    #[serde(default)]
    pub providers: Vec<DesiredProvider>,
    #[serde(default)]
    pub mcp_servers: Vec<DesiredMcpServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredApp {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredProvider {
    pub id: String,
    pub app: String,
    #[serde(default = "present")]
    pub state: String,
    #[serde(default)]
    pub config: Map<String, Value>,
    #[serde(default)]
    pub live: Option<DesiredLiveState>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredLiveState {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub on_drift: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredMcpServer {
    pub id: String,
    #[serde(default = "present")]
    pub state: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub spec: Value,
    #[serde(default)]
    pub apps: BTreeMap<String, String>,
}

fn present() -> String {
    "present".to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclarativeAction {
    pub action: String,
    pub resource_kind: String,
    pub resource_id: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclarativePlan {
    pub api_version: String,
    pub manager: String,
    pub source_path: PathBuf,
    pub file_hash: String,
    pub actions: Vec<DeclarativeAction>,
    pub conflicts: Vec<Value>,
    pub summary: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnershipStore {
    #[serde(default)]
    resources: BTreeMap<String, OwnershipRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnershipRecord {
    manager: String,
    source_path: PathBuf,
    last_applied_hash: String,
    last_applied_at: i64,
}

struct LoadedDocument {
    path: PathBuf,
    hash: String,
    document: DeclarativeDocument,
}

impl Application {
    pub fn validate_declarative_file(&self, path: &Path) -> ApplicationResult<Value> {
        let loaded = load_document(path)?;
        self.validate_document(&loaded.document)?;
        Ok(json!({
            "valid": true,
            "apiVersion": loaded.document.api_version,
            "kind": loaded.document.kind,
            "name": loaded.document.metadata.name,
            "path": loaded.path,
            "fileHash": loaded.hash
        }))
    }

    pub fn plan_declarative_file(
        &self,
        path: &Path,
        adopt: bool,
        prune: bool,
    ) -> ApplicationResult<DeclarativePlan> {
        let loaded = load_document(path)?;
        self.validate_document(&loaded.document)?;
        let manager = format!("cli:file:{}", loaded.path.display());
        let ownership = self.load_ownership()?;
        let mut actions = Vec::new();
        let mut conflicts = Vec::new();
        let mut declared = BTreeSet::new();

        for desired in &loaded.document.spec.apps {
            let id = AppId::parse(&desired.id)?;
            let current = self.get_app(&id)?;
            let key = resource_key("app", id.as_str());
            declared.insert(key.clone());
            let action = if current.enabled == desired.enabled {
                "noop"
            } else {
                "update"
            };
            push_action(
                &mut actions,
                action,
                "app",
                id.as_str(),
                json!({ "enabled": current.enabled }),
                json!({ "enabled": desired.enabled }),
            );
            ownership_conflict(&ownership, &key, &manager, action, adopt, &mut conflicts);
        }

        for (path, desired) in &loaded.document.spec.settings {
            let current = self.get_setting(path, false).unwrap_or(Value::Null);
            let key = resource_key("setting", path);
            declared.insert(key.clone());
            let action = if current == *desired {
                "noop"
            } else {
                "update"
            };
            push_action(
                &mut actions,
                action,
                "setting",
                path,
                current,
                desired.clone(),
            );
            ownership_conflict(&ownership, &key, &manager, action, adopt, &mut conflicts);
        }

        for desired in &loaded.document.spec.providers {
            let app = AppId::parse(&desired.app)?;
            self.get_app(&app)?;
            let id = format!("{}:{}", app, desired.id);
            let key = resource_key("provider", &id);
            declared.insert(key.clone());
            let current = self
                .state()
                .db
                .get_provider_by_id(&desired.id, app.as_str())?;
            let (action, after) = if desired.state == "absent" {
                (
                    if current.is_some() { "delete" } else { "noop" },
                    Value::Null,
                )
            } else {
                let provider = self.build_desired_provider(desired, current.as_ref(), true)?;
                let after = serde_json::to_value(&provider)
                    .map_err(|source| crate::AppError::JsonSerialize { source })?;
                let action = match current.as_ref() {
                    None => "create",
                    Some(current)
                        if serde_json::to_value(current)
                            .map_err(|source| crate::AppError::JsonSerialize { source })?
                            == after =>
                    {
                        "noop"
                    }
                    Some(_) => "update",
                };
                (action, crate::application::redact_json(&after))
            };
            let before = current
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|source| crate::AppError::JsonSerialize { source })?
                .map(|value| crate::application::redact_json(&value))
                .unwrap_or(Value::Null);
            push_action(&mut actions, action, "provider", &id, before, after);
            ownership_conflict(&ownership, &key, &manager, action, adopt, &mut conflicts);
        }

        let current_mcp = self.state().db.get_all_mcp_servers()?;
        for desired in &loaded.document.spec.mcp_servers {
            let key = resource_key("mcp", &desired.id);
            declared.insert(key.clone());
            let current = current_mcp.get(&desired.id);
            let (action, after) = if desired.state == "absent" {
                (
                    if current.is_some() { "delete" } else { "noop" },
                    Value::Null,
                )
            } else {
                let server = desired_mcp_server(desired)?;
                let after = serde_json::to_value(&server)
                    .map_err(|source| crate::AppError::JsonSerialize { source })?;
                let action = match current {
                    None => "create",
                    Some(current)
                        if serde_json::to_value(current)
                            .map_err(|source| crate::AppError::JsonSerialize { source })?
                            == after =>
                    {
                        "noop"
                    }
                    Some(_) => "update",
                };
                (action, crate::application::redact_json(&after))
            };
            let before = current
                .map(serde_json::to_value)
                .transpose()
                .map_err(|source| crate::AppError::JsonSerialize { source })?
                .map(|value| crate::application::redact_json(&value))
                .unwrap_or(Value::Null);
            push_action(&mut actions, action, "mcp", &desired.id, before, after);
            ownership_conflict(&ownership, &key, &manager, action, adopt, &mut conflicts);
        }

        if prune {
            for (key, record) in &ownership.resources {
                if record.manager == manager && !declared.contains(key) && prunable_resource(key) {
                    let Some((kind, id)) = key.split_once(':') else {
                        continue;
                    };
                    push_action(
                        &mut actions,
                        "delete",
                        kind,
                        id,
                        json!({ "managed": true }),
                        Value::Null,
                    );
                }
            }
        }

        let mut summary = BTreeMap::new();
        for action in &actions {
            *summary.entry(action.action.clone()).or_insert(0) += 1;
        }
        Ok(DeclarativePlan {
            api_version: loaded.document.api_version,
            manager,
            source_path: loaded.path,
            file_hash: loaded.hash,
            actions,
            conflicts,
            summary,
        })
    }

    pub async fn apply_declarative_file(
        &self,
        path: &Path,
        adopt: bool,
        prune: bool,
    ) -> ApplicationResult<Value> {
        let loaded = load_document(path)?;
        let plan = self.plan_declarative_file(&loaded.path, adopt, prune)?;
        if plan.file_hash != loaded.hash {
            return Err(ApplicationError::ResourceConflict(
                "declarative file changed while its plan was being built".to_string(),
            ));
        }
        if !plan.conflicts.is_empty() {
            return Err(ApplicationError::ResourceConflict(format!(
                "{} managed resource conflict(s); use --adopt to take ownership",
                plan.conflicts.len()
            )));
        }
        let manager = plan.manager.clone();
        let now = chrono::Utc::now().timestamp();
        let mut ownership = self.load_ownership()?;

        for (path, value) in &loaded.document.spec.settings {
            self.set_setting(path, resolve_secret_refs(value, true)?)?;
            own(
                &mut ownership,
                resource_key("setting", path),
                &manager,
                &loaded,
                now,
            );
        }
        for desired in &loaded.document.spec.providers {
            let app = AppId::parse(&desired.app)?;
            let key = resource_key("provider", &format!("{}:{}", app, desired.id));
            let current = self
                .state()
                .db
                .get_provider_by_id(&desired.id, app.as_str())?;
            if desired.state == "absent" {
                if current.is_some() {
                    self.delete_provider(&app, &desired.id)?;
                }
                ownership.resources.remove(&key);
                continue;
            }
            let provider = self.build_desired_provider(desired, current.as_ref(), true)?;
            self.state().db.save_provider(app.as_str(), &provider)?;
            if desired.live.as_ref().and_then(|live| live.state.as_deref()) == Some("active") {
                let policy = match desired
                    .live
                    .as_ref()
                    .and_then(|live| live.on_drift.as_deref())
                    .unwrap_or("abort")
                {
                    "preserve" => ProviderSwitchPolicy::Preserve,
                    "discard" => ProviderSwitchPolicy::Discard,
                    _ => ProviderSwitchPolicy::Abort,
                };
                let plugin =
                    crate::plugin::get_plugin(&app).ok_or_else(|| ApplicationError::NotFound {
                        kind: "app",
                        id: app.to_string(),
                    })?;
                match (plugin.mode(), AppType::from_app_id(&app)) {
                    (AppMode::Switch, Some(_)) => {
                        self.switch_provider(&app, &desired.id, policy)?;
                    }
                    (AppMode::Switch, None) | (AppMode::Additive, _) => {
                        plugin
                            .live()
                            .write_live(self.state().db.as_ref(), &provider)?;
                    }
                }
            }
            own(&mut ownership, key, &manager, &loaded, now);
        }
        for desired in &loaded.document.spec.mcp_servers {
            let key = resource_key("mcp", &desired.id);
            if desired.state == "absent" {
                if self
                    .state()
                    .db
                    .get_all_mcp_servers()?
                    .contains_key(&desired.id)
                {
                    self.delete_mcp_server(&desired.id)?;
                }
                ownership.resources.remove(&key);
            } else {
                self.upsert_mcp_server(desired_mcp_server(desired)?)?;
                own(&mut ownership, key, &manager, &loaded, now);
            }
        }
        for desired in &loaded.document.spec.apps {
            self.set_app_enabled(&AppId::parse(&desired.id)?, desired.enabled)
                .await?;
            own(
                &mut ownership,
                resource_key("app", &desired.id),
                &manager,
                &loaded,
                now,
            );
        }

        if prune {
            let declared = declared_keys(&loaded.document)?;
            let stale = ownership
                .resources
                .iter()
                .filter(|(key, record)| {
                    record.manager == manager && !declared.contains(*key) && prunable_resource(key)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in stale {
                self.prune_resource(&key).await?;
                ownership.resources.remove(&key);
            }
        }
        self.save_ownership(&ownership)?;
        Ok(json!({
            "applied": true,
            "manager": manager,
            "fileHash": loaded.hash,
            "summary": plan.summary
        }))
    }

    fn validate_document(&self, document: &DeclarativeDocument) -> ApplicationResult<()> {
        if document.api_version != "ochub.io/v1alpha1" {
            return Err(ApplicationError::InvalidInput(format!(
                "unsupported apiVersion {}; expected ochub.io/v1alpha1",
                document.api_version
            )));
        }
        if document.kind != "OcHubConfig" {
            return Err(ApplicationError::InvalidInput(format!(
                "unsupported kind {}; expected OcHubConfig",
                document.kind
            )));
        }
        if document.metadata.name.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "metadata.name cannot be empty".to_string(),
            ));
        }
        let mut unique = BTreeSet::new();
        for app in &document.spec.apps {
            let id = AppId::parse(&app.id)?;
            self.get_app(&id)?;
            ensure_unique(&mut unique, resource_key("app", id.as_str()))?;
        }
        for path in document.spec.settings.keys() {
            self.validate_setting(path, document.spec.settings[path].clone())?;
            ensure_unique(&mut unique, resource_key("setting", path))?;
        }
        for provider in &document.spec.providers {
            validate_state(&provider.state)?;
            let app = AppId::parse(&provider.app)?;
            self.get_app(&app)?;
            ensure_unique(
                &mut unique,
                resource_key("provider", &format!("{}:{}", app, provider.id)),
            )?;
            if provider.state != "absent" {
                self.build_desired_provider(provider, None, true)?;
            }
        }
        for server in &document.spec.mcp_servers {
            validate_state(&server.state)?;
            ensure_unique(&mut unique, resource_key("mcp", &server.id))?;
            if server.state != "absent" {
                self.validate_mcp_server(&desired_mcp_server(server)?)?;
            }
        }
        Ok(())
    }

    fn build_desired_provider(
        &self,
        desired: &DesiredProvider,
        current: Option<&Provider>,
        resolve_secrets: bool,
    ) -> ApplicationResult<Provider> {
        if desired.id.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "provider id cannot be empty".to_string(),
            ));
        }
        let app = AppId::parse(&desired.app)?;
        let plugin = crate::plugin::get_plugin(&app).ok_or_else(|| ApplicationError::NotFound {
            kind: "app",
            id: app.to_string(),
        })?;
        let codec =
            plugin
                .provider_config()
                .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                    app: app.to_string(),
                    capability: "provider.write",
                })?;
        let resolved =
            resolve_secret_refs(&Value::Object(desired.config.clone()), resolve_secrets)?;
        let mut config = resolved.as_object().cloned().unwrap_or_default();
        let name = config
            .remove("name")
            .and_then(|value| value.as_str().map(ToString::to_string))
            .unwrap_or_else(|| desired.id.clone());
        let website_url = config
            .remove("websiteUrl")
            .or_else(|| config.remove("homepage"))
            .and_then(|value| value.as_str().map(ToString::to_string))
            .or_else(|| current.and_then(|provider| provider.website_url.clone()));
        let category = config
            .remove("category")
            .and_then(|value| value.as_str().map(ToString::to_string))
            .or_else(|| current.and_then(|provider| provider.category.clone()));
        let notes = config
            .remove("notes")
            .and_then(|value| value.as_str().map(ToString::to_string))
            .or_else(|| current.and_then(|provider| provider.notes.clone()));
        let prior = current
            .map(|provider| &provider.settings_config)
            .unwrap_or(&Value::Null);
        let encoded = if let Some(settings) = config.remove("settingsConfig") {
            crate::provider_config::EncodeResult {
                settings_config: settings,
                meta: current.and_then(|provider| provider.meta.clone()),
            }
        } else {
            let issues = codec.validate_for_category(&config, category.as_deref());
            let errors = issues
                .into_iter()
                .filter(|issue| issue.severity == Severity::Error)
                .map(|issue| {
                    issue
                        .field
                        .map(|field| format!("{field}: {}", issue.message))
                        .unwrap_or(issue.message)
                })
                .collect::<Vec<_>>();
            if !errors.is_empty() {
                return Err(ApplicationError::ValidationFailed {
                    message: format!("provider {} is invalid", desired.id),
                    details: json!({ "issues": errors }),
                });
            }
            codec.encode(
                &config,
                prior,
                current.and_then(|provider| provider.meta.as_ref()),
            )
        };
        Ok(Provider {
            id: desired.id.clone(),
            name,
            settings_config: encoded.settings_config,
            website_url,
            category,
            created_at: current
                .and_then(|provider| provider.created_at)
                .or_else(|| Some(chrono::Utc::now().timestamp())),
            sort_index: current.and_then(|provider| provider.sort_index),
            notes,
            meta: encoded.meta,
            icon: current.and_then(|provider| provider.icon.clone()),
            icon_color: current.and_then(|provider| provider.icon_color.clone()),
        })
    }

    fn load_ownership(&self) -> ApplicationResult<OwnershipStore> {
        self.state()
            .db
            .get_setting(OWNERSHIP_KEY)?
            .map(|raw| {
                serde_json::from_str(&raw).map_err(|error| {
                    ApplicationError::OperationFailed(format!(
                        "managed-resource metadata is invalid: {error}"
                    ))
                })
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn save_ownership(&self, ownership: &OwnershipStore) -> ApplicationResult<()> {
        let raw = serde_json::to_string(ownership)
            .map_err(|source| crate::AppError::JsonSerialize { source })?;
        self.state().db.set_setting(OWNERSHIP_KEY, &raw)?;
        Ok(())
    }

    async fn prune_resource(&self, key: &str) -> ApplicationResult<()> {
        let Some((kind, id)) = key.split_once(':') else {
            return Ok(());
        };
        match kind {
            "provider" => {
                let (app, provider) = id.split_once(':').ok_or_else(|| {
                    ApplicationError::InvalidInput(format!("invalid provider ownership key: {key}"))
                })?;
                if self.state().db.get_provider_by_id(provider, app)?.is_some() {
                    self.delete_provider(&AppId::parse(app)?, provider)?;
                }
            }
            "mcp" if self.state().db.get_all_mcp_servers()?.contains_key(id) => {
                self.delete_mcp_server(id)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn load_document(path: &Path) -> ApplicationResult<LoadedDocument> {
    let metadata = fs::metadata(path).map_err(|error| crate::AppError::io(path, error))?;
    if !metadata.is_file() {
        return Err(ApplicationError::InvalidInput(format!(
            "declarative config is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_DECLARATIVE_BYTES {
        return Err(ApplicationError::InvalidInput(format!(
            "declarative config exceeds {MAX_DECLARATIVE_BYTES} bytes"
        )));
    }
    let path = path
        .canonicalize()
        .map_err(|error| crate::AppError::io(path, error))?;
    let bytes = fs::read(&path).map_err(|error| crate::AppError::io(&path, error))?;
    let document = serde_yaml::from_slice(&bytes).map_err(|error| {
        ApplicationError::InvalidInput(format!(
            "invalid declarative config {}: {error}",
            path.display()
        ))
    })?;
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(LoadedDocument {
        path,
        hash,
        document,
    })
}

fn desired_mcp_server(desired: &DesiredMcpServer) -> ApplicationResult<McpServer> {
    if desired.id.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "MCP server id cannot be empty".to_string(),
        ));
    }
    let enabled = |app: &str| {
        desired
            .apps
            .get(app)
            .is_some_and(|state| state == "enabled")
    };
    Ok(McpServer {
        id: desired.id.clone(),
        name: desired.name.clone().unwrap_or_else(|| desired.id.clone()),
        server: desired.spec.clone(),
        apps: McpApps {
            claude: enabled("claude"),
            codex: enabled("codex"),
            gemini: false,
            grokbuild: enabled("grokbuild"),
            opencode: enabled("opencode"),
            hermes: enabled("hermes"),
        },
        description: None,
        homepage: None,
        docs: None,
        tags: vec!["declarative".to_string()],
    })
}

fn resolve_secret_refs(value: &Value, materialize: bool) -> ApplicationResult<Value> {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("fromEnv") => {
            let name = object["fromEnv"].as_str().ok_or_else(|| {
                ApplicationError::InvalidInput("fromEnv must be a string".to_string())
            })?;
            let resolved = std::env::var(name).map_err(|_| {
                ApplicationError::InvalidInput(format!(
                    "referenced secret environment variable is not set: {name}"
                ))
            })?;
            Ok(Value::String(if materialize {
                resolved
            } else {
                "******".to_string()
            }))
        }
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve_secret_refs(value, materialize)?)))
            .collect::<ApplicationResult<Map<_, _>>>()
            .map(Value::Object),
        Value::Array(items) => items
            .iter()
            .map(|item| resolve_secret_refs(item, materialize))
            .collect::<ApplicationResult<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

fn validate_state(state: &str) -> ApplicationResult<()> {
    if matches!(state, "present" | "absent") {
        Ok(())
    } else {
        Err(ApplicationError::InvalidInput(format!(
            "resource state must be present or absent, got {state}"
        )))
    }
}

fn ensure_unique(unique: &mut BTreeSet<String>, key: String) -> ApplicationResult<()> {
    if unique.insert(key.clone()) {
        Ok(())
    } else {
        Err(ApplicationError::InvalidInput(format!(
            "duplicate declarative resource: {key}"
        )))
    }
}

fn resource_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

fn prunable_resource(key: &str) -> bool {
    key.starts_with("provider:") || key.starts_with("mcp:")
}

fn push_action(
    actions: &mut Vec<DeclarativeAction>,
    action: &str,
    kind: &str,
    id: &str,
    before: Value,
    after: Value,
) {
    actions.push(DeclarativeAction {
        action: action.to_string(),
        resource_kind: kind.to_string(),
        resource_id: id.to_string(),
        before,
        after,
    });
}

fn ownership_conflict(
    ownership: &OwnershipStore,
    key: &str,
    manager: &str,
    action: &str,
    adopt: bool,
    conflicts: &mut Vec<Value>,
) {
    if action == "noop" || adopt {
        return;
    }
    if let Some(record) = ownership.resources.get(key)
        && record.manager != manager
    {
        conflicts.push(json!({
            "resource": key,
            "owner": record.manager,
            "requestedManager": manager
        }));
    }
}

fn own(
    ownership: &mut OwnershipStore,
    key: String,
    manager: &str,
    loaded: &LoadedDocument,
    now: i64,
) {
    ownership.resources.insert(
        key,
        OwnershipRecord {
            manager: manager.to_string(),
            source_path: loaded.path.clone(),
            last_applied_hash: loaded.hash.clone(),
            last_applied_at: now,
        },
    );
}

fn declared_keys(document: &DeclarativeDocument) -> ApplicationResult<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    keys.extend(
        document
            .spec
            .apps
            .iter()
            .map(|app| resource_key("app", &app.id)),
    );
    keys.extend(
        document
            .spec
            .settings
            .keys()
            .map(|path| resource_key("setting", path)),
    );
    for provider in &document.spec.providers {
        let app = AppId::parse(&provider.app)?;
        keys.insert(resource_key(
            "provider",
            &format!("{}:{}", app, provider.id),
        ));
    }
    keys.extend(
        document
            .spec
            .mcp_servers
            .iter()
            .map(|server| resource_key("mcp", &server.id)),
    );
    Ok(keys)
}
