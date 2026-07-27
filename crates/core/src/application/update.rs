use serde_json::{json, Value};

use crate::application::{Application, ApplicationError, ApplicationResult};

impl Application {
    pub fn update_status(&self) -> ApplicationResult<Value> {
        let settings = crate::settings::get_settings();
        let channel = crate::services::update::channel::detect();
        Ok(json!({
            "currentVersion": crate::services::update::current_version(),
            "installChannel": channel.as_str(),
            "supportsSelfInstall": channel.supports_self_install(),
            "signatureVerificationConfigured": crate::services::update::manifest::signing_configured(),
            "autoCheck": settings.auto_update_check,
            "lastCheckedAt": settings.last_update_check_at,
            "skippedVersion": settings.skipped_update_version,
            "releaseUrl": crate::services::update::latest_release_url(None)
        }))
    }

    pub async fn check_for_update(&self) -> ApplicationResult<Value> {
        let result = crate::services::update::check_for_updates(None)
            .await
            .map_err(map_update_error)?;
        let now = chrono::Utc::now().timestamp();
        crate::settings::mutate_settings(|settings| {
            settings.last_update_check_at = Some(now);
        })?;
        serde_json::to_value(result)
            .map_err(|source| crate::AppError::JsonSerialize { source }.into())
    }

    pub async fn install_update(&self) -> ApplicationResult<Value> {
        if let Some(owner) = crate::runtime::active_owner()? {
            return Err(ApplicationError::OwnerConflict(format!(
                "{:?} runtime pid {} must be stopped before replacing the installed application",
                owner.kind, owner.pid
            )));
        }
        let Some(prepared) = crate::services::update::install::prepare(None, None)
            .await
            .map_err(map_update_error)?
        else {
            return Ok(json!({
                "installed": false,
                "reason": "already-current",
                "currentVersion": crate::services::update::current_version()
            }));
        };
        let version = prepared.version.clone();
        crate::services::update::apply_and_arm_restart(prepared).map_err(map_update_error)?;
        Ok(json!({
            "installed": true,
            "version": version,
            "restartArmed": true
        }))
    }
}

fn map_update_error(error: crate::AppError) -> ApplicationError {
    match error {
        crate::AppError::HttpStatus { .. } => ApplicationError::UpstreamRejected(error.to_string()),
        crate::AppError::Message(message)
            if message.contains("下载")
                || message.contains("网络")
                || message.contains("connection")
                || message.contains("request") =>
        {
            ApplicationError::NetworkUnavailable(message)
        }
        crate::AppError::Message(message) => ApplicationError::PlatformUnsupported(message),
        other => ApplicationError::Core(other),
    }
}
