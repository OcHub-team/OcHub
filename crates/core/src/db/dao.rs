//! Data Access Object layer
//!
//! Database access operations for each domain. All DAO methods are exposed as
//! `impl Database` blocks across these submodules.

pub mod gateway;
pub mod mcp;
pub mod prompts;
pub mod providers;
pub mod providers_seed;
pub mod proxy;
pub mod settings;
pub mod skills;
pub mod stream_check;
pub mod usage_rollup;
