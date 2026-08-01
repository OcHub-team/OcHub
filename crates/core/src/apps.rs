//! Per-app live-config writers.
//!
//! Each submodule reads and writes one managed app's live configuration files
//! (`~/.claude`, `~/.codex`, `~/.config/opencode`, `~/.openclaw`,
//! `~/.hermes`, Claude Desktop's 3P profile). Ported from cc-switch's top-level
//! `*_config.rs` modules.

pub mod cherry_studio;
pub mod claude_desktop;
pub mod claude_plugin;
pub mod codex;
pub mod codex_app_launcher;
pub mod grokbuild;
pub mod hermes;
pub mod kimi_code;
pub mod openclaw;
pub mod opencode;
