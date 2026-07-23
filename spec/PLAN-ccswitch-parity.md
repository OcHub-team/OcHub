# Plan: cc-switch parity + Gemini CLI removal + skills → `npx skills`

> Historical upstream integration plan. Its Gemini-removal and Skills-CLI
> workstreams are now integrated. The proxy workstream is superseded by OcHub's
> in-process relay gateway, which is the only active routing implementation.
> Names and paths below are retained as implementation history.

Authored 2026-07-04 (Fable, from first-hand code reading + 6-agent gap analysis).
Detailed machine-readable inventories live in the session scratchpad:
`/tmp/claude-1000/-home-sleepstars-gateway/4addb833-df69-400e-9371-6ed1b803e4c0/scratchpad/`
(`gemini-inventory.json`, `skills-blast.json`, `proxy-status.json`, `ui-parity.json`,
`command-parity.json`, `shell-parity.json`).

## Findings that shape the plan

- The port is far more complete than README claims. Cross-format transforms
  (Claude↔OpenAI-chat↔Responses↔Gemini-native, streaming + non-streaming) are
  line-identical to cc-switch AND wired (`forward_transform.rs`); the rquickjs
  usage-script engine is a complete port (8-line diff). ~260 of ~277 cc-switch
  commands have HTTP equivalents.
- The one **serious functional gap** in the proxy: `content_encoding.rs` was never
  ported, yet `http_client.rs:226` disables reqwest auto-decompression. Compressed
  client request bodies (Codex Desktop sends zstd/gzip) and compressed upstream
  responses on the Codex-conversion path fail to parse; passthrough usage
  accounting silently skips compressed non-SSE responses.
- UI gaps are convenience-level, not data-model-level (no common-config snippet
  editor, no confirm dialogs, no reorder, no update badge, plain-text editors).
- Skills subsystem (3037-line SSOT service) has ~20 HTTP routes, a GPUI view,
  WebDAV/S3 sync coupling, and a sync-on-switch hook — full blast radius mapped
  in `skills-blast.json`.

## Standing decisions (resolve the analysts' ambiguities)

1. **DB schema stays cc-switch-v11-compatible.** No destructive migrations. All
   `enabled_gemini` columns, CHECK constraints, and existing rows stay. The
   `gemini: bool` fields on `McpApps`/`SkillApps`/legacy-JSON structs REMAIN as
   vestigial always-false fields (serde compat + DAO column positions unchanged).
   Only stop *seeding* new gemini data where trivially separable
   (`providers_seed.rs` gemini-official entry). DAO read/write of the column is
   left as-is wherever removing it would change SQL column lists.
2. **Historical usage rows keep rendering.** Read/display paths that match
   `'gemini_session'` / `app_type='gemini'` (usage_stats CASE arms, usage_view
   label/color lookups, usage_rollup IN-lists) are KEPT so old charts don't lose
   data. Only the *producers* are removed (session_usage_gemini, `/gemini`
   inbound endpoint).
3. **Inbound `/v1beta` + `/gemini/*` proxy routes are removed** along with
   `handle_gemini` and `resolve_gemini`. Raw Gemini-wire *clients* are out of
   scope; gemini as *upstream provider format* (`api_format="gemini_native"`,
   transform/streaming/schema/shadow/url, `ProviderType::Gemini`, pricing
   catalog) is untouched — see the keep-list in `gemini-inventory.json`.
4. **Proxy takeover ends up Claude+Codex only.** No replacement third app.
5. **Header-case preservation divergence is accepted** (documented in server.rs).

## Workstream G — remove Gemini CLI app support

Follow `gemini-inventory.json` remove-list precisely, amended by decisions 1–2
above (keep vestigial struct fields + DB columns + historical read paths).
Core edit: delete `AppType::Gemini`; the compiler then drives ~50 files of match
arms. Whole-file deletes: `apps/gemini.rs`, `provider_config/gemini.rs`,
`services/session_usage_gemini.rs`, `mcp/gemini.rs`, `mcp/gemini_mcp.rs`,
`services/provider/gemini_auth.rs`, `session_manager/providers/gemini.rs`,
`crates/app/assets/icons/agents/gemini.svg`.

Partitions (disjoint file sets, editable in parallel):
- **G1 core-non-proxy**: app_type.rs, apps{,.rs}, mcp/*, services/* (mcp, provider*,
  proxy.rs takeover legs, subscription.rs ~1000-line Gemini section, env/checker,
  config.rs, skill.rs arms, sql_helpers, usage_stats producers-only, session_manager/*,
  prompt/files.rs, settings.rs, model.rs, deeplink/*).
- **G2 core-proxy + db**: proxy/forward.rs inbound arms, proxy/server.rs routes +
  handle_gemini (KEEP gemini_shadow state), proxy/providers.rs AppType-keyed arms
  only, db/dao/providers_seed.rs; db/proxy_types.rs `ProxyTakeoverStatus.gemini`.
- **G3 server + tests**: api_apps.rs, api_data.rs, api_more.rs, control_api_smoke.rs
  (drop the gemini row; unknown app → 400 per AppType::from_str).
- **G4 app UI**: app_ui, app_settings_view, mcp_view, prompts_view, settings_view,
  shell_menu, skills_view, tools_view, usage_view (keep historical display arms),
  icons.rs, theme.rs.

Then a fix-up agent compiles core+server and repairs residual fallout.

## Workstream S — skills management wraps the Vercel `skills` CLI

Replace the SSOT engine inside `services/skill.rs` with a wrapper around
`npx -y skills` (interface verified 2026-07-04: `add <pkg> [-g] [-a agents]
[-s skills] [-l] [-y] [--copy]`, `remove [-g] [-a] [-s] [-y]`, `list [-g]`,
`update [-y]`, `find`, `init`). Design:

- **Registry of record stays SQLite** (`skills`, `skill_repos` tables, unchanged
  schema): OcHub records id/name/description/source(owner/repo@branch)/apps
  so per-app toggles, auto-sync watchers, and the UI keep working. File
  placement/symlinking is delegated entirely to the CLI.
- App↔agent mapping probed at runtime (`claude→claude`, `codex→codex`,
  `opencode→opencode`, `openclaw→openclaw`, hermes: if unsupported by the CLI,
  surface "not supported" in toggles rather than faking it).
- install(repo, skills, apps) → `npx -y skills add <owner/repo> -g -y -s <names>
  -a <agents>`; uninstall → `remove -g -y -s <name> -a '*'`; toggle_app →
  add/remove with `-a <agent>` using the recorded source; update_skill/update-all
  → `skills update -g -y`; catalog/discover for a repo → `skills add <repo> -l`
  parsed; skills.sh search stays as-is (plain HTTP, unrelated to storage).
- Implementation MUST probe real CLI output first (run `npx -y skills list -g`,
  install a sample into a temp HOME) and write robust parsing; capture
  stderr for error mapping into the existing `format_skill_error` codes
  (NPX_MISSING as a new code with a friendly "install Node.js" message).
- Sync surface for WebDAV/S3 (`sync_protocol.rs`/`archive.rs` call
  `SkillService::get_ssot_dir`): repoint to the CLI's canonical global store
  (probe location, expected `~/.agents/skills`; fall back to legacy
  `~/.cc-switch/skills` if CLI store absent) so skills.zip snapshots keep working.
- **Dropped** (superseded by CLI; delete routes + UI affordances + service code):
  backups (list/delete/restore), migrate-storage + SkillStorageLocation UI,
  install-from-zip, scan-unmanaged/import-from-apps, content-hash update
  checking, migrate_skills_to_ssot bootstrap. `check_updates` route returns an
  empty list (UI button becomes "update all").
- **Removed hook**: `live.rs:1023` sync_to_app on provider switch (CLI installs
  persist in agent dirs; nothing to re-sync).
- Deeplink skill-repo import stays (it just registers a `SkillRepo`).

## Workstream P — proxy correctness fixes (port from cc-switch)

- **P1 content-encoding**: port `cc-switch/src-tauri/src/proxy/content_encoding.rs`
  (gzip/deflate/br/zstd, stacked encodings, request+response) and wire:
  (a) decompress-or-reject client request bodies before every
  `serde_json::from_slice` in forward.rs (`prepare_passthrough_body`, codex
  conversion gate) and forward_transform.rs (`:72`);
  (b) force `accept-encoding: identity` upstream on the codex-conversion branch
  (mirror `forward_transform.rs:289`), or decompress before conversion;
  (c) decompress before the passthrough usage-log parse (`forward.rs:1020`).
  Needs zstd/brotli deps added to workspace Cargo.toml mirroring cc-switch's.
- **P2 unlabeled-SSE fallback** (cc-switch #2234): port `body_looks_like_sse` +
  aggregate-then-transform fallback into `non_stream_back_transformed` and the
  codex non-stream branch.
- Unit tests ported/added for both.

## Workstream U — UI & API parity quick wins

- **U1 server routes** (sonnet-simple): POST `/api/config/:app/common-snippet/extract`
  → `ProviderService::extract_common_config_snippet`; POST `/api/deeplink/merge`
  → `deeplink::parse_and_merge_config` (preview before import).
- **U2 provider editor**: common-config snippet section (enable toggle + edit +
  "extract from current" using U1 semantics via core service, merge-on-write
  already exists in live.rs); per-provider "convert/copy to app X" action using
  existing codecs + universal plumbing.
- **U3 shared confirm modal**: one GPUI confirm component; wire into provider /
  prompt / MCP / session / memory deletes (skills_view handled in S).
- **U4 list & shell niceties**: provider reorder (up/down, persist via existing
  `/api/providers/:app/sort` service fn), startup auto-update-check + sidebar
  badge, env-conflict banner on provider list (reuse tools_view scan).
- **U5 app shell quick wins**: panic hook writing crash reports, window-bounds
  persistence via settings DB, lock-file single-instance guard, first-run notice
  dialog reading/writing `first_run_notice_confirmed`.

## Deferred (tracked, out of this pass)

Deeplink OS scheme registration + import dialog; usage-script authoring modal;
dark theme; syntax-highlighted JSON/markdown editors; real tray icon (GPUI has
no status-item API); updater download/install; lightweight-mode window
destruction; DatabaseUpgrade recovery UI; byte-exact header casing.

## Verification protocol

Machine sits behind mitmproxy — every network-touching cargo/git command needs:
`CARGO_HTTP_CAINFO=$HOME/.mitmproxy/mitmproxy-ca-cert.pem GIT_SSL_CAINFO=$HOME/.mitmproxy/mitmproxy-ca-cert.pem CARGO_NET_GIT_FETCH_WITH_CLI=true`

- Baseline (2026-07-04): `cargo check -p ochub-core -p ochub-server` green.
- Gate each workstream on `cargo check`; final gate `cargo test -p ochub-core
  -p ochub-server` vs the recorded baseline, plus `cargo check -p ochub-app`
  if the Linux GPUI probe succeeds (in flight).
- No commits without explicit ask; work lands as uncommitted diff on `main`.
