use serde_json::Value;

use crate::application::{
    Application, ApplicationError, ApplicationResult, providers::redact_json,
};
use crate::settings::ProxySettings;

impl Application {
    pub fn settings(&self, show_secrets: bool) -> ApplicationResult<Value> {
        let settings = if show_secrets {
            crate::settings::get_settings()
        } else {
            crate::settings::get_settings_for_frontend()
        };
        let value = serde_json::to_value(settings).map_err(|error| {
            ApplicationError::InvalidInput(format!("failed to serialize settings: {error}"))
        })?;
        Ok(if show_secrets {
            value
        } else {
            redact_json(&value)
        })
    }

    pub fn get_setting(&self, path: &str, show_secrets: bool) -> ApplicationResult<Value> {
        let value = self.settings(show_secrets)?;
        get_path(&value, path)
            .cloned()
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "setting",
                id: path.to_string(),
            })
    }

    pub fn set_setting(&self, path: &str, value: Value) -> ApplicationResult<Value> {
        let (next, normalized) = prepare_setting(path, value)?;
        crate::settings::update_settings(next)?;
        Ok(normalized)
    }

    pub fn validate_setting(&self, path: &str, value: Value) -> ApplicationResult<Value> {
        prepare_setting(path, value).map(|(_, normalized)| normalized)
    }

    pub fn unset_setting(&self, path: &str) -> ApplicationResult<Value> {
        let defaults =
            serde_json::to_value(crate::settings::AppSettings::default()).map_err(|error| {
                ApplicationError::InvalidInput(format!(
                    "failed to serialize setting defaults: {error}"
                ))
            })?;
        self.get_setting(path, false)?;
        let replacement = get_path(&defaults, path).cloned().unwrap_or(Value::Null);
        self.set_setting(path, replacement)
    }

    pub fn proxy_settings(&self, show_secrets: bool) -> ProxySettings {
        let mut proxy = crate::settings::get_settings().proxy.unwrap_or_default();
        if !show_secrets && !proxy.password.is_empty() {
            proxy.password = "******".to_string();
        }
        proxy
    }

    pub fn set_proxy_settings(&self, mut proxy: ProxySettings) -> ApplicationResult<ProxySettings> {
        resolve_proxy_password(&mut proxy);
        proxy.normalize();
        proxy.validate()?;
        let stored = proxy.clone();
        crate::settings::mutate_settings(move |settings| {
            settings.proxy = Some(stored);
        })?;
        Ok(proxy)
    }

    pub async fn test_proxy_settings(&self, mut proxy: ProxySettings) -> ApplicationResult<Value> {
        resolve_proxy_password(&mut proxy);
        proxy.enabled = true;
        proxy.normalize();
        proxy.validate()?;
        crate::services::network_proxy::check_connection(&proxy).await?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

fn resolve_proxy_password(proxy: &mut ProxySettings) {
    if !proxy.password.is_empty() && proxy.password.chars().all(|character| character == '*') {
        proxy.password = crate::settings::get_settings()
            .proxy
            .filter(|stored| {
                stored.host.trim() == proxy.host.trim()
                    && stored.port == proxy.port
                    && stored.username.trim() == proxy.username.trim()
            })
            .map(|stored| stored.password)
            .unwrap_or_default();
    }
}

fn prepare_setting(
    path: &str,
    value: Value,
) -> ApplicationResult<(crate::settings::AppSettings, Value)> {
    if path.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "setting path cannot be empty".to_string(),
        ));
    }
    let mut document = serde_json::to_value(crate::settings::get_settings()).map_err(|error| {
        ApplicationError::InvalidInput(format!("failed to serialize settings: {error}"))
    })?;
    set_path(&mut document, path, value)?;
    let next: crate::settings::AppSettings = serde_json::from_value(document).map_err(|error| {
        ApplicationError::InvalidInput(format!("invalid setting value: {error}"))
    })?;
    let normalized = serde_json::to_value(&next).map_err(|error| {
        ApplicationError::InvalidInput(format!("failed to normalize settings: {error}"))
    })?;
    if get_path(&normalized, path).is_none() {
        return Err(ApplicationError::NotFound {
            kind: "setting",
            id: path.to_string(),
        });
    }
    Ok((next, get_path(&normalized, path).cloned().unwrap()))
}

fn tokens(path: &str) -> impl Iterator<Item = &str> {
    path.split('.').filter(|token| !token.is_empty())
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    tokens(path).try_fold(value, |current, token| current.get(token))
}

fn set_path(value: &mut Value, path: &str, replacement: Value) -> ApplicationResult<()> {
    let parts = tokens(path).collect::<Vec<_>>();
    let Some((last, parents)) = parts.split_last() else {
        return Err(ApplicationError::InvalidInput(
            "setting path cannot be empty".to_string(),
        ));
    };
    let mut current = value;
    for token in parents {
        current = current
            .get_mut(*token)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "setting",
                id: path.to_string(),
            })?;
    }
    let object = current
        .as_object_mut()
        .ok_or_else(|| ApplicationError::InvalidInput(format!("{path} is not an object path")))?;
    object.insert((*last).to_string(), replacement);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{get_path, set_path};

    #[test]
    fn dotted_setting_paths_update_existing_and_candidate_fields() {
        let mut value = serde_json::json!({
            "theme": { "mode": "system" },
            "enabled": true
        });
        set_path(&mut value, "theme.mode", serde_json::json!("dark")).unwrap();
        assert_eq!(
            get_path(&value, "theme.mode"),
            Some(&serde_json::json!("dark"))
        );
        set_path(&mut value, "theme.missing", serde_json::json!(1)).unwrap();
        assert_eq!(
            get_path(&value, "theme.missing"),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn typed_normalization_accepts_omitted_optional_fields_and_drops_unknown_fields() {
        let mut optional = serde_json::to_value(crate::settings::AppSettings::default()).unwrap();
        assert!(get_path(&optional, "language").is_none());
        set_path(&mut optional, "language", serde_json::json!("zh-CN")).unwrap();
        let settings: crate::settings::AppSettings = serde_json::from_value(optional).unwrap();
        let normalized = serde_json::to_value(settings).unwrap();
        assert_eq!(
            get_path(&normalized, "language"),
            Some(&serde_json::json!("zh-CN"))
        );

        let mut unknown = serde_json::to_value(crate::settings::AppSettings::default()).unwrap();
        set_path(&mut unknown, "missing", serde_json::json!(1)).unwrap();
        let settings: crate::settings::AppSettings = serde_json::from_value(unknown).unwrap();
        let normalized = serde_json::to_value(settings).unwrap();
        assert!(get_path(&normalized, "missing").is_none());
    }
}
