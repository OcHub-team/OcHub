# OcHub

A native desktop manager for switching API providers across AI coding tools —
**Claude Code, Claude Desktop, Codex, OpenCode, OpenClaw, and Hermes**.

This is a from-scratch rewrite of [`cc-switch`](https://github.com/farion1231/cc-switch)
(originally Tauri + React) onto a new stack:

- **[axum](https://github.com/tokio-rs/axum)** as the service backbone (control API + the local relay gateway), replacing Tauri's IPC/command layer.
- **[GPUI](https://www.gpui.rs/)** (Zed's GPU-accelerated UI framework) as the native UI, replacing the Tauri webview + React frontend.

OcHub owns its own data directory (`~/.ochub/`, with `ochub.db` on
an independent schema line starting at v1). On first launch it performs a
**one-time, read-only import** of existing cc-switch data
(`~/.cc-switch/cc-switch.db`, tolerant of schema v11–v16+): providers, MCP
servers, skills, usage history, settings, and managed OAuth accounts
all carry over, and `~/.cc-switch/` is never written to. Historical Gemini
usage rows remain readable, but Gemini CLI is no longer a managed app. OcHub
manages live config locations such as `~/.claude`, `~/.codex`, and the
OpenCode/OpenClaw/Hermes directories — quit the
original cc-switch app before switching providers from OcHub, or the two
will overwrite each other's live configs.

OcHub provides two connection paths:

1. **Direct connection** — automatically discovers local tool configs and
   switches the common provider fields in place.
2. **Relay-station mode** — adds a complete commercial relay configuration
   (for example New API or Sub2API) with its address, key, API format, model
   aliases, and reasoning mapping. A station can be applied to supported CLIs
   with one click; after that, switching stations takes effect without rewriting
   the app config again. The local conversion service and route bindings stay
   hidden as implementation details.

## Architecture

A Cargo workspace with three crates:

| Crate | Path | Role |
|-------|------|------|
| `ochub-core` | `crates/core` | UI/transport-agnostic core: domain model, config/paths, SQLite store, per-app live-config writers, provider switching, MCP/skills/sessions/sync/usage services. A faithful port of cc-switch's `src-tauri/src` minus Tauri. |
| `ochub-server` | `crates/server` | axum HTTP/JSON control API plus the in-process relay gateway (multi-dialect forwarding, channel routing, failover, health checks, and usage accounting). |
| `ochub-app` | `crates/app` | GPUI desktop application. Embeds `ochub-core` and hosts `ochub-server` in-process. |

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
cargo check -p ochub-core
cargo build -p ochub-server

# The GPUI desktop app (needs DEVELOPER_DIR + Metal toolchain, see above):
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer cargo run -p ochub-app
```

## Status

A faithful, near-complete port. `ochub-core` compiles green (hundreds of tests
passing) and contains the full cc-switch backend:

- Domain model, config/paths, device settings
- SQLite store — independent schema v4, backup/restore, legacy read-only import, and usage rollups
- Provider switching + all 6 per-app live-config writers + first-launch seeding
- MCP and common-config services
- Skills management through the Vercel `npx -y skills` CLI
- Relay-station mode — station-centric configuration, silent in-process
  lifecycle, hidden per-app bindings, model/reasoning mapping, protocol
  conversion, one-click CLI configuration, and usage accounting
- Usage statistics, session manager, environment management, deeplink import
- Cloud sync (WebDAV + S3, both functional) + auto-sync
- Model-fetch / speedtest / subscription / balance / coding-plan
- Auth — Copilot OAuth device flow, Codex OAuth, managed multi-account stores

`ochub-server` exposes the local control API. `ochub-app` is a working GPUI desktop UI
(sidebar app-switcher, provider list with switch/import/add/edit/delete, a
text-input component, settings panel, sessions browser, usage dashboard, and relay-station panel).

**Verified end-to-end:** direct switching rewrites the live configs; relay-station
mode starts its local service silently; hidden station bindings isolate the selected
commercial relay and map models/reasoning; official providers seed on first launch.

The former standalone proxy, live-config takeover, failover queue, circuit-breaker
configuration, upstream-proxy settings, UI page, and control API have been removed.
Routing and protocol translation now have a single owner: the gateway.

## License

OcHub is licensed under the **GNU General Public License v3.0 (or later)** —
see [LICENSE](LICENSE).
