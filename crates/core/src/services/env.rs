//! Environment-variable management.
//!
//! Ported from cc-switch `services/env_checker.rs` + `services/env_manager.rs`.
//! Detects and removes conflicting `ANTHROPIC_*` / `OPENAI_*` shell
//! env vars that would override the live config, with backup/restore.

pub mod checker;
pub mod manager;

pub use checker::{EnvConflict, check_env_conflicts};
pub use manager::{BackupInfo, delete_env_vars, restore_env_backup, restore_from_backup};
