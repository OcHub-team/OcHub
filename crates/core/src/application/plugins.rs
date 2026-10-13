use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::application::{
    Application, ApplicationError, ApplicationResult, PluginDetails, PluginSummary,
};
use crate::plugin::{AppManifest, ManifestPlugin, ManifestSource};
use crate::{AppError, AppId};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

impl Application {
    pub fn list_user_plugins(&self) -> ApplicationResult<Vec<PluginSummary>> {
        let mut plugins = crate::plugin::all_plugins()
            .into_iter()
            .filter(|plugin| plugin.is_user_manifest())
            .map(|plugin| PluginSummary {
                id: plugin.id().to_string(),
                name: plugin.display_name().to_string(),
                mode: match plugin.mode() {
                    crate::plugin::AppMode::Switch => "switch",
                    crate::plugin::AppMode::Additive => "additive",
                }
                .to_string(),
                enabled: crate::plugin::is_app_enabled(plugin.as_ref()),
                path: plugin
                    .manifest_path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        plugins.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(plugins)
    }

    pub fn get_user_plugin(&self, id: &AppId) -> ApplicationResult<PluginDetails> {
        let plugin = self
            .list_user_plugins()?
            .into_iter()
            .find(|plugin| plugin.id == id.as_str())
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "plugin",
                id: id.to_string(),
            })?;
        let manifest = fs::read_to_string(&plugin.path)
            .map_err(|error| crate::AppError::io(Path::new(&plugin.path), error))?;
        Ok(PluginDetails { plugin, manifest })
    }

    pub fn validate_plugin_manifest(&self, path: &Path) -> ApplicationResult<Value> {
        let (manifest, _) = read_and_validate_manifest(path)?;
        Ok(json!({
            "valid": true,
            "id": manifest.app.id,
            "name": manifest.app.name,
            "mode": manifest.app.mode,
            "files": manifest.files.len(),
            "fields": manifest.fields().count(),
            "hooks": {
                "liveValidate": manifest.hooks.live_validate,
                "postWrite": manifest.hooks.post_write,
                "decode": manifest.hooks.decode,
            }
        }))
    }

    pub fn install_plugin_manifest(&self, source: &Path) -> ApplicationResult<PluginDetails> {
        let (manifest, bytes) = read_and_validate_manifest(source)?;
        let id = manifest.app_id().map_err(manifest_error)?;
        if crate::plugin::get_plugin(&id).is_some() {
            return Err(ApplicationError::AlreadyExists {
                kind: "app",
                id: id.to_string(),
            });
        }
        let directory = crate::plugin::user_plugins_dir();
        fs::create_dir_all(&directory).map_err(|error| crate::AppError::io(&directory, error))?;
        let destination = directory.join(format!("{id}.toml"));
        if destination.exists() {
            return Err(ApplicationError::AlreadyExists {
                kind: "plugin-manifest",
                id: destination.to_string_lossy().into_owned(),
            });
        }
        crate::paths::atomic_write(&destination, &bytes)?;
        if let Err(error) = set_private_permissions(&destination) {
            let rollback_error = fs::remove_file(&destination)
                .err()
                .map(|error| error.to_string());
            return Err(ApplicationError::ValidationFailed {
                message: format!("Plugin was not activated: {error}"),
                details: json!({"path": destination, "rollbackError": rollback_error}),
            });
        }
        let errors = crate::plugin::reload_user_plugins();
        if !errors.is_empty() {
            // Reload preserves the old registry on any invalid candidate,
            // including a different manifest. Do not leave a new on-disk file
            // behind and report it as an installed, active adapter.
            let rollback_error = fs::remove_file(&destination)
                .err()
                .map(|error| error.to_string());
            return Err(ApplicationError::ValidationFailed {
                message: "Plugin was not activated; the previous plugin registry was preserved"
                    .into(),
                details: json!({ "path": destination, "reloadErrors": errors, "rollbackError": rollback_error }),
            });
        }
        self.get_user_plugin(&id)
    }

    pub fn reload_plugins(&self) -> ApplicationResult<Value> {
        let errors = crate::plugin::reload_user_plugins();
        Ok(json!({
            "plugins": self.list_user_plugins()?,
            "errors": errors
        }))
    }

    pub fn plugin_errors(&self) -> Vec<crate::plugin::ManifestLoadError> {
        crate::plugin::manifest_load_errors()
    }

    pub fn remove_plugin_manifest(&self, id: &AppId, purge_data: bool) -> ApplicationResult<Value> {
        let details = self.get_user_plugin(id)?;
        let path = PathBuf::from(&details.plugin.path);
        let bytes = fs::read(&path).map_err(|error| crate::AppError::io(&path, error))?;
        let permissions = fs::metadata(&path)
            .map_err(|error| crate::AppError::io(&path, error))?
            .permissions();
        let previous = crate::plugin::all_plugins()
            .into_iter()
            .filter(|plugin| plugin.is_user_manifest())
            .collect::<Vec<_>>();
        fs::remove_file(&path).map_err(|error| crate::AppError::io(&path, error))?;
        let errors = crate::plugin::reload_user_plugins();
        if !errors.is_empty() {
            let rollback_errors = restore_removed_manifest(&path, &bytes, permissions, previous);
            return Err(ApplicationError::ValidationFailed {
                message: "Plugin was not removed; the previous registry was preserved".into(),
                details: json!({"path": path, "reloadErrors": errors, "recoveryRequired": !rollback_errors.is_empty(), "rollbackErrors": rollback_errors}),
            });
        }
        if purge_data && let Err(error) = self.purge_removed_plugin_data(id) {
            let rollback_errors = restore_removed_manifest(&path, &bytes, permissions, previous);
            let settings_recovery_error = match &error {
                ApplicationError::ValidationFailed { details, .. } => details
                    .get("settingsRecoveryError")
                    .cloned()
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            };
            return Err(ApplicationError::ValidationFailed {
                message: format!(
                    "Plugin removal was rolled back because data cleanup failed: {error}"
                ),
                details: json!({"path": path, "recoveryRequired": !rollback_errors.is_empty() || !settings_recovery_error.is_null(), "rollbackErrors": rollback_errors, "settingsRecoveryError": settings_recovery_error}),
            });
        }
        Ok(json!({
            "removed": true,
            "id": id,
            "path": path,
            "purgedData": purge_data,
            "reloadErrors": errors
        }))
    }

    fn purge_removed_plugin_data(&self, id: &AppId) -> ApplicationResult<()> {
        let previous = crate::settings::get_settings();
        let enabled = previous
            .enabled_apps
            .as_ref()
            .and_then(|values| values.get(id.as_str()))
            .copied();
        let directory = previous
            .app_config_dirs
            .as_ref()
            .and_then(|values| values.get(id.as_str()))
            .cloned();
        let mut connection = crate::db::lock_conn!(self.state.db.conn);
        let transaction = connection
            .transaction()
            .map_err(|error| crate::AppError::Database(error.to_string()))?;
        transaction
            .execute("DELETE FROM providers WHERE app_type = ?1", [id.as_str()])
            .map_err(|error| crate::AppError::Database(error.to_string()))?;
        transaction
            .execute("DELETE FROM editor_drafts WHERE app_id = ?1", [id.as_str()])
            .map_err(|error| crate::AppError::Database(error.to_string()))?;
        transaction
            .execute(
                "DELETE FROM config_apply_heads WHERE app_id = ?1",
                [id.as_str()],
            )
            .map_err(|error| crate::AppError::Database(error.to_string()))?;
        crate::settings::mutate_settings(|settings| {
            if let Some(values) = settings.enabled_apps.as_mut() {
                values.remove(id.as_str());
            }
            if let Some(values) = settings.app_config_dirs.as_mut() {
                values.remove(id.as_str());
            }
        })?;
        if let Err(error) = transaction.commit() {
            let restore = crate::settings::mutate_settings(|settings| {
                if let Some(value) = enabled {
                    settings
                        .enabled_apps
                        .get_or_insert_with(Default::default)
                        .insert(id.to_string(), value);
                }
                if let Some(value) = directory {
                    settings
                        .app_config_dirs
                        .get_or_insert_with(Default::default)
                        .insert(id.to_string(), value);
                }
            });
            return Err(ApplicationError::ValidationFailed {
                message: format!("Plugin data transaction failed: {error}"),
                details: json!({"settingsRecoveryError": restore.err().map(|error| error.to_string())}),
            });
        }
        Ok(())
    }
}

fn restore_removed_manifest(
    path: &Path,
    bytes: &[u8],
    permissions: fs::Permissions,
    previous: Vec<Arc<dyn crate::plugin::AppPlugin>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    match crate::paths::atomic_write(path, bytes) {
        Ok(()) => {
            if let Err(error) = fs::set_permissions(path, permissions) {
                errors.push(error.to_string());
            }
        }
        Err(error) => errors.push(error.to_string()),
    }
    if let Err(error) = crate::plugin::registry::replace_user_plugins(previous) {
        errors.push(error.to_string());
    }
    errors
}

fn read_and_validate_manifest(path: &Path) -> ApplicationResult<(AppManifest, Vec<u8>)> {
    let metadata = fs::metadata(path).map_err(|error| crate::AppError::io(path, error))?;
    if !metadata.is_file() {
        return Err(ApplicationError::InvalidInput(format!(
            "plugin manifest is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ApplicationError::InvalidInput(format!(
            "plugin manifest exceeds {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    let bytes = fs::read(path).map_err(|error| crate::AppError::io(path, error))?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
    let manifest = AppManifest::parse(content).map_err(manifest_error)?;
    ManifestPlugin::from_manifest(
        manifest.clone(),
        crate::plugin::builtin_hooks(),
        ManifestSource::User(path.to_path_buf()),
    )
    .map_err(manifest_error)?;
    Ok((manifest, bytes))
}

fn manifest_error(error: crate::plugin::ManifestError) -> ApplicationError {
    ApplicationError::ValidationFailed {
        message: error.to_string(),
        details: Value::Null,
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> ApplicationResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| crate::AppError::io(path, error))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> ApplicationResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOVA: &str = include_str!("../plugin/fixtures/audit-nova.toml");

    fn application() -> Application {
        Application::from_state(Arc::new(crate::AppState::new(Arc::new(
            crate::Database::memory().unwrap(),
        ))))
    }

    #[test]
    fn install_rolls_back_copied_file_when_another_candidate_is_invalid() {
        let home = crate::test_support::TestHome::new();
        let directory = crate::plugin::user_plugins_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bad.toml"), "malformed manifest").unwrap();
        let source = home.path().join("candidate.toml");
        fs::write(&source, NOVA).unwrap();
        let app = application();
        let error = app.install_plugin_manifest(&source).err().unwrap();
        assert!(error.to_string().contains("not activated"));
        assert!(!directory.join("audit-nova.toml").exists());
        assert!(crate::plugin::get_plugin(&AppId::parse("audit-nova").unwrap()).is_none());
    }

    #[test]
    fn failed_remove_restores_manifest_and_keeps_data_before_purge() {
        let home = crate::test_support::TestHome::new();
        let source = home.path().join("candidate.toml");
        fs::write(&source, NOVA).unwrap();
        let app = application();
        let installed = app.install_plugin_manifest(&source).unwrap();
        let id = AppId::parse("audit-nova").unwrap();
        let original = crate::plugin::get_plugin(&id).unwrap();
        app.state
            .db
            .save_provider(
                id.as_str(),
                &crate::Provider::with_id("one".into(), "One".into(), json!({}), None),
            )
            .unwrap();
        fs::write(
            crate::plugin::user_plugins_dir().join("bad.toml"),
            "malformed manifest",
        )
        .unwrap();
        assert!(app.remove_plugin_manifest(&id, true).is_err());
        assert_eq!(fs::read_to_string(&installed.plugin.path).unwrap(), NOVA);
        assert!(Arc::ptr_eq(
            &original,
            &crate::plugin::get_plugin(&id).unwrap()
        ));
        assert!(
            app.state
                .db
                .get_provider_by_id("one", id.as_str())
                .unwrap()
                .is_some()
        );
        crate::plugin::unregister_plugin(&id).unwrap();
    }

    #[test]
    fn failed_purge_restores_removed_plugin() {
        let home = crate::test_support::TestHome::new();
        let source = home.path().join("candidate.toml");
        fs::write(&source, NOVA).unwrap();
        let app = application();
        let installed = app.install_plugin_manifest(&source).unwrap();
        let id = AppId::parse("audit-nova").unwrap();
        let original = crate::plugin::get_plugin(&id).unwrap();
        app.state
            .db
            .save_provider(
                id.as_str(),
                &crate::Provider::with_id("one".into(), "One".into(), json!({}), None),
            )
            .unwrap();
        app.state.db.conn.lock().unwrap().execute_batch(
            "CREATE TRIGGER deny_plugin_purge BEFORE DELETE ON providers WHEN OLD.app_type = 'audit-nova' BEGIN SELECT RAISE(ABORT, 'purge refused'); END;"
        ).unwrap();
        assert!(app.remove_plugin_manifest(&id, true).is_err());
        assert_eq!(fs::read_to_string(&installed.plugin.path).unwrap(), NOVA);
        assert!(Arc::ptr_eq(
            &original,
            &crate::plugin::get_plugin(&id).unwrap()
        ));
        assert!(
            app.state
                .db
                .get_provider_by_id("one", id.as_str())
                .unwrap()
                .is_some()
        );
        crate::plugin::unregister_plugin(&id).unwrap();
    }

    #[test]
    fn uninstall_keeps_data_unless_purge_was_requested() {
        let home = crate::test_support::TestHome::new();
        let source = home.path().join("candidate.toml");
        fs::write(&source, NOVA).unwrap();
        let app = application();
        app.install_plugin_manifest(&source).unwrap();
        let id = AppId::parse("audit-nova").unwrap();
        app.state
            .db
            .save_provider(
                id.as_str(),
                &crate::Provider::with_id("one".into(), "One".into(), json!({}), None),
            )
            .unwrap();
        crate::settings::mutate_settings(|settings| {
            settings
                .enabled_apps
                .get_or_insert_with(Default::default)
                .insert(id.to_string(), true);
            settings
                .app_config_dirs
                .get_or_insert_with(Default::default)
                .insert(
                    id.to_string(),
                    home.path().join("custom").to_string_lossy().into_owned(),
                );
        })
        .unwrap();
        app.upsert_editor_draft(
            &id,
            "unfinished",
            None,
            crate::application::EditorDraftInput {
                provider: crate::Provider::with_id("unfinished".into(), "".into(), json!({}), None),
                original_provider_id: None,
                base_provider_hash: None,
                editor_state: json!({"tab": "files"}),
                raw_documents: Some(vec!["{ unfinished".into()]),
                common_config: None,
            },
        )
        .unwrap();
        app.state.db.conn.lock().unwrap().execute(
            "INSERT INTO config_apply_heads(app_id,target_id,receipt_id) VALUES (?1, 'live', 'synthetic-receipt')", [id.as_str()]
        ).unwrap();
        app.remove_plugin_manifest(&id, false).unwrap();
        assert!(
            app.state
                .db
                .get_provider_by_id("one", id.as_str())
                .unwrap()
                .is_some()
        );
        assert!(app.get_editor_draft(&id, "unfinished").unwrap().is_some());
        assert!(
            app.state
                .db
                .config_apply_head(id.as_str(), "live")
                .unwrap()
                .is_some()
        );
        app.install_plugin_manifest(&source).unwrap();
        app.remove_plugin_manifest(&id, true).unwrap();
        assert!(
            app.state
                .db
                .get_provider_by_id("one", id.as_str())
                .unwrap()
                .is_none()
        );
        assert!(app.get_editor_draft(&id, "unfinished").unwrap().is_none());
        assert!(
            app.state
                .db
                .config_apply_head(id.as_str(), "live")
                .unwrap()
                .is_none()
        );
        let settings = crate::settings::get_settings();
        assert!(
            !settings
                .enabled_apps
                .unwrap_or_default()
                .contains_key(id.as_str())
        );
        assert!(
            !settings
                .app_config_dirs
                .unwrap_or_default()
                .contains_key(id.as_str())
        );
    }
}
