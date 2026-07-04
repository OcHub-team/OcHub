//! Prompt management.
//!
//! Ported from cc-switch top-level `prompt.rs` + `prompt_files.rs`. The `Prompt`
//! struct is the same one the DB layer reads/writes, so we re-export it from
//! `crate::db::legacy_json` rather than re-defining it.

mod files;

pub use crate::db::legacy_json::Prompt;
pub use files::prompt_file_path;
