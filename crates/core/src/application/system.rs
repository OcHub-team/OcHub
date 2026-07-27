use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};

use crate::application::{Application, ApplicationResult, DoctorCheck, DoctorReport};

impl Application {
    pub async fn doctor(&self, network: bool) -> ApplicationResult<DoctorReport> {
        let mut checks = Vec::new();
        checks.push(ok(
            "database",
            format!(
                "database opened with schema v{}",
                crate::Database::schema_version()
            ),
            json!({
                "path": crate::paths::get_database_path(),
                "schemaVersion": crate::Database::schema_version()
            }),
        ));

        let data_dir = crate::paths::get_app_config_dir();
        checks.push(check_directory("data-dir", &data_dir));

        match crate::runtime::active_owner() {
            Ok(Some(owner)) => checks.push(ok(
                "runtime-owner",
                format!("{} owner is active", owner_kind(owner.kind)),
                serde_json::to_value(owner).unwrap_or(Value::Null),
            )),
            Ok(None) => checks.push(warning(
                "runtime-owner",
                "no persistent runtime owner is active",
                Value::Null,
            )),
            Err(error) => checks.push(error_check("runtime-owner", error.to_string(), Value::Null)),
        }

        let config_paths = self
            .list_apps()?
            .into_iter()
            .map(|app| {
                json!({
                    "app": app.id,
                    "path": app.config_dir,
                    "error": app.config_error
                })
            })
            .collect::<Vec<_>>();
        let config_errors = config_paths
            .iter()
            .filter(|entry| !entry["error"].is_null())
            .count();
        checks.push(if config_errors == 0 {
            ok(
                "app-config-paths",
                "all application config paths resolved",
                json!({ "paths": config_paths }),
            )
        } else {
            warning(
                "app-config-paths",
                format!("{config_errors} application config paths could not be resolved"),
                json!({ "paths": config_paths }),
            )
        });

        let dependency_checks = futures::future::join_all(
            [
                "node", "npx", "claude", "codex", "opencode", "openclaw", "hermes",
            ]
            .into_iter()
            .map(check_dependency),
        )
        .await;
        checks.extend(dependency_checks);

        let gateway = self.gateway_config()?;
        let gateway_status = self.state.gateway.status().await;
        if gateway_status.running {
            checks.push(ok(
                "gateway-port",
                "Gateway is listening",
                json!({ "host": "127.0.0.1", "port": gateway.port }),
            ));
        } else {
            match TcpListener::bind(("127.0.0.1", gateway.port)) {
                Ok(listener) => {
                    drop(listener);
                    checks.push(ok(
                        "gateway-port",
                        "Gateway port is available",
                        json!({ "host": "127.0.0.1", "port": gateway.port }),
                    ));
                }
                Err(error) => checks.push(error_check(
                    "gateway-port",
                    format!("Gateway port is unavailable: {error}"),
                    json!({ "host": "127.0.0.1", "port": gateway.port }),
                )),
            }
        }

        let plugin_errors = crate::plugin::manifest_load_errors();
        checks.push(if plugin_errors.is_empty() {
            ok("plugins", "all plugin manifests loaded", Value::Null)
        } else {
            error_check(
                "plugins",
                format!("{} plugin manifest(s) failed to load", plugin_errors.len()),
                json!({ "errors": plugin_errors }),
            )
        });

        checks.push(sync_config_check(
            "webdav",
            crate::settings::get_webdav_sync_settings()
                .as_ref()
                .map(|settings| settings.validate().map_err(|error| error.to_string())),
        ));
        checks.push(sync_config_check(
            "s3",
            crate::settings::get_s3_sync_settings()
                .as_ref()
                .map(|settings| settings.validate().map_err(|error| error.to_string())),
        ));

        if network {
            if crate::settings::get_webdav_sync_settings().is_some() {
                checks.push(match self.test_webdav_sync().await {
                    Ok(_) => ok("webdav-network", "WebDAV connection succeeded", Value::Null),
                    Err(error) => error_check("webdav-network", error.to_string(), Value::Null),
                });
            }
            if crate::settings::get_s3_sync_settings().is_some() {
                checks.push(match self.test_s3_sync().await {
                    Ok(_) => ok("s3-network", "S3 connection succeeded", Value::Null),
                    Err(error) => error_check("s3-network", error.to_string(), Value::Null),
                });
            }
        }

        Ok(DoctorReport {
            healthy: !checks.iter().any(|check| check.status == "error"),
            checks,
        })
    }

    pub fn portable_runtime_status(&self) -> ApplicationResult<Value> {
        let channel = crate::services::update::channel::detect();
        let executable = std::env::current_exe()
            .map_err(|error| crate::AppError::io(Path::new("current-executable"), error))?;
        Ok(json!({
            "portable": matches!(
                channel,
                crate::services::update::channel::InstallChannel::WindowsPortable
            ),
            "installChannel": channel.as_str(),
            "supportsSelfInstall": channel.supports_self_install(),
            "executable": executable
        }))
    }

    pub fn desktop_autostart_status(&self) -> ApplicationResult<Value> {
        Ok(json!({
            "enabled": crate::autostart::is_enabled()?,
            "supported": crate::autostart::unsupported_reason().is_none(),
            "unsupportedReason": crate::autostart::unsupported_reason()
        }))
    }

    pub fn set_desktop_autostart(&self, enabled: bool) -> ApplicationResult<Value> {
        let silent = crate::settings::get_settings().silent_startup;
        crate::autostart::set_enabled(enabled, silent)?;
        crate::settings::mutate_settings(|settings| settings.launch_on_startup = enabled)?;
        self.desktop_autostart_status()
    }
}

fn check_directory(id: &str, path: &Path) -> DoctorCheck {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => ok(
            id,
            "directory is available",
            json!({ "path": path, "readonly": metadata.permissions().readonly() }),
        ),
        Ok(_) => error_check(
            id,
            "configured path is not a directory",
            json!({ "path": path }),
        ),
        Err(error) => error_check(
            id,
            format!("directory is unavailable: {error}"),
            json!({ "path": path }),
        ),
    }
}

async fn check_dependency(program: &str) -> DoctorCheck {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let result = match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(result) => result,
        Err(_) => {
            return warning(
                &format!("dependency:{program}"),
                format!("{program} version probe timed out after 3 seconds"),
                Value::Null,
            )
        }
    };
    match result {
        Ok(output) if output.status.success() => ok(
            &format!("dependency:{program}"),
            format!("{program} is available"),
            json!({
                "version": String::from_utf8_lossy(&output.stdout).trim()
            }),
        ),
        Ok(output) => warning(
            &format!("dependency:{program}"),
            format!("{program} was found but its version probe failed"),
            json!({ "exitCode": output.status.code() }),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => warning(
            &format!("dependency:{program}"),
            format!("{program} is not installed"),
            Value::Null,
        ),
        Err(error) => warning(
            &format!("dependency:{program}"),
            format!("{program} could not be probed: {error}"),
            Value::Null,
        ),
    }
}

fn sync_config_check(backend: &str, result: Option<Result<(), String>>) -> DoctorCheck {
    match result {
        None => warning(
            &format!("sync:{backend}"),
            format!("{backend} sync is not configured"),
            Value::Null,
        ),
        Some(Ok(())) => ok(
            &format!("sync:{backend}"),
            format!("{backend} sync configuration is valid"),
            Value::Null,
        ),
        Some(Err(message)) => error_check(
            &format!("sync:{backend}"),
            format!("{backend} sync configuration is invalid: {message}"),
            Value::Null,
        ),
    }
}

fn owner_kind(kind: crate::runtime::OwnerKind) -> &'static str {
    match kind {
        crate::runtime::OwnerKind::Gui => "GUI",
        crate::runtime::OwnerKind::Daemon => "daemon",
        crate::runtime::OwnerKind::Foreground => "foreground",
    }
}

fn ok(id: &str, message: impl Into<String>, details: Value) -> DoctorCheck {
    check(id, "ok", message, details)
}

fn warning(id: &str, message: impl Into<String>, details: Value) -> DoctorCheck {
    check(id, "warning", message, details)
}

fn error_check(id: &str, message: impl Into<String>, details: Value) -> DoctorCheck {
    check(id, "error", message, details)
}

fn check(id: &str, status: &str, message: impl Into<String>, details: Value) -> DoctorCheck {
    DoctorCheck {
        id: id.to_string(),
        status: status.to_string(),
        message: message.into(),
        details,
    }
}
