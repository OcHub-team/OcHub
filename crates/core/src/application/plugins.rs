use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::application::{
    Application, ApplicationError, ApplicationResult, PluginDetails, PluginSummary,
};
use crate::plugin::{AppManifest, ManifestPlugin, ManifestSource};
use crate::AppId;

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

impl Application {
    pub fn list_user_plugins(&self) -> ApplicationResult<Vec<PluginSummary>> {
        let loaded = crate::plugin::loader::load_user_manifests(crate::plugin::builtin_hooks());
        let mut plugins = loaded
            .plugins
            .into_iter()
            .map(|plugin| PluginSummary {
                id: plugin.manifest().app.id.clone(),
                name: plugin.manifest().app.name.clone(),
                mode: plugin.manifest().app.mode.clone(),
                enabled: crate::plugin::is_app_enabled(plugin.as_ref()),
                path: plugin
                    .source_path()
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
        set_private_permissions(&destination)?;
        let errors = crate::plugin::reload_user_plugins();
        if let Some(error) = errors
            .iter()
            .find(|error| Path::new(&error.path) == destination)
        {
            return Err(ApplicationError::ValidationFailed {
                message: error.message.clone(),
                details: json!({ "path": error.path }),
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
        if purge_data {
            for provider in self.state.db.get_all_providers(id.as_str())?.into_values() {
                self.state.db.delete_provider(id.as_str(), &provider.id)?;
            }
            crate::settings::mutate_settings(|settings| {
                if let Some(enabled) = settings.enabled_apps.as_mut() {
                    enabled.remove(id.as_str());
                }
                if let Some(dirs) = settings.app_config_dirs.as_mut() {
                    dirs.remove(id.as_str());
                }
            })?;
        }
        fs::remove_file(&path).map_err(|error| crate::AppError::io(&path, error))?;
        let errors = crate::plugin::reload_user_plugins();
        Ok(json!({
            "removed": true,
            "id": id,
            "path": path,
            "purgedData": purge_data,
            "reloadErrors": errors
        }))
    }
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
