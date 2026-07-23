# OcHub — architecture & integration contract

Living notes for the OcHub architecture. cc-switch remains a compatibility
reference and read-only import source, not OcHub's runtime source of truth.

## Crates
- `ochub-core` (`crates/core`): domain + config + SQLite store + services. No Tauri/GPUI.
- `ochub-server` (`crates/server`): axum control API + local relay gateway. Depends on `ochub-core`.
- `ochub-app` (`crates/app`): GPUI UI. Depends on `ochub-core` (+ hosts `ochub-server`).

## Central state — `AppState` (from `store.rs`)
```rust
pub struct AppState {
    pub db: Arc<Database>,
    pub gateway: Arc<GatewayService>,  // gateway/service.rs
    pub usage_cache: Arc<UsageCache>,  // services/usage_cache.rs
    pub copilot_auth: Arc<RwLock<CopilotAuthManager>>,
    pub codex_oauth: Arc<RwLock<CodexOAuthManager>>,
}
impl AppState { pub fn new(db: Arc<Database>) -> Self }
```
`ochub-server::ServerState` and the GPUI app both hold an `Arc<AppState>`.

## Provider switching contract (from `commands/provider.rs` → `ProviderService`)
`ProviderService` lives in `services/provider/mod.rs` (2822 lines) + `live.rs` (1799).
Key API (all take `&AppState`):
- `list(state, app_type) -> IndexMap<String, Provider>`
- `current(state, app_type) -> String`
- `add(state, app_type, provider, add_to_live: bool) -> bool`
- `update(state, app_type, original_id: Option<&str>, provider) -> bool`
- `delete(state, app_type, id)`
- `switch(state, app_type, id) -> SwitchResult`
- `remove_from_live_config(state, app_type, id)`
- `auto_import_live_providers(state, app_type) -> usize`
- `extract_common_config_snippet`, `migrate_legacy_common_config_usage_if_needed`,
  `update_sort_order`, speedtest/endpoint helpers, usage helpers.

### Switch modes (from `AppType::is_additive_mode`)
- **Switch mode** (Claude, ClaudeDesktop, Codex): only the current provider
  is written to the live config; switching replaces it.
- **Additive mode** (OpenCode, OpenClaw, Hermes): all enabled providers are written;
  per-provider `meta.live_config_managed` tracks membership.

### Live-config write targets (per-app writer modules)
- Claude → `~/.claude/settings.json` (+ `claude_plugin.rs` for `~/.claude/config.json`).
  settings_config is the settings.json body; common-config snippet merged on write.
- Claude Desktop → native direct profile writer; arbitrary model routing is configured in Gateway.
- Codex → `codex_config.rs`: writes `~/.codex/auth.json` + `~/.codex/config.toml`
  from `settings_config.{auth, config}`; OAuth + session-history bucketing logic.
- OpenCode → `opencode_config.rs`: `opencode.json` providers map (additive).
- OpenClaw → `openclaw_config.rs`: `openclaw.json` (additive).
- Hermes → `hermes_config.rs`: `config.yaml` (additive).

`live.rs::write_live_with_common_config` and `write_live_snapshot` are the choke
points that merge common config and write the file.

## Command surface to expose via axum (≈250 commands)
Grouped into axum routers: providers, claude-desktop, config status, MCP
(claude + unified), skills (Vercel CLI-backed), gateway (lifecycle/config/
upstreams/routes/keys/import/apply), usage stats + pricing, sessions, sync (webdav/s3),
auth (managed accounts + copilot + codex oauth), OMO, OpenClaw, Hermes, env
management, deeplink, settings, update/restart, lightweight mode.

## Current integration rules

1. SQLite under `~/.ochub/` is OcHub's source of truth; cc-switch data is imported
   once and never written back.
2. The product exposes one complete commercial relay station (address, key,
   upstream API format, model aliases, and reasoning policy) as the user-facing
   unit. The in-process service remains the sole owner of protocol conversion
   and usage accounting. Each station maps internally to one channel and one
   hidden route; supported applications are configured once with an app-specific
   local key, so switching stations does not rewrite the application config.
   Legacy unbound keys remain readable for migration but are not exposed in the UI.
3. Skills are installed and linked by `npx -y skills`; SQLite retains catalog
   metadata and enabled-app state.
4. Gemini CLI producers and live writers are removed. Historical Gemini usage
   and vestigial compatibility columns remain readable.
5. The GPUI app and axum server share the same `Arc<AppState>`.
