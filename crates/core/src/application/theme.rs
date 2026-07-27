use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::settings::ThemeMode;
use crate::theme::{ThemeFamily, ThemeRecord};

impl Application {
    pub fn list_themes(&self) -> ApplicationResult<Value> {
        let registry = crate::theme::load_registry();
        let settings = crate::settings::get_settings();
        Ok(json!({
            "selected": settings.theme_family,
            "mode": settings.theme_mode,
            "themes": registry.themes,
            "diagnostics": registry.diagnostics
        }))
    }

    pub fn get_theme(&self, id: &str) -> ApplicationResult<ThemeRecord> {
        crate::theme::load_registry()
            .themes
            .iter()
            .find(|record| record.family.id == id)
            .cloned()
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "theme",
                id: id.to_string(),
            })
    }

    pub fn validate_theme_file(&self, path: &Path) -> ApplicationResult<ThemeFamily> {
        crate::theme::read_theme_file(path).map_err(Into::into)
    }

    pub fn import_theme(&self, path: &Path) -> ApplicationResult<Value> {
        let family = crate::theme::import_family(path)?;
        let record = self.get_theme(&family.id)?;
        Ok(json!({
            "imported": true,
            "theme": record
        }))
    }

    pub fn export_theme(&self, id: &str, path: Option<&Path>) -> ApplicationResult<Value> {
        let record = self.get_theme(id)?;
        if let Some(path) = path {
            crate::theme::export_family(&record.family, path)?;
            Ok(json!({
                "exported": true,
                "id": id,
                "path": path
            }))
        } else {
            serde_json::to_value(record.family)
                .map_err(|source| crate::AppError::JsonSerialize { source }.into())
        }
    }

    pub fn duplicate_theme(&self, id: &str) -> ApplicationResult<Value> {
        let source = self.get_theme(id)?;
        let family = crate::theme::duplicate_family(&source.family)?;
        let path = crate::theme::save_user_family(&family)?;
        Ok(json!({
            "duplicated": true,
            "sourceId": id,
            "theme": family,
            "path": path
        }))
    }

    pub fn delete_theme(&self, id: &str) -> ApplicationResult<PathBuf> {
        let settings = crate::settings::get_settings();
        if settings.theme_family == id {
            return Err(ApplicationError::ResourceConflict(format!(
                "theme {id} is selected; choose another theme before deleting it"
            )));
        }
        crate::theme::delete_user_family(id).map_err(Into::into)
    }

    pub fn set_theme_family(&self, id: &str) -> ApplicationResult<Value> {
        let record = self.get_theme(id)?;
        crate::settings::mutate_settings(|settings| {
            settings.theme_family = id.to_string();
        })?;
        Ok(json!({
            "selected": record.family.id,
            "mode": crate::settings::get_settings().theme_mode
        }))
    }

    pub fn set_theme_mode(&self, mode: ThemeMode) -> ApplicationResult<Value> {
        crate::settings::mutate_settings(|settings| {
            settings.theme_mode = mode;
        })?;
        Ok(json!({
            "selected": crate::settings::get_settings().theme_family,
            "mode": mode
        }))
    }
}
