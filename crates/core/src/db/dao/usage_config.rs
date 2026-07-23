//! Per-application usage pricing defaults.

use std::str::FromStr;

use rusqlite::params;
use rust_decimal::Decimal;

use crate::db::{lock_conn, Database};
use crate::error::AppError;

pub(crate) const PRICING_SOURCE_RESPONSE: &str = "response";
pub(crate) const PRICING_SOURCE_REQUEST: &str = "request";

pub(crate) fn validate_cost_multiplier(value: &str) -> Result<Decimal, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::localized(
            "error.multiplierEmpty",
            "倍率不能为空",
            "Multiplier cannot be empty",
        ));
    }
    let parsed = Decimal::from_str(trimmed).map_err(|error| {
        AppError::localized(
            "error.invalidMultiplier",
            format!("无效倍率: {value} - {error}"),
            format!("Invalid multiplier: {value} - {error}"),
        )
    })?;
    if parsed < Decimal::ZERO {
        return Err(AppError::localized(
            "error.invalidMultiplier",
            format!("无效倍率: {value} - 倍率不能为负数"),
            format!("Invalid multiplier: {value} - multiplier cannot be negative"),
        ));
    }
    Ok(parsed)
}

pub(crate) fn validate_pricing_source(value: &str) -> Result<&str, AppError> {
    let trimmed = value.trim();
    if matches!(trimmed, PRICING_SOURCE_RESPONSE | PRICING_SOURCE_REQUEST) {
        Ok(trimmed)
    } else {
        Err(AppError::localized(
            "error.invalidPricingMode",
            format!("无效计费模式: {value}"),
            format!("Invalid pricing mode: {value}"),
        ))
    }
}

impl Database {
    pub async fn get_default_cost_multiplier(&self, app_type: &str) -> Result<String, AppError> {
        let conn = lock_conn!(self.conn);
        match conn.query_row(
            "SELECT default_cost_multiplier FROM usage_config WHERE app_type = ?1",
            [app_type],
            |row| row.get(0),
        ) {
            Ok(value) => Ok(value),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok("1".to_string()),
            Err(error) => Err(AppError::Database(error.to_string())),
        }
    }

    pub async fn set_default_cost_multiplier(
        &self,
        app_type: &str,
        value: &str,
    ) -> Result<(), AppError> {
        validate_cost_multiplier(value)?;
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO usage_config (app_type, default_cost_multiplier)
             VALUES (?1, ?2)
             ON CONFLICT(app_type) DO UPDATE SET default_cost_multiplier = excluded.default_cost_multiplier",
            params![app_type, value.trim()],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(())
    }

    pub async fn get_pricing_model_source(&self, app_type: &str) -> Result<String, AppError> {
        let conn = lock_conn!(self.conn);
        match conn.query_row(
            "SELECT pricing_model_source FROM usage_config WHERE app_type = ?1",
            [app_type],
            |row| row.get(0),
        ) {
            Ok(value) => Ok(value),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(PRICING_SOURCE_RESPONSE.to_string()),
            Err(error) => Err(AppError::Database(error.to_string())),
        }
    }

    pub async fn set_pricing_model_source(
        &self,
        app_type: &str,
        value: &str,
    ) -> Result<(), AppError> {
        let value = validate_pricing_source(value)?;
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO usage_config (app_type, pricing_model_source)
             VALUES (?1, ?2)
             ON CONFLICT(app_type) DO UPDATE SET pricing_model_source = excluded.pricing_model_source",
            params![app_type, value],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_defaults_round_trip() {
        let db = Database::memory().unwrap();
        assert_eq!(
            futures::executor::block_on(db.get_default_cost_multiplier("claude")).unwrap(),
            "1"
        );
        futures::executor::block_on(db.set_default_cost_multiplier("claude", "1.25")).unwrap();
        futures::executor::block_on(db.set_pricing_model_source("claude", "request")).unwrap();
        assert_eq!(
            futures::executor::block_on(db.get_default_cost_multiplier("claude")).unwrap(),
            "1.25"
        );
        assert_eq!(
            futures::executor::block_on(db.get_pricing_model_source("claude")).unwrap(),
            "request"
        );
    }
}
