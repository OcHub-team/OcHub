# OCHUB — architecture & integration contract

Living notes that anchor the port. Source of truth is `cc-switch/src-tauri/src`.

## Crates
- `ochub-core` (`crates/core`): domain + config + SQLite store + services. No Tauri/GPUI.
- `ochub-server` (`crates/server`): axum control API + local proxy. Depends on `ochub-core`.
- `ochub-app` (`crates/app`): GPUI UI. Depends on `ochub-core` (+ hosts `ochub-server`).

## Central state — `AppState` (from `store.rs`)
```rust
pub struct AppState {
    pub db: Arc<Database>,
    pub proxy_service: ProxyService,   // services/proxy.rs
    pub usage_cache: Arc<UsageCache>,  // services/usage_cache.rs
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
- `import_default_config(state, app_type) -> bool`
- `extract_common_config_snippet`, `migrate_legacy_common_config_usage_if_needed`,
  `update_sort_order`, speedtest/endpoint helpers, usage helpers.

### Switch modes (from `AppType::is_additive_mode`)
- **Switch mode** (Claude, ClaudeDesktop, Codex, Gemini): only the current provider
  is written to the live config; switching replaces it.
- **Additive mode** (OpenCode, OpenClaw, Hermes): all enabled providers are written;
  per-provider `meta.live_config_managed` tracks membership.

### Live-config write targets (per-app writer modules)
- Claude → `~/.claude/settings.json` (+ `claude_plugin.rs` for `~/.claude/config.json`).
  settings_config is the settings.json body; common-config snippet merged on write.
- Claude Desktop → `claude_desktop_config.rs` (direct vs proxy mode, model routes).
- Codex → `codex_config.rs`: writes `~/.codex/auth.json` + `~/.codex/config.toml`
  from `settings_config.{auth, config}`; OAuth + session-history bucketing logic.
- Gemini → `gemini_config.rs`: `~/.gemini/settings.json` (env-based).
- OpenCode → `opencode_config.rs`: `opencode.json` providers map (additive).
- OpenClaw → `openclaw_config.rs`: `openclaw.json` (additive).
- Hermes → `hermes_config.rs`: `config.yaml` (additive).

`live.rs::write_live_with_common_config` and `write_live_snapshot` are the choke
points that merge common config and write the file.

## Command surface to expose via axum (≈250 commands)
Grouped in `lib.rs invoke_handler`: providers, claude-desktop, config status, MCP
(claude + unified), prompts, skills (unified + legacy), proxy (start/stop/config/
failover/circuit-breaker), usage stats + pricing, sessions, sync (webdav/s3),
auth (managed accounts + copilot + codex oauth), OMO, OpenClaw, Hermes, workspace
files, env management, deeplink, settings, update/restart, lightweight mode.

## Port phases (each ends with `cargo check -p ochub-core` green)
1. ✅ Foundation: error, app_type, model, settings, app_store, paths.
2. ⏳ DB store: database/* → `crates/core/src/db/` (delegated).
3. Per-app writers + ProviderService + AppState (+ minimal ProxyService/UsageCache).
4. axum server: control API routes calling services; in-process host from app.
5. GPUI UI wired to live data (replace `demo_providers`): list/switch/add/edit/delete.
6. Proxy server (forward/failover/circuit-breaker/transforms/usage).
7. Remaining subsystems: MCP, prompts, skills, sessions, sync, auth, OMO/OpenClaw/
   Hermes specifics, deeplink, env, tray, updater, usage UI.
