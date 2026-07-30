use crate::application::{
    Application, ApplicationError, ApplicationResult, OperationOutcome, redact_json,
};
use crate::settings::{S3SyncSettings, WebDavSyncSettings};

impl Application {
    pub fn webdav_sync_status(&self, show_secrets: bool) -> ApplicationResult<serde_json::Value> {
        sync_status_value(crate::settings::get_webdav_sync_settings(), show_secrets)
    }

    pub fn s3_sync_status(&self, show_secrets: bool) -> ApplicationResult<serde_json::Value> {
        sync_status_value(crate::settings::get_s3_sync_settings(), show_secrets)
    }

    pub fn configure_webdav_sync(
        &self,
        mut incoming: WebDavSyncSettings,
        preserve_empty_password: bool,
    ) -> ApplicationResult<serde_json::Value> {
        if let Some(existing) = crate::settings::get_webdav_sync_settings() {
            if preserve_empty_password && incoming.password.is_empty() {
                incoming.password = existing.password;
            }
            incoming.status = existing.status;
        }
        incoming.normalize();
        incoming.validate()?;
        crate::settings::set_webdav_sync_settings(Some(incoming))?;
        self.webdav_sync_status(false)
    }

    pub fn configure_s3_sync(
        &self,
        mut incoming: S3SyncSettings,
        preserve_empty_secret: bool,
    ) -> ApplicationResult<serde_json::Value> {
        if let Some(existing) = crate::settings::get_s3_sync_settings() {
            if preserve_empty_secret && incoming.secret_access_key.is_empty() {
                incoming.secret_access_key = existing.secret_access_key;
            }
            incoming.status = existing.status;
        }
        incoming.normalize();
        incoming.validate()?;
        crate::settings::set_s3_sync_settings(Some(incoming))?;
        self.s3_sync_status(false)
    }

    pub async fn test_webdav_sync(&self) -> ApplicationResult<serde_json::Value> {
        let settings = require_webdav(false)?;
        self.test_webdav_sync_settings(settings).await
    }

    pub async fn test_webdav_sync_settings(
        &self,
        mut settings: WebDavSyncSettings,
    ) -> ApplicationResult<serde_json::Value> {
        if settings.password.is_empty()
            && let Some(existing) = crate::settings::get_webdav_sync_settings()
        {
            settings.password = existing.password;
        }
        settings.normalize();
        settings.validate()?;
        crate::services::webdav_sync::check_connection(&settings)
            .await
            .map_err(map_sync_error)?;
        Ok(serde_json::json!({ "success": true, "message": "WebDAV connection ok" }))
    }

    pub async fn test_s3_sync(&self) -> ApplicationResult<serde_json::Value> {
        let settings = require_s3(false)?;
        self.test_s3_sync_settings(settings).await
    }

    pub async fn test_s3_sync_settings(
        &self,
        mut settings: S3SyncSettings,
    ) -> ApplicationResult<serde_json::Value> {
        if settings.secret_access_key.is_empty()
            && let Some(existing) = crate::settings::get_s3_sync_settings()
        {
            settings.secret_access_key = existing.secret_access_key;
        }
        settings.normalize();
        settings.validate()?;
        crate::services::s3_sync::check_connection(&settings)
            .await
            .map_err(map_sync_error)?;
        Ok(serde_json::json!({ "success": true, "message": "S3 connection ok" }))
    }

    pub async fn upload_webdav_sync(&self) -> ApplicationResult<serde_json::Value> {
        let mut settings = require_webdav(true)?;
        crate::services::webdav_sync::run_with_sync_lock(crate::services::webdav_sync::upload(
            &self.state.db,
            &mut settings,
        ))
        .await
        .map_err(map_sync_error)
    }

    pub async fn upload_s3_sync(&self) -> ApplicationResult<serde_json::Value> {
        let mut settings = require_s3(true)?;
        crate::services::s3_sync::run_with_sync_lock(crate::services::s3_sync::upload(
            &self.state.db,
            &mut settings,
        ))
        .await
        .map_err(map_sync_error)
    }

    pub async fn download_webdav_sync(
        &self,
    ) -> ApplicationResult<OperationOutcome<serde_json::Value>> {
        let mut settings = require_webdav(true)?;
        let data = crate::services::webdav_sync::run_with_sync_lock(
            crate::services::webdav_sync::download(&self.state.db, &mut settings),
        )
        .await
        .map_err(map_sync_error)?;
        Ok(self.finish_sync_download(data))
    }

    pub async fn download_s3_sync(&self) -> ApplicationResult<OperationOutcome<serde_json::Value>> {
        let mut settings = require_s3(true)?;
        let data = crate::services::s3_sync::run_with_sync_lock(
            crate::services::s3_sync::download(&self.state.db, &mut settings),
        )
        .await
        .map_err(map_sync_error)?;
        Ok(self.finish_sync_download(data))
    }

    pub async fn webdav_remote_info(&self) -> ApplicationResult<serde_json::Value> {
        let settings = require_webdav(true)?;
        Ok(crate::services::webdav_sync::fetch_remote_info(&settings)
            .await
            .map_err(map_sync_error)?
            .unwrap_or_else(|| serde_json::json!({ "empty": true })))
    }

    pub async fn s3_remote_info(&self) -> ApplicationResult<serde_json::Value> {
        let settings = require_s3(true)?;
        Ok(crate::services::s3_sync::fetch_remote_info(&settings)
            .await
            .map_err(map_sync_error)?
            .unwrap_or_else(|| serde_json::json!({ "empty": true })))
    }

    fn finish_sync_download(&self, data: serde_json::Value) -> OperationOutcome<serde_json::Value> {
        let mut warnings = Vec::new();
        if let Err(error) = crate::services::ProviderService::sync_current_to_live(&self.state) {
            warnings.push(format!(
                "snapshot restored, but live provider synchronization failed: {error}"
            ));
        }
        OperationOutcome { data, warnings }
    }
}

fn sync_status_value<T: serde::Serialize>(
    settings: Option<T>,
    show_secrets: bool,
) -> ApplicationResult<serde_json::Value> {
    let Some(settings) = settings else {
        return Ok(serde_json::json!({
            "configured": false,
            "settings": null
        }));
    };
    let settings = serde_json::to_value(settings)
        .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
    Ok(serde_json::json!({
        "configured": true,
        "settings": if show_secrets { settings } else { redact_json(&settings) }
    }))
}

fn require_webdav(enabled: bool) -> ApplicationResult<WebDavSyncSettings> {
    let settings =
        crate::settings::get_webdav_sync_settings().ok_or_else(|| ApplicationError::NotFound {
            kind: "webdav-sync-config",
            id: "default".to_string(),
        })?;
    if enabled && !settings.enabled {
        return Err(ApplicationError::InvalidInput(
            "WebDAV sync is configured but disabled".to_string(),
        ));
    }
    Ok(settings)
}

fn require_s3(enabled: bool) -> ApplicationResult<S3SyncSettings> {
    let settings =
        crate::settings::get_s3_sync_settings().ok_or_else(|| ApplicationError::NotFound {
            kind: "s3-sync-config",
            id: "default".to_string(),
        })?;
    if enabled && !settings.enabled {
        return Err(ApplicationError::InvalidInput(
            "S3 sync is configured but disabled".to_string(),
        ));
    }
    Ok(settings)
}

fn map_sync_error(error: crate::AppError) -> ApplicationError {
    match error {
        crate::AppError::HttpStatus { .. } => ApplicationError::Core(error),
        crate::AppError::InvalidInput(_)
        | crate::AppError::Config(_)
        | crate::AppError::Json { .. }
        | crate::AppError::Toml { .. } => ApplicationError::Core(error),
        other => ApplicationError::NetworkUnavailable(other.to_string()),
    }
}
