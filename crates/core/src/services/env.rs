//! Environment-variable management.
//!
//! Ported from cc-switch `services/env_checker.rs` + `services/env_manager.rs`.
//! Detects and removes conflicting `ANTHROPIC_*` / `OPENAI_*` / `GEMINI_*` shell
//! env vars that would override the live config, with backup/restore.

pub mod checker;
pub mod manager;

pub use checker::{check_env_conflicts, EnvConflict};
pub use manager::{delete_env_vars, restore_env_backup, restore_from_backup, BackupInfo};
