//! Data Access Object layer
//!
//! Database access operations for each domain. All DAO methods are exposed as
//! `impl Database` blocks across these submodules.

pub mod failover;
pub mod mcp;
pub mod prompts;
pub mod providers;
pub mod providers_seed;
pub mod proxy;
pub mod settings;
pub mod skills;
pub mod stream_check;
pub mod universal_providers;
pub mod usage_rollup;

// 导出 FailoverQueueItem 供外部使用
pub use failover::FailoverQueueItem;
