# Subsystem Spec 10 — Prompt Management + Per-App Prompt Files

Faithful re-implementation spec for the **Prompt management** subsystem of cc-switch, for the Rust + GPUI + axum rewrite. This subsystem lets a user maintain a library of named "prompts" (markdown system-prompt / agent-instruction files) **per target app**, store them in the cc-switch SQLite DB, and toggle exactly one as "enabled". The enabled prompt's content is mirrored to that app's live on-disk instruction file (e.g. `~/.claude/CLAUDE.md`).

Source files (Tauri implementation):
- `src-tauri/src/prompt.rs` — `Prompt` struct.
- `src-tauri/src/prompt_files.rs` — per-app on-disk file path resolution.
- `src-tauri/src/services/prompt.rs` — `PromptService` business logic.
- `src-tauri/src/commands/prompt.rs` — Tauri command wrappers (6 commands).
- `src-tauri/src/database/dao/prompts.rs` — SQLite DAO (`get_prompts`, `save_prompt`, `delete_prompt`).
- `src-tauri/src/database/mod.rs` — `is_prompts_table_empty`.
- `src-tauri/src/database/schema.rs` — `prompts` table DDL.
- `src-tauri/src/database/migration.rs` — JSON→SQLite migration of prompts.
- `src-tauri/src/deeplink/prompt.rs` — import a prompt via `ccswitch://` deep link.
- Frontend: `src/lib/api/prompts.ts`, `src/hooks/usePromptActions.ts`, `src/components/prompts/*`, `src/components/deeplink/PromptConfirmation.tsx`.

---

## 1. Data Structures

### 1.1 `Prompt` (`src-tauri/src/prompt.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}
```

Field-by-field:
| Rust field | JSON key (serde) | Type | Attributes |
|---|---|---|---|
| `id` | `id` | `String` | — (primary key part) |
| `name` | `name` | `String` | — |
| `content` | `content` | `String` | the markdown body written to the live file |
| `description` | `description` | `Option<String>` | `skip_serializing_if = "Option::is_none"` |
| `enabled` | `enabled` | `bool` | `#[serde(default)]` (defaults to `false` on deserialize) |
| `created_at` | `createdAt` | `Option<i64>` | rename + skip-if-none; Unix **seconds** (sometimes millis — see notes) |
| `updated_at` | `updatedAt` | `Option<i64>` | rename + skip-if-none; Unix **seconds** |

Notes on timestamps:
- `PromptService` writes `created_at`/`updated_at` in **Unix seconds** (`get_unix_timestamp()` → `as_secs()`).
- The frontend form also uses **seconds** (`Math.floor(Date.now()/1000)`).
- The deep-link importer (`deeplink/prompt.rs`) writes **milliseconds** (`chrono::Utc::now().timestamp_millis()`). This inconsistency exists in the original and is only used for ordering tie-breaks; preserve as-is unless explicitly normalizing.

### 1.2 `AppType` (`src-tauri/src/app_config.rs`)

The subsystem is parameterized by app. Enum with serde `rename_all = "lowercase"`:

```rust
pub enum AppType { Claude, ClaudeDesktop, Codex, Gemini, OpenCode, OpenClaw, Hermes }
```
- `ClaudeDesktop` has serde `rename = "claude-desktop"`, `alias = "claude_desktop"`, `alias = "claudeDesktop"`.
- `as_str()` → `"claude"`, `"claude-desktop"`, `"codex"`, `"gemini"`, `"opencode"`, `"openclaw"`, `"hermes"`.
- `FromStr` lowercases+trims input; accepts `"claude-desktop" | "claude_desktop" | "claudedesktop"`; unknown → `AppError::localized("unsupported_app", …)`.
- The `app_type` string stored in the `prompts` table is `AppType::as_str()`.

### 1.3 Database row (table `prompts`)

DDL (`schema.rs`):
```sql
CREATE TABLE IF NOT EXISTS prompts (
    id TEXT NOT NULL,
    app_type TEXT NOT NULL,
    name TEXT NOT NULL,
    content TEXT NOT NULL,
    description TEXT,
    enabled BOOLEAN NOT NULL DEFAULT 1,
    created_at INTEGER,
    updated_at INTEGER,
    PRIMARY KEY (id, app_type)
);
```
- **Composite primary key `(id, app_type)`** — the same `id` can exist for different apps; uniqueness is per app.
- `enabled` column default is `1` in DDL, but every insert from code sets it explicitly.
- No foreign keys, no separate index (ordering done in query).

### 1.4 In-memory collection type

`get_prompts` returns `IndexMap<String, Prompt>` (from the `indexmap` crate) keyed by prompt `id`, **insertion-ordered** by the SQL `ORDER BY created_at ASC, id ASC`. The frontend receives this as a JS object `Record<string, Prompt>` (insertion order preserved by serde_json object emission). The GPUI rewrite must preserve this ordering (use an ordered map; do NOT use `HashMap`).

### 1.5 Legacy JSON migration shape (`migration.rs`)

When migrating from the old `config.json` (`MultiAppConfig`), prompts lived under `config.prompts.{claude,codex,gemini}.prompts` as a `HashMap<String, Prompt>`. Only `claude`, `codex`, `gemini` were migrated (opencode/openclaw/hermes had no legacy JSON prompts). Each entry is INSERT-OR-REPLACEd into the table with the loop key as `id`.

---

## 2. Files & Formats

### 2.1 Live per-app prompt file path (`prompt_files.rs::prompt_file_path`)

```rust
pub fn prompt_file_path(app: &AppType) -> Result<PathBuf, AppError>
```
- `ClaudeDesktop` → **error** `AppError::localized("claude_desktop.prompts_unsupported", "Claude Desktop 暂不支持 Prompts", "Claude Desktop does not support Prompts")`. Claude Desktop has no prompt file at all.
- Base directory per app:
  - `Claude` → parent of `get_claude_settings_path()`, fallback `home/.claude`.
  - `Codex` → parent of `get_codex_auth_path()`, fallback `home/.codex`.
  - `Gemini` → `get_gemini_dir()`.
  - `OpenCode` → `get_opencode_dir()`.
  - `OpenClaw` → `get_openclaw_dir()`.
  - `Hermes` → `get_hermes_dir()`.
- Filename per app:
  - `Claude` → `CLAUDE.md`
  - `Codex` → `AGENTS.md`
  - `Gemini` → `GEMINI.md`
  - `OpenCode` | `OpenClaw` | `Hermes` → `AGENTS.md`
- Returns `base_dir.join(filename)`.

`get_base_dir_with_fallback(primary_path, fallback_dir)`:
- Take `primary_path.parent()`; if none, fall back to `dirs::home_dir().join(fallback_dir)`; if still none → `AppError::localized("home_dir_not_found", …)`.

### 2.2 Concrete default paths (platform variants)

Directory resolution helpers (all support a per-app override dir from settings — see §5):

| App | Default base dir (macOS/Linux) | Live prompt file | Override source |
|---|---|---|---|
| Claude | `~/.claude/` (parent of `settings.json`) | `~/.claude/CLAUDE.md` | `get_claude_override_dir()` (via `get_claude_config_dir`) |
| Codex | `~/.codex/` (parent of `auth.json`) | `~/.codex/AGENTS.md` | codex override dir |
| Gemini | `~/.gemini/` | `~/.gemini/GEMINI.md` | `get_gemini_override_dir()` |
| OpenCode | `~/.config/opencode/` | `~/.config/opencode/AGENTS.md` | `get_opencode_override_dir()` |
| OpenClaw | `~/.openclaw/` | `~/.openclaw/AGENTS.md` | `get_openclaw_override_dir()` |
| Hermes | `~/.hermes/` | `~/.hermes/AGENTS.md` | `get_hermes_override_dir()` |
| Claude Desktop | N/A — unsupported | — | — |

Windows: `~` resolves to the user profile via `dirs::home_dir()`. Note: the cc-switch app-config dir (`~/.cc-switch/`) has a Windows-only legacy fallback (v3.10.3 `$HOME`), but the *prompt* live files use the app dirs above, which use `get_home_dir()` (not `HOME` env). The opencode default `~/.config/opencode` is the same on all OSes (not XDG-resolved here).

### 2.3 Format of the live file

- **Plain text / markdown.** The file is the verbatim `prompt.content` string. No JSON/TOML/YAML wrapping. Written via `write_text_file` (atomic: write to `{name}.tmp.{nanos}` then rename; creates parent dirs with `create_dir_all`).
- "Disable all" writes an **empty string** (truncates the file), not deletion.

### 2.4 cc-switch database file

- `~/.cc-switch/cc-switch.db` (SQLite). Prompts live in the `prompts` table (§1.3). This is the source of truth; the live files are a derived projection of the single enabled prompt.

---

## 3. Behavior

### 3.1 Tauri commands (`commands/prompt.rs`)

All are `#[tauri::command] async`, take `state: State<'_, AppState>` (except the file-content reader), parse `app: String` via `AppType::from_str`, and map `AppError` → `String` (`.to_string()`). Registered in `lib.rs` invoke_handler.

| Command (snake_case) | Params | Returns | Delegates to |
|---|---|---|---|
| `get_prompts` | `app: String` | `Result<IndexMap<String, Prompt>, String>` | `PromptService::get_prompts` |
| `upsert_prompt` | `app: String, id: String, prompt: Prompt` | `Result<(), String>` | `PromptService::upsert_prompt` |
| `delete_prompt` | `app: String, id: String` | `Result<(), String>` | `PromptService::delete_prompt` |
| `enable_prompt` | `app: String, id: String` | `Result<(), String>` | `PromptService::enable_prompt` |
| `import_prompt_from_file` | `app: String` | `Result<String, String>` (new id) | `PromptService::import_from_file` |
| `get_current_prompt_file_content` | `app: String` | `Result<Option<String>, String>` | `PromptService::get_current_file_content` (no `state`) |

Frontend invoke arg names (camelCase keys passed to `invoke`): `app`, `id`, `prompt`. The `prompt` object uses JSON keys `createdAt`/`updatedAt` (matching serde rename).

### 3.2 `PromptService` (`services/prompt.rs`)

Helper: `get_unix_timestamp() -> Result<i64, AppError>` — `SystemTime::now().duration_since(UNIX_EPOCH).as_secs() as i64`; error → `AppError::Message("Failed to get system time: …")`.

#### `get_prompts(state, app: AppType) -> Result<IndexMap<String, Prompt>, AppError>`
- Direct pass-through to `state.db.get_prompts(app.as_str())`. (DAO query: `SELECT … FROM prompts WHERE app_type = ?1 ORDER BY created_at ASC, id ASC`.)

#### `upsert_prompt(state, app: AppType, _id: &str, prompt: Prompt) -> Result<(), AppError>`
- `_id` param is **ignored**; the prompt's own `prompt.id` is the key used by `save_prompt`.
- `is_enabled = prompt.enabled`.
- Always `state.db.save_prompt(app.as_str(), &prompt)` (INSERT OR REPLACE).
- If `is_enabled`: resolve `prompt_file_path(&app)?` and `write_text_file(target, &prompt.content)` — mirror content to the live file.
- Else (disabled): reload all prompts; if **none** are enabled, and the live file `exists()`, overwrite it with `""` (clear). If the file doesn't exist, do nothing. If some other prompt is still enabled, leave the file untouched.
- **Note:** upsert does NOT enforce single-enabled invariant by itself. Saving a prompt with `enabled=true` will mirror its content but won't disable others. The single-enabled invariant is enforced by `enable_prompt` and by the frontend's optimistic `toggleEnabled` (which calls `enable_prompt` for enabling). The disable path of `toggleEnabled` calls `upsert_prompt` with `enabled=false`.
- Edge: if `app == ClaudeDesktop`, `prompt_file_path` errors; but DB save already happened. (Original behavior — Claude Desktop prompts shouldn't reach here from the UI since it has no prompt panel.)

#### `delete_prompt(state, app: AppType, id: &str) -> Result<(), AppError>`
- Load prompts; if `prompts[id].enabled` is true → return `AppError::InvalidInput("无法删除已启用的提示词")` ("Cannot delete an enabled prompt"). **An enabled prompt cannot be deleted**; user must disable/switch first.
- Otherwise `state.db.delete_prompt(app.as_str(), id)` (`DELETE … WHERE id=?1 AND app_type=?2`). Deleting a non-existent id is a silent no-op (SQL affects 0 rows, returns Ok).

#### `enable_prompt(state, app: AppType, id: &str) -> Result<(), AppError>`
The most intricate function. Two phases:

**Phase A — back-fill / backup the current live file content** (so manual edits to the live `CLAUDE.md` aren't lost):
1. `target = prompt_file_path(&app)?`.
2. If `target.exists()` and `read_to_string` succeeds and the content is **not blank** (`trim().is_empty()` false):
   - Load all prompts.
   - If there is a currently-enabled prompt: set `enabled_prompt.content = live_content`, `updated_at = now (seconds)`, log "回填 live 提示词内容到已启用项: {id}", and `save_prompt`. (The live edits are absorbed into the currently-enabled prompt.)
   - Else (no enabled prompt): de-dupe — only create a backup if no existing prompt's `content.trim()` equals `live_content.trim()`. If unique, create a backup `Prompt`:
     - `id = format!("backup-{timestamp}")` (timestamp = seconds, computed inline with `unwrap_or_default`).
     - `name = format!("原始提示词 {}", chrono::Local::now().format("%Y-%m-%d %H:%M"))` ("Original Prompt {datetime}").
     - `content = live_content`, `description = Some("自动备份的原始提示词")` ("Auto-backed-up original prompt"), `enabled = false`, `created_at`/`updated_at = timestamp`.
     - `save_prompt`.
   - (Read errors or blank content silently skip Phase A.)

**Phase B — enable the target and write live file:**
3. Reload all prompts into a mutable map.
4. Set every prompt's `enabled = false` (in memory).
5. If `prompts[id]` exists: set its `enabled = true`; `write_text_file(target, &prompt.content)` (atomic write of live file); `save_prompt` for it.
6. Else → return `AppError::InvalidInput("提示词 {id} 不存在")` ("Prompt {id} does not exist").
7. Loop over **all** prompts and `save_prompt` each (persists the `enabled=false` for the others). This guarantees the single-enabled invariant.

Ordering matters: the target's `enabled=true` save happens in step 5; the bulk save in step 7 re-saves it too (still true since the in-memory map has it true). Net result: exactly `id` is enabled in DB and the live file equals its content.

#### `import_from_file(state, app: AppType) -> Result<String, AppError>`
- `file_path = prompt_file_path(&app)?`. If not exists → `AppError::Message("提示词文件不存在")` ("Prompt file does not exist").
- `read_to_string` (io error → `AppError::io(path, e)`).
- `timestamp = get_unix_timestamp()?` (seconds); `id = format!("imported-{timestamp}")`.
- Build `Prompt`: `name = format!("导入的提示词 {}", chrono::Local::now().format("%Y-%m-%d %H:%M"))` ("Imported Prompt {dt}"), `content`, `description = Some("从现有配置文件导入")` ("Imported from existing config file"), `enabled = false`, timestamps = timestamp.
- `upsert_prompt(state, app, &id, prompt)` → since disabled, no live-file mutation (unless no enabled prompt exists, in which case the file may be cleared if it exists — but the file just got read and is non-empty, so the disable branch will see this newly-saved prompt is the only one and none enabled → clears the live file to `""`). **Edge to preserve:** importing from a file with no other prompts and none enabled will blank the live file after import. (This is original behavior; the imported content is safely in the DB.)
- Returns the new `id`.

#### `get_current_file_content(app: AppType) -> Result<Option<String>, AppError>`
- No `state`. `file_path = prompt_file_path(&app)?`. If not exists → `Ok(None)`. Else `read_to_string` → `Ok(Some(content))`; io error → `AppError::io`.

#### `import_from_file_on_first_launch(state, app: AppType) -> Result<usize, AppError>`
First-run auto-import (called from `lib.rs` setup):
- Idempotency: if `state.db.get_prompts(app)` is non-empty → `Ok(0)`.
- `file_path = prompt_file_path(&app)?`; if not exists → `Ok(0)`.
- `read_to_string`: on error, log warn and `Ok(0)` (does not propagate).
- If content `trim().is_empty()` → `Ok(0)`.
- Build `Prompt`: `id = format!("auto-imported-{timestamp}")`, `name = format!("Auto-imported Prompt {}", Local::now…%Y-%m-%d %H:%M)`, `description = Some("Automatically imported on first launch")`, **`enabled = true`** (auto-enabled on first launch), timestamps = seconds.
- `state.db.save_prompt(app, &prompt)` directly (NOT via `upsert_prompt`, so it does NOT re-write the live file — content already came from it). Returns `Ok(1)`.

First-launch caller (`lib.rs` setup, ~line 758): only runs when `db.is_prompts_table_empty()` is true; iterates `[Claude, Codex, Gemini, OpenCode, OpenClaw, Hermes]` (NOT ClaudeDesktop), logging per-app success/skip/failure. A per-app failure is logged (warn) and does not abort startup.

### 3.3 DAO (`database/dao/prompts.rs`)

- `get_prompts(&self, app_type: &str) -> Result<IndexMap<String, Prompt>, AppError>`: `SELECT id, name, content, description, enabled, created_at, updated_at FROM prompts WHERE app_type = ?1 ORDER BY created_at ASC, id ASC`. Maps each row → `(id, Prompt)`; collects into `IndexMap`. `enabled` read as `bool` (SQLite int 0/1). Errors → `AppError::Database(e.to_string())`.
- `save_prompt(&self, app_type, &Prompt)`: `INSERT OR REPLACE INTO prompts (id, app_type, name, content, description, enabled, created_at, updated_at) VALUES (?1..?8)`. Upsert keyed by `(id, app_type)` PK.
- `delete_prompt(&self, app_type, id)`: `DELETE FROM prompts WHERE id = ?1 AND app_type = ?2`.
- `is_prompts_table_empty(&self) -> Result<bool>` (in `database/mod.rs`): `SELECT COUNT(*) FROM prompts` == 0.
- All use `lock_conn!(self.conn)` (a `Mutex<Connection>` wrapper); the rewrite must serialize DB access (rusqlite `Connection` is not `Sync`).

### 3.4 Deep-link prompt import (`deeplink/prompt.rs`)

`import_prompt_from_deeplink(state, request: DeepLinkImportRequest) -> Result<String, AppError>`:
- Validate `request.resource == "prompt"` else `InvalidInput`.
- Require `request.app` (else "Missing 'app' field"), `request.name` (else "Missing 'name' field").
- `AppType::from_str(app_str)` → `InvalidInput("Invalid app type: …")` on failure.
- Require `request.content` (base64) → `decode_base64_param("content", …)` → `String::from_utf8` (invalid UTF-8 → `InvalidInput`).
- `timestamp = chrono::Utc::now().timestamp_millis()` (millis here).
- `sanitized_name` = name filtered to `[alphanumeric, '-', '_']`, lowercased. `id = format!("{sanitized_name}-{timestamp}")`.
- `should_enable = request.enabled.unwrap_or(false)`.
- Build `Prompt` with `enabled = false` (always saved disabled first), `description = request.description`, timestamps = millis.
- `PromptService::upsert_prompt(state, app, &id, prompt)`.
- If `should_enable`: `PromptService::enable_prompt(state, app, &id)` (disables others, writes live file).
- Returns `id`.

URL shape (for the deep-link subsystem, included for completeness): `ccswitch://import?resource=prompt&app=<app>&name=<name>&content=<base64>&description=<desc>&enabled=<bool>`. After a successful import the frontend dispatches a DOM `CustomEvent("prompt-imported", { detail: { app } })` to trigger a reload (see §6).

---

## 4. External APIs / Dependencies

- **No HTTP / network calls.** Entirely local filesystem + SQLite.
- Crates: `serde`/`serde_json` (serialization), `indexmap` (`IndexMap`, ordered), `rusqlite` (SQLite via `params!`), `chrono` (`Local`/`Utc` for human-readable names + millis timestamps), `dirs` (`home_dir`), `log`, `std::fs`/`std::time`.
- OS integration: filesystem read/write of per-app instruction files in the user home dir; atomic write (temp file + rename). No keychain, no process spawning.
- Tauri-specific: `tauri::command`, `tauri::State<AppState>` (DI of the DB + config), the invoke IPC bridge, the deep-link plugin (`ccswitch://`) feeding `import_prompt_from_deeplink`.

---

## 5. Rewrite Notes (Rust + GPUI + axum)

- **DB/store** (`AppState`): keep the `Mutex<rusqlite::Connection>` pattern or move to an async pool; the three DAO methods + `is_prompts_table_empty` port verbatim. Preserve composite PK `(id, app_type)` and `ORDER BY created_at ASC, id ASC`. Keep `IndexMap` ordering in the UI model.
- **Commands → in-process services or axum handlers.** Two surfaces are possible:
  1. GPUI UI calls `PromptService` directly (no IPC). Simplest; recommended.
  2. If the axum layer exposes an HTTP API (e.g. for the proxy/companion), mirror the 6 commands as JSON endpoints. Suggested mapping: `GET /prompts?app=`, `PUT /prompts` (body `{app,id,prompt}`), `DELETE /prompts/{id}?app=`, `POST /prompts/{id}/enable?app=`, `POST /prompts/import?app=`, `GET /prompts/file?app=`. Keep camelCase JSON (`createdAt`/`updatedAt`) for compatibility if any external client exists; otherwise keep serde renames anyway since the `Prompt` struct uses them.
- **Path/override resolution**: re-implement `prompt_file_path` + all `get_*_dir`/`get_*_override_dir` helpers. The override dirs come from app settings (the Tauri Store / `~/.cc-switch` settings). Whatever settings mechanism the rewrite chooses (plain JSON file instead of `tauri-plugin-store`) must still feed these overrides, or prompt files will write to the wrong place.
- **Atomic write**: port `write_text_file`/`atomic_write` (temp `{name}.tmp.{nanos}` then `fs::rename`, `create_dir_all` parent). GPUI has no equivalent; just use `std::fs`.
- **Timestamps**: replicate the seconds-vs-millis split exactly (service = seconds, deeplink = millis) to avoid changing ordering/ids, OR normalize to millis everywhere (a deliberate decision — note it).
- **Localized errors**: `AppError::localized(key, zh, en)` carries an i18n key + zh/en messages. Reproduce this dual-message error type so the GPUI UI can localize. Key strings to preserve: `claude_desktop.prompts_unsupported`, `home_dir_not_found`, `unsupported_app`. Plain-message errors (`"无法删除已启用的提示词"`, `"提示词 {id} 不存在"`, `"提示词文件不存在"`) are surfaced raw — consider converting to localized keys in the rewrite.
- **Deep-link event**: Tauri delivers `ccswitch://` URLs to the backend; the frontend learns of a completed import via a DOM `CustomEvent("prompt-imported")`. In GPUI there is no DOM — replace with a model/observer notification: after `import_prompt_from_deeplink`, emit an app-level event so the open Prompt panel (if its `appId` matches) reloads. Single-instance + URL-scheme registration must be reproduced at the OS level (the original explicitly re-registers the scheme on Linux/Windows-debug).
- **First-launch import**: reproduce in app startup after DB init, guarded by `is_prompts_table_empty()`, iterating the 6 supported apps (no Claude Desktop), non-fatal on per-app error.
- **Single-enabled invariant** lives in `enable_prompt` (bulk disable + save), not in a DB constraint. Keep it; the UI's optimistic toggle relies on it.
- **`upsert_prompt` ignores its `_id` arg** — the prompt's own `id` is authoritative. Don't "fix" this; the deep-link path and the form both rely on `prompt.id`.
- **Markdown editor**: the form uses a markdown editor with `minHeight` and dark-mode awareness. GPUI needs an equivalent multi-line text editor; markdown preview is optional (the original editor `MarkdownEditor` provides editing; content is stored raw).

---

## 6. Frontend: Screens, Controls, User Flows

The Prompt subsystem is one **view** within the main window (`currentView === "prompts"`), shown for the currently selected "shared feature app" (`sharedFeatureApp`). Claude Desktop has no prompt view.

### 6.1 Screens / panels / dialogs

1. **Prompt Panel** (`components/prompts/PromptPanel.tsx`) — the main list view.
   - Header strip (glass card): summary line `prompts.count` ("{{count}} prompts") · either `prompts.enabledName` ("Enabled: {{name}}") or `prompts.noneEnabled` ("No prompt enabled").
   - Body states:
     - Loading → `prompts.loading` ("Loading...").
     - Empty (no prompts) → FileText icon + `prompts.empty` ("No prompts yet") + `prompts.emptyDescription`.
     - List → one **PromptListItem** per prompt (insertion order).
   - Imperative handle: `openAdd()` (exposed via ref) opens the add form. Triggered by the toolbar **Add** button (`prompts.add`, Plus icon) shown only when `currentView==='prompts'` (`App.tsx` ~line 1244).
   - Reloads on mount/`open`, and on `window` `CustomEvent("prompt-imported")` when `detail.app === appId`.
   - Hosts the `ConfirmDialog` for deletes.

2. **PromptListItem** (`components/prompts/PromptListItem.tsx`) — a 64px row:
   - `PromptToggle` (green/gray switch) → `onToggle(id, newEnabled)`.
   - Name (bold) + optional description (truncated).
   - Edit button (Edit3 icon, `common.edit` title) → `onEdit(id)`.
   - Delete button (Trash2 icon, red hover, `common.delete` title) → `onDelete(id)`.

3. **PromptToggle** (`components/prompts/PromptToggle.tsx`) — `role="switch"`, `aria-checked`; emerald when enabled, gray when disabled; optional `disabled` (opacity + not-allowed cursor).

4. **PromptFormPanel** (`components/prompts/PromptFormPanel.tsx`) — full-screen add/edit panel (used by PromptPanel). Controls:
   - Title: `prompts.addTitle` / `prompts.editTitle` ("Add/Edit {{appName}} Prompt").
   - **Name** input (`prompts.name`, placeholder `prompts.namePlaceholder`) — required (Save disabled if blank).
   - **Description** input (`prompts.description`, placeholder `prompts.descriptionPlaceholder`) — optional.
   - **Content** `MarkdownEditor` (`prompts.content`, placeholder `prompts.contentPlaceholder` with `{{filename}}` interpolated, dark-mode aware, `minHeight="167px"`).
   - Footer Save button: `common.saving`/`common.save`.
   - On save: `id = editingId || prompt-${Date.now()}`; timestamp = `Math.floor(Date.now()/1000)`; `enabled = initialData?.enabled || false`; `createdAt = initialData?.createdAt || ts`; `updatedAt = ts`; content/name/description trimmed (description → `undefined` if empty). Calls `onSave(id, prompt)` → `usePromptActions.savePrompt` → `upsert_prompt`.
   - `filenameMap`: claude/claude-desktop→`CLAUDE.md`, codex→`AGENTS.md`, gemini→`GEMINI.md`, opencode/openclaw/hermes→`AGENTS.md`.

5. **PromptFormModal** (`components/prompts/PromptFormModal.tsx`) — a dialog-based variant of the form (same fields/logic, Dialog + DialogFooter with Cancel + Save, `minHeight="300px"`). Its `filenameMap` excludes `openclaw`. Currently the panel uses `PromptFormPanel`; the modal is an alternate component (replicate both or consolidate into one GPUI form).

6. **ConfirmDialog** (delete confirmation) — `prompts.confirm.deleteTitle` ("Confirm Delete") + `prompts.confirm.deleteMessage` ("Are you sure you want to delete prompt \"{{name}}\"?"). Confirm → `deletePrompt(id)`.

7. **PromptConfirmation** (`components/deeplink/PromptConfirmation.tsx`) — read-only preview inside the **DeepLinkImportDialog** when a `ccswitch://` prompt import arrives. Shows: title `deeplink.prompt.title` ("Import System Prompt"); App (`deeplink.prompt.app`, capitalized `request.app`); Name (`deeplink.prompt.name`); optional Description (`deeplink.prompt.description`); Content preview (`deeplink.prompt.contentPreview`, base64-decoded, truncated to 500 chars with "…"); if `request.enabled`, a yellow warning `deeplink.prompt.enabledWarning`. Confirm/cancel handled by the parent DeepLinkImportDialog, which on success dispatches `CustomEvent("prompt-imported", {detail:{app}})`.

### 6.2 Hook (`usePromptActions.ts`)

State: `prompts: Record<string, Prompt>`, `loading: boolean`, `currentFileContent: string | null`. Actions (all toast on success/failure):
- `reload()` — `getPrompts` + `getCurrentFileContent` (the latter failure → `currentFileContent=null`, no toast). On list failure toast `prompts.loadFailed`.
- `savePrompt(id, prompt)` — `upsertPrompt` → reload → toast `prompts.saveSuccess`/`prompts.saveFailed`.
- `deletePrompt(id)` — `deletePrompt` → reload → `prompts.deleteSuccess`/`prompts.deleteFailed`.
- `enablePrompt(id)` — `enablePrompt` → reload → `prompts.enableSuccess`/`prompts.enableFailed`.
- `toggleEnabled(id, enabled)` — optimistic: when enabling, locally set only `id` enabled (others false); when disabling, set `id` false. Then: enable → `enablePrompt` (toast `prompts.enableSuccess`); disable → `upsertPrompt(id, {...prompt, enabled:false})` (toast `prompts.disableSuccess`); reload after. On error: rollback to previous map + toast `prompts.enableFailed`/`prompts.disableFailed`.
- `importFromFile()` — `importFromFile` → reload → toast `prompts.importSuccess`/`prompts.importFailed`; returns new id.

### 6.3 Core user flows

1. **Add prompt**: toolbar Add → form → fill name(+desc)+content → Save → `upsert_prompt` (disabled by default; live file cleared only if no enabled prompt exists).
2. **Edit prompt**: row Edit → form prefilled → Save → `upsert_prompt`. If the edited prompt was enabled, its content re-mirrors to the live file.
3. **Enable / switch**: row toggle ON → `enable_prompt`: back-fills/baks current live file, disables all others, enables this one, writes live file. Only one can be enabled.
4. **Disable**: row toggle OFF → `upsert_prompt(enabled:false)`: if no prompt remains enabled, the live file is cleared to empty.
5. **Delete**: row Delete → confirm → `delete_prompt`. Blocked (backend error toast) if the prompt is enabled.
6. **Import from existing file**: `import_prompt_from_file` reads the live `*.md` and creates a disabled "Imported Prompt …". (The `prompts.import` / "Import Existing" string and `prompts.currentFile` exist for an import-affordance; wire a toolbar Import button to `importFromFile` in the rewrite.)
7. **First launch auto-import**: handled in backend startup (auto-enabled). No UI.
8. **Deep-link import**: `ccswitch://` → DeepLinkImportDialog shows PromptConfirmation → user confirms → backend `import_prompt_from_deeplink` (optionally enables) → frontend reload via `prompt-imported` event.

### 6.4 i18n keys to translate

Locale files: `src/i18n/locales/{en,zh,ja,zh-TW}.json`. Sections:

**`prompts.*`**: `manage`, `title` ({{appName}}), `claudeTitle`, `codexTitle`, `add`, `edit`, `addTitle` ({{appName}}), `editTitle` ({{appName}}), `import`, `count` ({{count}}), `enabled`, `enable`, `enabledName` ({{name}}), `noneEnabled`, `currentFile` ({{filename}}), `empty`, `emptyDescription`, `loading`, `name`, `namePlaceholder`, `description`, `descriptionPlaceholder`, `content`, `contentPlaceholder` ({{filename}}), `loadFailed`, `saveSuccess`, `saveFailed`, `deleteSuccess`, `deleteFailed`, `enableSuccess`, `enableFailed`, `disableSuccess`, `disableFailed`, `importSuccess`, `importFailed`, `confirm.deleteTitle`, `confirm.deleteMessage` ({{name}}).

**`deeplink.prompt.*`**: `title`, `app`, `name`, `description`, `contentPreview`, `enabledWarning`.

**Shared `common.*`** used here: `save`, `saving`, `cancel`, `edit`, `delete`. **`apps.<appId>`** (app display names) used for `{{appName}}` interpolation.

(All four locales must carry every key above. The English values are in §6.1/§6.3; mirror keys across zh, ja, zh-TW.)
