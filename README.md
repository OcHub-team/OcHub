# RouteDeck

A native desktop manager for switching API providers across AI coding tools —
**Claude Code, Claude Desktop, Codex, OpenCode, OpenClaw, and Hermes**.

This is a from-scratch rewrite of [`cc-switch`](https://github.com/farion1231/cc-switch)
(originally Tauri + React) onto a new stack:

- **[axum](https://github.com/tokio-rs/axum)** as the service backbone (control API + the local provider proxy), replacing Tauri's IPC/command layer.
- **[GPUI](https://www.gpui.rs/)** (Zed's GPU-accelerated UI framework) as the native UI, replacing the Tauri webview + React frontend.

RouteDeck is a **drop-in backend replacement**: it reads and writes the same
data directory (`~/.cc-switch/`, including `cc-switch.db`) and the same live
config locations (`~/.claude`, `~/.codex`, …), so existing cc-switch data keeps
working. (Gemini CLI app management was dropped; the DB schema stays
cc-switch-v11 compatible, and historical Gemini usage rows still display.)

## Architecture

A Cargo workspace with three crates:

| Crate | Path | Role |
|-------|------|------|
| `routedeck-core` | `crates/core` | UI/transport-agnostic core: domain model, config/paths, SQLite store, per-app live-config writers, provider switching, MCP/prompts/skills/sessions/sync/usage services. A faithful port of cc-switch's `src-tauri/src` minus Tauri. |
| `routedeck-server` | `crates/server` | axum HTTP/JSON control API exposing the command surface, plus the local streaming provider proxy (forwarding, failover, circuit breaker, transforms, usage accounting). |
| `routedeck-app` | `crates/app` | GPUI desktop application. Embeds `routedeck-core` and hosts `routedeck-server` in-process. |

The reference source (`cc-switch/`) and the GPUI source (`zed/`) live alongside
the workspace and are excluded from it (`Cargo.toml` `[workspace] exclude`).

## Build prerequisites (macOS)

GPUI renders with Metal, and Zed pins a specific Rust toolchain. Two non-obvious
requirements:

1. **Rust 1.95.0** — pinned in `rust-toolchain.toml` (matches `zed/`). Install with
   `rustup toolchain install 1.95.0`.
2. **Xcode + the Metal toolchain.** Building `gpui_macos` compiles Metal shaders,
   which needs the full Xcode (not just Command Line Tools) *and* the separately
   downloadable Metal toolchain component (Xcode 26+):
   ```sh
   # point the build at Xcode (per-process; no sudo) — or run `sudo xcode-select -s ...`
   export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
   # one-time: download the Metal compiler component
   xcodebuild -downloadComponent MetalToolchain
   ```

## Building

```sh
# Core library and headless server (no Metal/Xcode needed):
cargo check -p routedeck-core
cargo build -p routedeck-server

# The GPUI desktop app (needs DEVELOPER_DIR + Metal toolchain, see above):
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer cargo run -p routedeck-app
```

## Status

A faithful, near-complete port. `routedeck-core` compiles green (hundreds of tests
passing) and contains the full cc-switch backend:

- Domain model, config/paths, device settings
- SQLite store — schema v11 (cc-switch compatible), backup/restore, JSON migration, ~100 DAOs
- Provider switching + all 6 per-app live-config writers + first-launch seeding
- MCP / prompts / common-config services (sync-on-switch wired)
- Skills management wrapping the Vercel `skills` CLI (`npx -y skills`) —
  install/uninstall/toggle/update delegated to the CLI, registry kept in SQLite
- Proxy server — lifecycle, live-config takeover/restore (Claude + Codex),
  hot-switch, passthrough forwarding, circuit breaker, cross-format transforms
  (Claude↔OpenAI-chat↔Responses↔Gemini-native, streaming + non-streaming),
  request/response content-encoding (gzip/deflate/br/zstd), rquickjs
  usage-script engine
- Usage statistics, session manager, environment management, deeplink import
- Cloud sync (WebDAV + S3, both functional) + auto-sync
- Model-fetch / speedtest / subscription / balance / coding-plan
- Auth — Copilot OAuth device flow, Codex OAuth, managed multi-account stores

`routedeck-server` exposes ~150 control-API routes. `routedeck-app` is a working
GPUI desktop UI (sidebar app-switcher, provider list with
switch/import/add/edit/delete/reorder, provider editor with common-config
snippet + copy-to-app, confirm dialogs, settings panel, an async proxy panel,
panic-hook crash reports, window-bounds persistence, a single-instance guard,
and a first-run notice).

**Verified end-to-end:** provider switching rewrites the live configs; the proxy
starts/stops on a loopback port; official providers seed on first launch.

**Remaining (tracked):** app-shell polish (tray icon, updater download/install,
auto-launch, OS deeplink scheme registration, dark theme, a code-signed `.app`
for distribution).

## License

RouteDeck is licensed under the **GNU General Public License v3.0 (or later)** —
see [LICENSE](LICENSE).
