//! Data Access Object layer
//!
//! Database access operations for each domain. All DAO methods are exposed as
//! `impl Database` blocks across these submodules.

pub mod gateway;
pub mod mcp;
pub mod providers;
pub mod providers_seed;
pub mod settings;
pub mod skills;
pub mod usage_config;
pub mod usage_rollup;
