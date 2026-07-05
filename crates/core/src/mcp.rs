//! MCP (Model Context Protocol) server management.
//!
//! Ported from cc-switch `src-tauri/src/mcp/`. Validates, syncs, and imports MCP
//! server configs into each app's live MCP config (`~/.claude.json`, codex
//! `config.toml` mcp section, opencode, hermes).
//!
//! Submodules:
//! - `validation` — server config validation
//! - `claude` — Claude MCP sync/import (`~/.claude.json`)
//! - `codex` — Codex MCP sync/import (incl. JSON↔TOML conversion)
//! - `opencode` — OpenCode MCP sync/import (local/remote format conversion)
//! - `hermes` — Hermes MCP sync/import (YAML format conversion)
//!
//! The top-level live-config writers (`claude_mcp`) live alongside as submodules
//! here too.

mod claude;
mod claude_mcp;
mod codex;
mod hermes;
mod opencode;
mod validation;

// Re-export the public sync/import API (mirrors cc-switch `mcp/mod.rs`).
pub use claude::{
    import_from_claude, remove_server_from_claude, sync_enabled_to_claude,
    sync_single_server_to_claude,
};
pub use codex::{
    import_from_codex, remove_server_from_codex, sync_enabled_to_codex, sync_single_server_to_codex,
};
pub use hermes::{import_from_hermes, remove_server_from_hermes, sync_single_server_to_hermes};
pub use opencode::{
    import_from_opencode, remove_server_from_opencode, sync_single_server_to_opencode,
};

// Re-export the live MCP-config helpers (cc-switch top-level `claude_mcp.rs`).
pub use claude_mcp::{
    clear_has_completed_onboarding, delete_mcp_server, get_mcp_status, read_mcp_json,
    read_mcp_servers_map, set_has_completed_onboarding, set_mcp_servers_map, upsert_mcp_server,
    validate_command_in_path, McpStatus,
};
