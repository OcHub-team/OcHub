# Subsystem 12 — Session Manager, Per-Provider Session Sources, Terminal Launch, Session Usage Sync

This spec describes the complete behavior of the **session manager** subsystem in cc-switch, covering:

1. The session **scanner / loader / deleter** (`src-tauri/src/session_manager/`).
2. The per-provider **session source parsers** (claude, codex, gemini, hermes, openclaw, opencode).
3. The **terminal launcher** (macOS-only resume support).
4. The Tauri **commands** that expose all of the above to the frontend.
5. The **session usage sync** services that scan provider transcript files / SQLite DBs and import token-usage rows into the `usage_logs` table (`session_usage*.rs`).

The rewrite target is Rust + GPUI + axum. There is **no real external HTTP** in this subsystem — all I/O is local filesystem and SQLite. The Tauri command layer becomes either GPUI actions or axum endpoints; `spawn_blocking` becomes a thread pool / `tokio::task::spawn_blocking`.

---

## 0. Provider IDs and module map

Provider IDs (string constants, used everywhere as routing keys):

| `provider_id` | module file | session storage kind |
|---|---|---|
| `claude` | `providers/claude.rs` | JSONL transcript files |
| `codex` | `providers/codex.rs` | JSONL transcript files (active + archived) |
| `gemini` | `providers/gemini.rs` | single JSON files |
| `hermes` | `providers/hermes.rs` | SQLite **and** JSONL (merged) |
| `openclaw` | `providers/openclaw.rs` | JSONL transcript files + `sessions.json` index |
| `opencode` | `providers/opencode.rs` | legacy JSON flat-file **and** SQLite (merged) |

`session_manager/mod.rs` dispatches on `provider_id`. Module declaration order in `providers/mod.rs`: `claude, codex, gemini, hermes, openclaw, opencode` (plus private `utils`).

---

## 1. Data structures

All structs live in `session_manager/mod.rs` unless noted. serde `rename_all = "camelCase"` is applied to every public DTO; fields are serialized to JS in camelCase.

### 1.1 `SessionMeta` (Serialize, camelCase) — one scanned session
```rust
pub struct SessionMeta {
    pub provider_id: String,                                  // -> providerId
    pub session_id: String,                                   // -> sessionId
    #[serde(skip_serializing_if = "Option::is_none")] pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub project_dir: Option<String>,   // -> projectDir
    #[serde(skip_serializing_if = "Option::is_none")] pub created_at: Option<i64>,        // -> createdAt (ms epoch)
    #[serde(skip_serializing_if = "Option::is_none")] pub last_active_at: Option<i64>,    // -> lastActiveAt (ms epoch)
    #[serde(skip_serializing_if = "Option::is_none")] pub source_path: Option<String>,    // -> sourcePath
    #[serde(skip_serializing_if = "Option::is_none")] pub resume_command: Option<String>, // -> resumeCommand
}
```
Notes:
- `source_path` is **opaque routing data** — usually a real filesystem path, but for SQLite-backed providers it is a prefixed pseudo-path (`sqlite:<db>#<id>` for hermes; `sqlite:<db>:<id>` for opencode; for opencode JSON it is the **message directory** path).
- `created_at` / `last_active_at` are epoch **milliseconds** (i64). Sorting and timestamp parsing all use ms.

### 1.2 `SessionMessage` (Serialize, camelCase) — one transcript line
```rust
pub struct SessionMessage {
    pub role: String,    // normalized: "user" | "assistant" | "tool" | original/"unknown"
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub ts: Option<i64>, // ms epoch
}
```

### 1.3 `DeleteSessionRequest` (Deserialize, camelCase) — batch delete input item
```rust
pub struct DeleteSessionRequest {
    pub provider_id: String,   // <- providerId
    pub session_id: String,    // <- sessionId
    pub source_path: String,   // <- sourcePath
}
```

### 1.4 `DeleteSessionOutcome` (Serialize, camelCase) — batch delete result item
```rust
pub struct DeleteSessionOutcome {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub error: Option<String>,
}
```

### 1.5 Usage-sync DTOs (in `services/session_usage.rs`)
```rust
#[derive(Serialize, Deserialize, camelCase)]
pub struct SessionSyncResult { pub imported: u32, pub skipped: u32, pub files_scanned: u32, pub errors: Vec<String> }

#[derive(Serialize, Deserialize, camelCase)]
pub struct DataSourceSummary { pub data_source: String, pub request_count: u32, pub total_cost_usd: String }
```
`files_scanned` → `filesScanned`; `data_source` → `dataSource`; `request_count` → `requestCount`; `total_cost_usd` → `totalCostUsd`.

### 1.6 Internal-only structs (not serialized)
- `ParsedAssistantUsage` (claude usage): `message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens: u32`, `stop_reason: Option<String>`, `timestamp: Option<String>`, `session_id: Option<String>`.
- `CumulativeTokens { input, cached_input, output: u64 }`, `DeltaTokens { input, cached_input, output: u32 }` (+ `is_zero()`), `FileParseState { session_id: Option<String>, current_model: String, prev_total: Option<CumulativeTokens>, event_index: u32 }` (codex usage).
- `GeminiTokens { input, output, cached, thoughts: u32 }` (gemini usage).
- `OpenCodeMessageData { input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens: u32, cost: f64, model_id: String, timestamp_ms: i64 }`, `OpenCodeMessageQueryResult { messages: Vec<(String, OpenCodeMessageData)>, has_incomplete_usage: bool }` (opencode usage).
- `DedupKey` (from `services::usage_stats`, referenced): fields used here are `app_type: &str, model: &str, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens: u32, created_at: i64`.

---

## 2. Files & formats (on-disk paths)

Base config-dir helpers come from other subsystems; the directory roots used here are:

| Provider | Root helper | Path (typical, macOS/Linux `~`) | Format |
|---|---|---|---|
| claude | `config::get_claude_config_dir()` + `/projects` | `~/.claude/projects/<encoded-project>/<session>.jsonl` | JSONL |
| claude (sidecar) | — | `<session-stem>/` dir next to the jsonl (subagents/tool-results) | dir |
| codex | `codex_config::get_codex_config_dir()` + `/sessions` and `/archived_sessions` | `~/.codex/sessions/YYYY/MM/DD/*.jsonl`, `~/.codex/archived_sessions/*.jsonl` | JSONL |
| gemini | `gemini_config::get_gemini_dir()` + `/tmp` | `~/.gemini/tmp/<project>/chats/session-*.json`, `~/.gemini/tmp/<project>/.project_root` | JSON |
| hermes (sqlite) | `hermes_config::get_hermes_dir()` + `/state.db` | `~/.hermes/state.db` | SQLite |
| hermes (jsonl) | `hermes_config::get_hermes_dir()` + `/sessions` | `~/.hermes/sessions/*.jsonl|*.json` | JSONL/JSON |
| openclaw | `openclaw_config::get_openclaw_dir()` + `/agents` | `~/.openclaw/agents/<agent>/sessions/*.jsonl` + `sessions.json` | JSONL + JSON index |
| opencode (json) | `get_opencode_base_dir()` + `/storage` | `$XDG_DATA_HOME/opencode/storage/...` or `~/.local/share/opencode/storage/...` | JSON tree |
| opencode (sqlite) | `get_opencode_base_dir()` + `/opencode.db` | `~/.local/share/opencode/opencode.db` (+`-wal`) | SQLite |

### 2.1 OpenCode base-dir resolution (`opencode.rs::get_opencode_base_dir`)
- If env `XDG_DATA_HOME` is set and non-empty → `<XDG_DATA_HOME>/opencode`.
- Else `dirs::home_dir()/.local/share/opencode`, fallback literal `.local/share/opencode`.
- `get_opencode_data_dir()` = base + `/storage`. `get_opencode_db_path()` (private here; `opencode_config::get_opencode_db_path()` in the usage service) = base + `/opencode.db`.

### 2.2 OpenCode JSON storage tree layout
```
storage/
  session/**/<sessionId>.json          (session meta: id, title, directory, time.{created,updated}, projectID)
  message/<sessionId>/<msgId>.json     (per-message: id, role, time.created)
  part/<messageId>/<partId>.json       (parts: {type:"text",text} | {type:"tool",tool})
  session_diff/<sessionId>.json
  project/<projectId>.json             (NOT touched by delete)
```
`source_path` for a JSON-backed opencode session = the **message directory** `storage/message/<sessionId>`.

### 2.3 OpenCode SQLite schema (read & delete)
Tables `session(id, title, directory, time_created, time_updated)`, `message(id, session_id, time_created, time_updated, data)`, `part(id, session_id, message_id, time_created, data)`. `data` columns are JSON strings. `source_path` = `sqlite:<db_path>:<sessionId>` (note session IDs begin with `ses_`, exploited by the parser).

### 2.4 Hermes SQLite schema
`sessions` table with flexible columns (accessed via `PRAGMA table_info`): used keys `id, title, cwd|directory, started_at|created_at, ended_at|updated_at`. `messages(role, content, created_at, session_id)`. `source_path` = `sqlite:<db_path>#<sessionId>`.

### 2.5 OpenClaw `sessions.json` index format
Map keyed by arbitrary string (e.g. `"agent:main:main"`) → object `{ "sessionId": "...", "displayName": "...", "sessionFile": "..." }`. Used to resolve display titles and to prune on delete. Written back with `config::write_json_file`.

### 2.6 Usage-sync state table
`session_log_sync(file_path TEXT PRIMARY KEY, last_modified INTEGER, last_line_offset INTEGER, last_synced_at INTEGER)`. `last_modified` stores file **mtime in nanoseconds** (new writes); legacy values may be seconds (harmlessly triggers one re-scan). Imports land in `usage_logs` (24-column insert, see §5).

---

## 3. Behavior — core `session_manager/mod.rs`

### 3.1 `pub fn scan_sessions() -> Vec<SessionMeta>`
- Spawns **6 OS threads** via `std::thread::scope`, one per provider, in this spawn order: codex, claude, opencode, openclaw, gemini, hermes. Each `h.join().unwrap_or_default()` (a panicking provider yields empty vec, never aborts).
- Concatenates results in order r1..r6 (codex, claude, opencode, openclaw, gemini, hermes).
- **Sorts descending** by `last_active_at.or(created_at).unwrap_or(0)` (most recent first). Stable-ish via `sort_by` comparing `b_ts.cmp(&a_ts)`.
- Returns the full flat list. No pagination.

### 3.2 `pub fn load_messages(provider_id: &str, source_path: &str) -> Result<Vec<SessionMessage>, String>`
- If `provider_id == "opencode"` and `source_path` starts with `"sqlite:"` → `opencode::load_messages_sqlite`.
- If `provider_id == "hermes"` and `source_path` starts with `"sqlite:"` → `hermes::load_messages_sqlite`.
- Otherwise dispatch by id to `<provider>::load_messages(Path::new(source_path))`.
- Unknown provider → `Err("Unsupported provider: {id}")`.

### 3.3 `pub fn delete_session(provider_id, session_id, source_path: &str) -> Result<bool, String>`
- SQLite short-circuit for opencode/hermes (same prefix test) → `<provider>::delete_session_sqlite(session_id, source_path)`.
- Else: `roots = provider_roots(provider_id)?`, then `delete_session_with_roots(...)`.

### 3.4 `provider_roots(provider_id) -> Result<Vec<PathBuf>, String>`
| id | roots |
|---|---|
| codex | `[<codex>/sessions, <codex>/archived_sessions]` |
| claude | `[<claude>/projects]` |
| opencode | `[get_opencode_data_dir()]` (i.e. storage) |
| openclaw | `[<openclaw>/agents]` |
| gemini | `[<gemini>/tmp]` |
| hermes | `[<hermes>/sessions]` |
| other | `Err("Unsupported provider: …")` |

### 3.5 `delete_session_with_roots(provider_id, session_id, source_path: &Path, roots) -> Result<bool,String>`
**Security-critical path-validation logic** (must reproduce exactly):
1. `validated_source = canonicalize_existing_path(source_path, "session source")` — errors `"session source not found: <path>"` if missing, or `"Failed to resolve session source <path>: <e>"`.
2. Iterate roots; skip non-existent. Track `saw_existing_root`.
3. For each existing root, canonicalize it (`"session root"`), and if `validated_source.starts_with(validated_root)` → dispatch to `<provider>::delete_session(&validated_root, &validated_source, session_id)`.
4. If no root existed → `Err("Session root not found for provider {id}: {first_root_or_<none>}")`.
5. If roots existed but source is under none → `Err("Session source path is outside provider roots: <source>")`.

This prevents deleting files outside provider roots (e.g. via a forged `source_path`). The SQLite delete paths perform an analogous check by canonicalizing the DB path and comparing to the expected DB path.

### 3.6 `pub fn delete_sessions(requests: &[DeleteSessionRequest]) -> Vec<DeleteSessionOutcome>`
- Maps each request through `delete_session`. Order preserved.
- `Ok(true)` → `success:true, error:None`. `Ok(false)` → `success:false, error:"Session was not deleted"`. `Err(e)` → `success:false, error:Some(e)`.

### 3.7 Helpers
- `canonicalize_existing_path(path, label)` — see 3.5.
- `collect_delete_session_outcomes(requests, deleter)` — generic mapper used by `delete_sessions` (testable seam).

---

## 4. Behavior — per-provider parsers

Shared utilities (`providers/utils.rs`):
- `TITLE_MAX_CHARS: usize = 80`.
- `read_head_tail_lines(path, head_n, tail_n) -> io::Result<(Vec<String>, Vec<String>)>`: files < 16 KiB read fully and split; larger files read head lines from start, then seek to `len-16384` for tail and **drop the first partial line** when `seek_pos>0`, finally keep the last `tail_n`.
- `parse_timestamp_to_ms(&Value) -> Option<i64>`: integer/float → if `>1e12` treat as ms else seconds×1000; string → RFC3339 → `timestamp_millis()`.
- `extract_text(&Value) -> String`: String→itself; Array→join non-empty item texts with `\n`; Object→`.text`. Per-item rules (`extract_text_from_item`): `type=="tool_use"` → `"[Tool: {name}]"`; `type=="tool_result"` → recurse into `.content` (None if empty); else try `.text`, `.input_text`, `.output_text`, then `.content` recursively.
- `truncate_summary(text, max_chars)`: trims; if `chars().count() <= max` returns as-is; else first `max` chars + `"..."` (char-aware, so len can be ≤ max+3 for ASCII).
- `path_basename(value) -> Option<String>`: trims trailing `/`/`\`, returns last path segment.

### 4.1 Claude (`claude.rs`)
- **scan**: `<claude>/projects` recursively collect `*.jsonl`. Skip files whose name starts with `agent-` (`is_agent_session`). Parse each.
- **parse_session**: reads head 10 / tail 30 lines.
  - Head: `sessionId`, `cwd`→project_dir, first `timestamp`→created_at, **first real user message** as title candidate (`type=="user"` or `message.role=="user"`), skipping ones containing `<local-command-caveat>` or starting with `<command-name>`.
  - Tail (reverse): last `timestamp`→last_active_at; last `type=="custom-title"` → `customTitle`; last non-meta `message.content` → summary (skips `isMeta==true`).
  - session_id fallback = filename stem.
  - **Title priority**: customTitle > first user message > project_dir basename. summary truncated to 160; title to `TITLE_MAX_CHARS`.
  - `resume_command = "claude --resume {session_id}"`.
- **load_messages**: per line; skip `isMeta==true`; read `.message`; role from `.message.role` (default `"unknown"`). **Reclassify**: if role `"user"` and `content` is a non-empty array whose items are ALL `tool_result` → role `"tool"`. content via `extract_text`; skip empty. ts from top-level `timestamp`.
- **delete_session(root, path, session_id)**: parse meta, assert `session_id` match (else mismatch error). Delete sidecar dir = `path.parent()/<file_stem>` (recursively, if exists). Then `remove_file(path)`.

### 4.2 Codex (`codex.rs`)
- `session_roots()` = `[<codex>/sessions, <codex>/archived_sessions]`. **scan** recursively collects `*.jsonl` from both.
- **parse_session** (head 10 / tail 30):
  - Skip **subagent** sessions: `session_meta.payload.source` object contains key `"subagent"` → return None.
  - Head: from `type=="session_meta"` payload extract `id`→session_id, `cwd`→project_dir, payload `timestamp`. created_at from any top-level `timestamp`. First user message from `type=="response_item"` payload `type=="message" && role=="user"` via `title_candidate_from_user_message`.
  - **title_candidate**: drop empties, drop messages starting with `# AGENTS.md` or `<environment_context>`. If starts with `"# Context from my IDE setup:"` → `extract_codex_prompt_from_ide_context` (finds the LAST `## My request for Codex:` heading; inline `: text` or following lines; separators `: ： - —`; ASCII-case-insensitive marker `"my request for codex"`).
  - Tail (reverse): last `timestamp`→last_active_at; last `response_item` message → summary.
  - session_id fallback = first UUID match in filename (regex `[0-9a-fA-F]{8}-…-{12}`).
  - Title: first user message > project_dir basename. `resume_command = "codex resume {session_id}"`.
- **load_messages**: only `type=="response_item"` lines; payload `type`:
  - `"message"` → role from payload.role, content via extract_text.
  - `"function_call"` → role `"assistant"`, content `"[Tool: {name}]"`.
  - `"function_call_output"` → role `"tool"`, content = `.output` string.
  - else skip. Skip empty content. ts from top `timestamp`.
- **delete_session**: parse meta, assert id match, `remove_file`.

### 4.3 Gemini (`gemini.rs`)
- **scan**: iterate `<gemini>/tmp/<project>/chats/*.json`. Reads sibling `<project>/.project_root` file → its text becomes `project_dir` (overrides parse's None).
- **parse_session**: reads whole JSON. Requires top-level `sessionId`. `startTime`→created_at, `lastUpdated`→last_active_at (fallback created_at). Title/summary = first `messages[*]` with `type=="user"` whose `content` is a string, truncated 160. `resume_command="gemini --resume {session_id}"`.
- **load_messages**: iterate `messages[]`: `type=="gemini"`→assistant, `"user"`→user, `"info"|"error"|other`→skip. content = string OR array-of-`{text}` joined `\n`. Append `[Tool: {name}]` for each entry in `toolCalls[]`. Skip empty. ts from `timestamp`.
- **delete_session**: parse meta, assert id match, `remove_file`.

### 4.4 Hermes (`hermes.rs`) — dual-source
- `get_hermes_db_path()` = `<hermes>/state.db`; `get_hermes_sessions_dir()` = `<hermes>/sessions`.
- **scan_sessions**: merge `scan_sessions_sqlite()` (precedence) with `scan_sessions_jsonl()`; jsonl entries with session_id already present in sqlite set are dropped.
- **SQLite scan**: open RO+NO_MUTEX. Require `sessions` table (via `sqlite_master`). `PRAGMA table_info` → columns; `SELECT * FROM sessions ORDER BY rowid DESC LIMIT 500`; each row → JSON map (string→int→float→null coercion). Map to meta: `id`→session_id (required), `title`→title (truncate 80), `cwd|directory`→project_dir, `started_at|created_at`→created_at, `ended_at|updated_at`→last_active_at. `source_path = "sqlite:<db>#<id>"`. `resume_command = None`.
- **JSONL scan**: `<hermes>/sessions/*.jsonl|*.json`. `parse_jsonl_session` (head 30 / tail 10): first/last timestamp (`timestamp` or `ts`); from `type=="session"|"init"` extract `id|sessionId`, `title`, `cwd|directory`; first user message (`role` or `message.role == "user"`) via extract_text → summary/title fallback. session_id fallback = filename stem. `resume_command=None`.
- **load_messages_sqlite(source)**: `parse_sqlite_source` splits on **last `#`** → `(db_path, session_id)` (session_id non-empty). Open RO. `SELECT role, content, created_at FROM messages WHERE session_id=?1 ORDER BY created_at ASC`. ts parsed via `parse_timestamp_to_ms`.
- **load_messages(path)** (JSONL): supports flat `{role,content,timestamp|ts}` and nested `{type:"message", message:{role,content}, timestamp}`.
- **delete_session_sqlite(session_id, source)**: parse source; canonicalize db path; require it equals canonicalized `get_hermes_db_path()` (else `"SQLite path does not match expected Hermes database"`); require `ref_session_id == session_id`. Open RW, `unchecked_transaction`, `DELETE FROM messages WHERE session_id` (best-effort), `DELETE FROM sessions WHERE id`, commit. Returns `deleted>0`.
- **delete_session(_root, path, _session_id)** (JSONL): just `remove_file` (no id verification — note: id arg ignored).

### 4.5 OpenClaw (`openclaw.rs`)
- **scan**: `<openclaw>/agents/<agent>/sessions/*.jsonl`. Per sessions dir, load `sessions.json` → `displayName` map (`load_display_names`: keyed by `sessionId`→`displayName`).
- `strip_message_id_suffix`: removes trailing `\n[message_id: ...]` gateway metadata.
- **parse_session(path, display_names)** (head 10 / tail 30): from `type=="session"` extract `id`, `cwd`, `timestamp`. From `type=="message"`: cleaned content → first user message (role user) and summary. last_active_at from tail. session_id fallback = filename stem.
  - **Title priority**: displayName (from index) > first user message > cwd basename. summary truncate 160; title 80. `resume_command=None` (gateway-managed).
- **load_messages**: lines `type=="message"`; `.message.role` mapped `"toolResult"→"tool"`, else passthrough; content via extract_text; ts from `timestamp`.
- **delete_session**: parse meta (no display names), assert id match; `prune_sessions_index(<dir>/sessions.json, session_id, path)` removes entries where `sessionId==id` OR `sessionFile==source_path` (writes back via `write_json_file`; no-op if index missing); then `remove_file`.

### 4.6 OpenCode (`opencode.rs`) — dual-source
- **scan_sessions**: merge `scan_sessions_json()` + `scan_sessions_sqlite()` (SQLite precedence by id).
- `parse_sqlite_source(source)`: strip `sqlite:`, split on **last `":ses_"`** (handles Windows `C:\` colons) → `(db_path, session_id)`.
- **SQLite scan**: open RO+NO_MUTEX. `SELECT id, title, directory, time_created, time_updated FROM session ORDER BY time_updated DESC`. Title empty → directory basename. created_at/last_active_at are the raw integers (already ms in opencode). `source_path = "sqlite:<db>:<id>"`. `resume_command = "opencode session resume {id}"`.
- **JSON scan**: `storage/session/**/*.json` → `parse_session`: require `id`. title (non-empty) else directory basename. created/updated from `time.created|updated`. `source_path = storage/message/<id>` (message dir). If no explicit title, compute summary via `get_first_user_summary` (reads earliest user message's first part text, truncated 160) — skipped when title present (I/O optimization).
- **load_messages(path)** (JSON; path is the message dir): storage = `path.parent().parent()`. Collect message JSONs; for each read `id`, `role`, `time.created`; gather parts from `storage/part/<msgId>/*.json` via `collect_parts_text` (text part→text, tool part→`[Tool: {tool}]`). Sort by created ts ascending. Skip empty.
- **load_messages_sqlite(source)**: open RO. Query `message(id,time_created,data)` and `part(message_id,data)` for session, build parts map, reconstruct content per message (text/tool parts joined `\n`), role from message `data.role`. ts = message `time_created`.
- **delete_session(storage, path, session_id)** (JSON): require `path.file_name() == session_id`. Collect message ids; delete each `storage/part/<msgId>` dir; delete `storage/session_diff/<id>.json`; delete the message dir; find & delete `storage/session/**/<id>.json` (`find_session_file`). `project/*.json` untouched.
- **delete_session_sqlite(session_id, source)**: parse source; canonicalize and compare to `get_opencode_db_path()` (else `"SQLite path does not match expected OpenCode database"`); id match check. RW transaction: `DELETE FROM part`, `DELETE FROM message`, `DELETE FROM session` (all by session). Returns `deleted>0`.

---

## 5. Behavior — terminal launcher (`terminal/mod.rs`)

### `pub fn launch_terminal(target: &str, command: &str, cwd: Option<&str>, custom_config: Option<&str>) -> Result<(), String>`
- Empty `command` → `Err("Resume command is empty")`.
- **Non-macOS** → `Err("Terminal resume is only supported on macOS")` (whole feature is macOS-only).
- Dispatch on `target`:
  - `"terminal"` → `Terminal.app` via osascript `tell application "Terminal" … do script "<cmd>"`.
  - `"iTerm" | "iterm"` → osascript: `create window with default profile`, `write text`.
  - `"ghostty"` → `open -na Ghostty --args --quit-after-last-window-closed=true [--working-directory=<cwd>] -e <SHELL> -l -c <command>`.
  - `"kitty"` → `open -na kitty --args -e <SHELL> -l -c "<cd && cmd>"`.
  - `"wezterm"` → `open -na WezTerm --args start [--cwd <cwd>] -- <SHELL> -c <cmd>`.
  - `"kaku"` → same as wezterm with app `Kaku`.
  - `"alacritty"` → `open -na Alacritty --args [--working-directory <cwd>] -e <SHELL> -c <cmd>`.
  - `"warp"` (unix only) → requires `cwd`; writes a self-deleting temp `.sh` (mode 0755) in cwd that `exec`s the command, then `open -a Warp "warp://action/new_tab?path=<script>"`.
  - `"custom"` → requires `custom_config` template; replace `{command}` and `{cwd}` (cwd default `"."`); run via `sh -c <line>`.
  - else → `Err("Unsupported terminal target: {target}")`.
- Each launcher runs `Command::new("open"|"osascript"|"sh")...status()`; non-success → provider-specific error string.
- Helpers: `build_shell_command(cmd, cwd)` = `cd "<escaped cwd>" && <cmd>` if cwd non-empty else `cmd`. `shell_escape` wraps in double quotes escaping `\` and `"`. `escape_osascript` escapes `\` and `"`. (Ghostty/wezterm/kaku/alacritty pass cwd as a flag instead of embedding it.)
- `SHELL` env var, default `/bin/zsh`.

---

## 6. Behavior — Tauri commands (`commands/session_manager.rs`)

All are `#[tauri::command] async`, run heavy work on `tauri::async_runtime::spawn_blocking`, and use camelCase JS params (`#![allow(non_snake_case)]`).

| Command | JS params | Returns | Behavior |
|---|---|---|---|
| `list_sessions` | — | `Result<Vec<SessionMeta>, String>` | `spawn_blocking(scan_sessions)`. |
| `get_session_messages` | `providerId: String, sourcePath: String` | `Result<Vec<SessionMessage>, String>` | `spawn_blocking(load_messages)`. |
| `launch_session_terminal` | `command: String, cwd: Option<String>, custom_config: Option<String>` | `Result<bool, String>` | Reads `settings::get_preferred_terminal()`; maps `"iterm2"→"iterm"`, else passthrough, default `"terminal"`; calls `terminal::launch_terminal`; returns `true`. |
| `delete_session` | `providerId, sessionId, sourcePath: String` | `Result<bool, String>` | `spawn_blocking(delete_session)`. |
| `delete_sessions` | `items: Vec<DeleteSessionRequest>` | `Result<Vec<DeleteSessionOutcome>, String>` | `spawn_blocking(delete_sessions)`. |

Note the global-setting terminal name normalization (`iterm2` vs session `iterm`) is part of the contract — preferred terminal lives in app settings, not passed by the UI.

---

## 7. Behavior — session usage sync services

These are **not** Tauri commands here; they are library functions invoked by a sync scheduler / usage commands elsewhere. They import token usage into `usage_logs`. All share:
- `get_sync_state(db, file_path) -> (last_modified, last_offset)` (default `(0,0)`).
- `metadata_modified_nanos(&Metadata) -> i64` (mtime ns).
- `update_sync_state(db, file_path, last_modified, last_offset)` → `INSERT OR REPLACE` with `last_synced_at = now_secs`.
- `should_skip_session_insert(conn, request_id, &DedupKey)` and `find_model_pricing(conn, model)` from `services::usage_stats`.
- `CostCalculator::calculate(...)` / `calculate_for_app(app, usage, pricing, multiplier=Decimal(1))`.
- `crate::usage_events::notify_log_recorded()` fired when a new row is written (frontend live-refresh).
- Inserts into `usage_logs` with a session-specific `data_source`; gateway-captured rows use `gateway`, while imported historical rows may retain `proxy`.

### 7.1 `sync_claude_session_logs(db) -> Result<SessionSyncResult, AppError>` (`session_usage.rs`)
- Root `~/.claude/projects`. `collect_jsonl_files` (NON-recursive fixed depth): project dir `*.jsonl`; plus `SESSION_ID/subagents/*.jsonl`; plus `SESSION_ID/subagents/workflows/wf_*/*.jsonl`. (`journal.jsonl` collected but naturally yields 0 assistant rows.)
- `sync_single_file`: skip unchanged file (`file_modified <= last_modified`). Incremental from `last_offset` by line number. Extract `sessionId` from first line that has it. Only `type=="assistant"` lines with `message.id` and `message.usage`. Parse input/output/cache_read(`cache_read_input_tokens`)/cache_creation(`cache_creation_input_tokens`), `stop_reason`, `timestamp`.
- **Dedup by `message.id`**: replace if new has stop_reason and old doesn't; if equal stop_reason presence, keep larger `output_tokens`; else keep old.
- Insert each msg with **any billable token > 0** (input/output/cache_read/cache_creation). `request_id = SESSION_REQUEST_ID_PREFIX + message_id` (i.e. `"session:<id>"`). `INSERT OR IGNORE`. `provider_id="_session"`, `app_type="claude"`, `provider_type/data_source="session_log"`, `status_code=200`, `is_streaming=1`, `cost_multiplier="1.0"`. `created_at` = RFC3339 timestamp secs or now.
- `update_sync_state(file, file_modified, line_offset)`.
- `get_data_source_breakdown(db) -> Vec<DataSourceSummary>`: groups `usage_logs` by `COALESCE(data_source,'proxy')` with the effective usage-log filter; returns `(data_source, count, total_cost "{:.6}")`.

### 7.2 `sync_codex_usage(db)` (`session_usage_codex.rs`)
- Files: `sessions/` recursive (max depth 3) + flat `archived_sessions/*.jsonl`.
- Per file, incremental by line; fast pre-filter (substring tests) for `event_msg`/`turn_context`/`session_meta` and `token_count`. Parse:
  - `session_meta` → session_id (`session_id|sessionId|id`).
  - `turn_context` → model (`model` or `info.model`), normalized.
  - `event_msg` `type==token_count` with non-null `info`: model from `info.model|model_name|payload.model`; prefer `total_token_usage` (cumulative → delta via `compute_delta`, update `prev_total`) else `last_token_usage` (used directly). Clamp `cached_input = min(cached, input)`. Skip zero delta. `event_index += 1`.
  - **State recovery**: lines `<= last_offset` are still parsed (to rebuild `prev_total`/model/index) but NOT inserted.
- `request_id = "codex_session:{session}:{event_index}"`. `provider_id="_codex_session"`, `app_type/data_source/provider_type="codex"/"codex_session"`. `cache_creation_tokens=0`. `calculate_for_app("codex", …)`. `INSERT OR IGNORE`.
- `normalize_codex_model`: lowercase → strip `provider/` prefix → strip `-YYYY-MM-DD` (11 chars) → strip `-YYYYMMDD` (9 chars compact).

### 7.3 `sync_gemini_usage(db)` (`session_usage_gemini.rs`)
- Files: `tmp/<project>/chats/session-*.json`. Whole-file JSON parse (skip if mtime unchanged). Top-level `sessionId`.
- For each `messages[*]` `type=="gemini"` with object `tokens`: parse `input/output/cached/thoughts`. Skip all-zero. `output_tokens = output + thoughts` (thoughts billed as output). `request_id = "gemini_session:{session}:{messageId}"`.
- **UPSERT** (not INSERT OR IGNORE): `ON CONFLICT(request_id) DO UPDATE` of tokens/costs **only WHERE values differ** — because gemini files are re-read in full and may carry updated values. `changed = conn.changes() > 0` determines imported vs skipped and whether to notify. `provider_id="_gemini_session"`, app/source `gemini`/`gemini_session`. `update_sync_state` uses `gemini_msg_count` as the "offset".

### 7.4 `sync_opencode_usage(db)` (`session_usage_opencode.rs`)
- Source: `opencode.db` (RO). Considers **WAL** mtime: `file_modified = max(db_mtime_ns, db-wal_mtime_ns)`.
- `query_sessions`: `SELECT s.id, MAX(s.time_updated, COALESCE(MAX(m.time_updated), s.time_updated)) watermark … GROUP BY s.id ORDER BY watermark`. Per-session sync key `"{db_path}:{session_id}"`; skip if `watermark <= sess_last_modified`.
- `query_assistant_messages`: only `role=="assistant"` with `tokens`; messages lacking `time.completed` are **skipped and flag `has_incomplete_usage=true`** (can't backfill due to INSERT OR IGNORE). `parse_message_data`: tokens `input/output/reasoning/cache.read/cache.write`; skip all-zero; `cost` (f64), `modelID`, `time.created` (ms).
- Insert: `output_with_reasoning = output + reasoning`. `request_id = "opencode_session:{session}:{message}"`. If `cost > 0` → put entire amount in `total_cost`, components 0; else compute via pricing. `created_at = timestamp_ms/1000`. `provider_id="_opencode_session"`, source `opencode_session`. `INSERT OR IGNORE`.
- **Watermark advancement rules**: only advance per-session state when that session had **no error AND no incomplete usage**; only advance file-level state when the whole run had no errors (`has_sync_errors`), to guarantee retries.

---

## 8. External APIs / dependencies / OS integration

- **No network calls.** Pure local FS + SQLite.
- Crates: `serde`, `serde_json`, `rusqlite` (with `OpenFlags::SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`, `unchecked_transaction`), `chrono` (RFC3339 parsing), `regex` + `LazyLock` (codex UUID), `rust_decimal` (`Decimal`), `dirs` (home dir), `tempfile` (warp script + tests), `url` (warp deep-link), `log`.
- OS integration: macOS `osascript` (Terminal/iTerm), `open -na/-a` for GUI terminals, `sh -c` for custom; threads via `std::thread::scope`; `XDG_DATA_HOME` / `SHELL` env vars.
- DB: shared `Database` wrapper with `lock_conn!(db.conn)` macro; tables `usage_logs`, `session_log_sync`, plus provider-owned `opencode.db`/`state.db` opened read-only.

---

## 9. Rewrite notes (Rust + GPUI + axum)

- **Tauri commands → transport-agnostic functions.** `scan_sessions/load_messages/delete_session(s)/launch_terminal` are already plain functions; expose them as axum handlers (`GET /sessions`, `POST /sessions/messages`, `POST /sessions/delete`, `POST /sessions/launch`) and/or GPUI actions. Replace `tauri::async_runtime::spawn_blocking` with `tokio::task::spawn_blocking`. Preserve camelCase JSON via serde (already configured).
- **Thread-scoped scan** (`std::thread::scope`) works unchanged; or use `tokio::join!`/`rayon`. Preserve provider order and the descending-by-`last_active_at.or(created_at)` sort.
- **`usage_events::notify_log_recorded()`** is a Tauri event today. In GPUI, replace with a GPUI model update / channel; for axum, expose an SSE/websocket or have the UI re-poll. Keep the "only notify when a new row was actually written" semantics (INSERT OR IGNORE rowcount / gemini `changes()>0`).
- **Preferred-terminal lookup** (`settings::get_preferred_terminal`) must remain in the settings subsystem; keep the `iterm2 → iterm` mapping.
- **Path-traversal validation** in `delete_session_with_roots` and the SQLite db-path canonicalization checks are security boundaries — reproduce exactly (canonicalize + `starts_with` for files; canonicalize-equality for DB paths). The source_path is attacker-controllable input from the UI.
- **macOS-only terminal launch** — keep the `cfg!(target_os="macos")` guard; the warp launcher is `#[cfg(unix)]`. For other platforms the function must return the same error string.
- **SQLite read-only opening with `NO_MUTEX`** is important to avoid locking the running CLI's DB; opencode usage sync additionally must check the `-wal` mtime.
- **Incremental sync state** (`session_log_sync`) semantics differ per provider (line offset for claude/codex, message count for gemini, per-session watermark for opencode) — keep each provider's quirks, especially codex's "parse-but-don't-insert below last_offset" state recovery and opencode's two-level watermark retry guarantee.
- **No dialogs / updater / store** are used by this subsystem; nothing to port there.

---

## 10. Frontend / UI surface (consumer of these commands)

This subsystem has **no React screen of its own defined in the Rust files read here**; the spec inventories the command/data contract the UI binds to. The session-manager UI (a "Sessions" panel) consumes:
- `list_sessions` → renders a list/table of `SessionMeta` (title, summary, providerId badge, projectDir, relative time from `lastActiveAt|createdAt`).
- `get_session_messages` → a transcript/detail view rendering `SessionMessage[]` with role styling (`user`/`assistant`/`tool`).
- `launch_session_terminal` → "Resume" button (enabled only when `resumeCommand` present); passes `resumeCommand` as `command`, `projectDir` as `cwd`.
- `delete_session` / `delete_sessions` → single + multi-select delete with per-item `DeleteSessionOutcome` feedback.
- Usage data-source breakdown (`get_data_source_breakdown`) feeds the usage UI's "data source" chart (gateway vs session_log vs codex_session vs gemini_session vs opencode_session; legacy `proxy` remains readable).

i18n note: user-facing English strings produced by **this** layer are the error messages returned in `Result<_, String>` (e.g. "Resume command is empty", "Terminal resume is only supported on macOS", "Session source path is outside provider roots", "Session was not deleted", "Unsupported terminal target: …"). These should be moved to i18n keys (e.g. `sessions.error.*`, `sessions.terminal.*`) in the rewrite rather than hard-coded English. Provider display names (`claude/codex/gemini/hermes/openclaw/opencode`) and role labels (`user/assistant/tool`) are additional translatable UI strings owned by the frontend.
