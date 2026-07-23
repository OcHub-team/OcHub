//! Capability surfaces implemented per plugin.

use serde_json::Value;

use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;

/// Live config file read/write surface of one app.
pub trait LiveConfigOps: Send + Sync {
    /// Switch mode: write the provider as the live config.
    /// Additive mode: upsert this provider into the live config.
    ///
    /// `db` is needed by apps whose live write is DB-coupled (Claude Desktop).
    fn write_live(&self, db: &Database, provider: &Provider) -> Result<(), AppError>;

    /// Additive mode: remove one provider from the live config.
    /// Switch-mode apps return an error.
    fn remove_from_live(&self, provider_id: &str) -> Result<(), AppError>;

    /// Read the current live config (import / editor prefill).
    fn read_live(&self) -> Result<Value, AppError>;
}
