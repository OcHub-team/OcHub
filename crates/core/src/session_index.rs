//! Full-text search index over CLI session transcripts.
//!
//! [`session_manager`](crate::session_manager) can already list sessions and
//! load one transcript on demand, but it deliberately reads only the head and
//! tail of each file so that listing stays fast. That leaves the conversation
//! body unsearchable: the only way to answer *"which session was the one about
//! rate limiting?"* is to read every file, and the transcripts on a working
//! machine run to gigabytes.
//!
//! This module keeps a SQLite FTS5 index of the transcripts so that question
//! becomes a sub-millisecond query.
//!
//! # Design notes
//!
//! **Only `user` and `assistant` messages are indexed.** Every provider loader
//! normalises roles, so this is a single filter rather than per-provider
//! parsing. It is also what keeps the index small: on a 2.2 GB corpus the
//! conversational text is ~36 M characters, because the bulk of a session file
//! is tool output, reasoning traces and turn context. Indexing those would
//! multiply the index size and bury real hits under file dumps and diffs.
//!
//! **The index is a rebuildable cache, and lives in its own database.** It is
//! deliberately *not* part of `ochub.db`: it must never enter a user backup or
//! the schema migration chain, and every failure path here is allowed to answer
//! "delete it and rebuild", which would be unthinkable for the main database.
//!
//! **Search uses `LIKE`, not `MATCH`.** With the `trigram` tokenizer FTS5
//! optimises `LIKE '%needle%'` into an index lookup for needles of three or
//! more characters, and falls back to a scan below that. That single code path
//! handles CJK — which the default `unicode61` tokenizer cannot, since it
//! treats an entire run of Han characters as one token — as well as
//! case-insensitive ASCII substrings. Measured on a real 163 k message index:
//! 1–2 ms at three characters or more, and 40–85 ms for the one- and
//! two-character queries that have to scan. See [`SessionIndex::search`] for
//! why the indexed half of that query must not carry an `ESCAPE` clause.
//!
//! **Deletions are immediate; reclaiming disk space is deferred.** A session
//! whose file is gone must leave the index at once or search would offer hits
//! that open nothing. Returning the freed pages to the filesystem is the
//! expensive half, so it is batched behind [`SessionIndex::needs_maintenance`]
//! and run in interruptible slices by [`SessionIndex::maintain`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};

use crate::session_manager::{SessionMessage, SessionMeta};

/// Bumped whenever the schema or the tokenizer changes. A mismatch drops the
/// database and rebuilds from scratch rather than migrating: this is a cache,
/// and a rebuild costs seconds.
const INDEX_VERSION: i64 = 1;

/// Per-message cap. A single pasted file or base64 blob should not be able to
/// push megabytes into the index; the first 8 000 characters are more than
/// enough to make a message findable.
const BODY_MAX_CHARS: usize = 8_000;

/// Characters of context kept around a match for the result snippet.
const SNIPPET_CHARS: i64 = 200;
/// How far before the match the snippet starts, so the hit is not flush left.
const SNIPPET_LEAD: i64 = 40;

/// Free pages worth reclaiming on their own, regardless of age.
const RECLAIM_BYTES_THRESHOLD: u64 = 64 * 1024 * 1024;
/// Age at which any outstanding free space is worth a maintenance pass.
const RECLAIM_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Pages per `incremental_vacuum` slice. Small enough that the write lock is
/// never held long enough to stall a search.
const VACUUM_SLICE_PAGES: i64 = 256;
/// Page budget per FTS5 `merge` slice. `'optimize'` is deliberately not used:
/// it rebuilds the whole index and blocks for seconds on an index this size.
const MERGE_SLICE_PAGES: i64 = 16;

/// Roles worth indexing. Every provider loader normalises to these, plus
/// `tool` and `system`, which are excluded.
fn is_indexable_role(role: &str) -> bool {
    matches!(role, "user" | "assistant")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One matching session, with the earliest matching message in it.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    /// Index into the full [`session_manager::load_messages`] result, so the
    /// transcript view can scroll straight to this message. Counts every
    /// message, including the tool and system ones that are not indexed.
    pub ord: usize,
    pub role: String,
    /// Text around the match, already trimmed to a displayable length.
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub sessions: i64,
    pub messages: i64,
    /// Size of the database file, including its write-ahead log.
    pub bytes: u64,
    /// Free pages that a maintenance pass would return to the filesystem.
    pub reclaimable_bytes: u64,
    pub last_maintenance_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct MaintenanceOutcome {
    pub reclaimed_bytes: u64,
    /// True when the time budget ran out with work still outstanding.
    pub incomplete: bool,
}

pub struct SessionIndex {
    path: PathBuf,
    /// Separate connections so a long `sync` never blocks a search: WAL lets a
    /// reader proceed against its own snapshot while the writer holds the lock.
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
}

impl SessionIndex {
    /// Open (or create) the index at `~/.ochub/sessions-index.db`.
    pub fn open() -> Result<Self, String> {
        Self::open_at(default_index_path())
    }

    /// Open (or create) the index at an explicit path.
    ///
    /// A database that cannot be opened, is corrupt, or carries a different
    /// [`INDEX_VERSION`] is deleted and recreated. Every one of those is
    /// recoverable here precisely because the index holds no original data.
    pub fn open_at(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create index directory: {e}"))?;
        }

        let writer = match open_versioned(&path) {
            Ok(conn) => conn,
            Err(_) => {
                remove_database_files(&path);
                open_versioned(&path).map_err(|e| format!("Failed to create index: {e}"))?
            }
        };
        let reader = open_reader(&path).map_err(|e| format!("Failed to open index: {e}"))?;

        Ok(Self {
            path,
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bring the index in line with `sessions`, loading transcripts through
    /// [`session_manager::load_messages`].
    pub fn sync<F>(
        &self,
        sessions: &[SessionMeta],
        progress: F,
        cancel: &AtomicBool,
    ) -> Result<SyncOutcome, String>
    where
        F: FnMut(usize, usize),
    {
        self.sync_with(
            sessions,
            |meta| {
                let source = meta.source_path.as_deref().unwrap_or_default();
                crate::session_manager::load_messages(&meta.provider_id, source)
            },
            progress,
            cancel,
        )
    }

    /// [`SessionIndex::sync`] with an injectable transcript loader, so tests do
    /// not need real session files on disk.
    pub fn sync_with<L, F>(
        &self,
        sessions: &[SessionMeta],
        loader: L,
        mut progress: F,
        cancel: &AtomicBool,
    ) -> Result<SyncOutcome, String>
    where
        L: Fn(&SessionMeta) -> Result<Vec<SessionMessage>, String>,
        F: FnMut(usize, usize),
    {
        let mut conn = lock(&self.writer)?;
        let mut outcome = SyncOutcome::default();
        let total = sessions.len();
        let mut seen: HashSet<String> = HashSet::with_capacity(total);

        for (position, meta) in sessions.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                break;
            }
            let Some(source_path) = meta.source_path.as_deref() else {
                continue;
            };
            seen.insert(source_path.to_string());

            let (mtime, size) = source_version(source_path, meta);
            let current: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT mtime, size FROM doc WHERE source_path = ?1",
                    params![source_path],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| format!("Index lookup failed: {e}"))?;

            if current == Some((mtime, size)) {
                outcome.skipped += 1;
                progress(position + 1, total);
                continue;
            }

            // A failed load leaves any previous rows in place: a transient read
            // error should not silently empty a session out of search results.
            let messages = match loader(meta) {
                Ok(messages) => messages,
                Err(_) => {
                    progress(position + 1, total);
                    continue;
                }
            };

            let tx = conn
                .transaction()
                .map_err(|e| format!("Failed to open index transaction: {e}"))?;
            replace_document(&tx, meta, source_path, mtime, size, &messages)?;
            tx.commit()
                .map_err(|e| format!("Failed to commit index transaction: {e}"))?;

            outcome.indexed += 1;
            progress(position + 1, total);
        }

        // Reconcile only after a complete pass: a cancelled sync has not seen
        // every session, so anything missing from `seen` may simply be one it
        // never reached.
        if !outcome.cancelled {
            outcome.removed = prune_missing(&mut conn, &seen)?;
        }

        Ok(outcome)
    }

    /// Drop one session from the index. Called when a session is deleted
    /// through OcHub, so search never offers a hit that opens nothing.
    pub fn remove_session(&self, source_path: &str) -> Result<(), String> {
        let mut conn = lock(&self.writer)?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("Failed to open index transaction: {e}"))?;
        delete_document(&tx, source_path)?;
        tx.commit()
            .map_err(|e| format!("Failed to commit index transaction: {e}"))
    }

    /// Find every session containing `query`, with the earliest matching
    /// message in each.
    ///
    /// Returns an empty result for a blank query rather than matching
    /// everything: an empty search box means "no search", not "select all".
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = lock(&self.reader)?;
        let loose = format!("%{query}%");
        let exact = format!("%{}%", escape_like(query));

        // Two conditions, deliberately:
        //
        // `f.body LIKE ?1` is what the trigram index can serve, so it must
        // carry **no** `ESCAPE` clause — adding one silently drops FTS5 back to
        // a full scan (`INDEX 0:L0` becomes `INDEX 0:` in the query plan), which
        // on a 160 k message index is the difference between 1 ms and 80 ms. It
        // therefore leaves any `%` or `_` the user typed acting as wildcards,
        // making it a *superset* of the real answer.
        //
        // `hit.body LIKE ?3 ESCAPE '\'` then narrows that superset back to a
        // literal substring match. It runs only on rows the index already
        // returned, so its lack of index support costs nothing.
        //
        // The inner query picks one message per session — the earliest match,
        // by rowid — so a session with a hundred hits still yields one row.
        // `instr` gives the character offset of the match, which turns into a
        // bounded snippet without sending whole message bodies across. It agrees
        // with `?3` by construction: a row only survives if it literally
        // contains the query, so the snippet is always centred on a real match.
        let mut stmt = conn
            .prepare_cached(
                "SELECT d.provider_id, d.session_id, d.source_path, m.ord, m.role,
                        substr(m.body,
                               MAX(1, instr(lower(m.body), lower(?2)) - ?4),
                               ?5)
                 FROM msg m
                 JOIN doc d ON d.id = m.doc_id
                 WHERE m.id IN (
                     SELECT MIN(hit.id)
                     FROM msg_fts f
                     JOIN msg hit ON hit.id = f.rowid
                     WHERE f.body LIKE ?1
                       AND hit.body LIKE ?3 ESCAPE '\\'
                     GROUP BY hit.doc_id
                 )
                 LIMIT ?6",
            )
            .map_err(|e| format!("Failed to prepare search: {e}"))?;

        let rows = stmt
            .query_map(
                params![
                    loose,
                    query,
                    exact,
                    SNIPPET_LEAD,
                    SNIPPET_CHARS,
                    limit as i64
                ],
                |row| {
                    Ok(SearchHit {
                        provider_id: row.get(0)?,
                        session_id: row.get(1)?,
                        source_path: row.get(2)?,
                        ord: row.get::<_, i64>(3)?.max(0) as usize,
                        role: row.get(4)?,
                        snippet: row.get::<_, String>(5)?.trim().to_string(),
                    })
                },
            )
            .map_err(|e| format!("Search failed: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Search failed: {e}"))
    }

    pub fn stats(&self) -> Result<IndexStats, String> {
        let conn = lock(&self.reader)?;
        let sessions = conn
            .query_row("SELECT COUNT(*) FROM doc", [], |row| row.get(0))
            .unwrap_or(0);
        let messages = conn
            .query_row("SELECT COUNT(*) FROM msg", [], |row| row.get(0))
            .unwrap_or(0);
        let last_maintenance_at = read_meta(&conn, "last_maintenance_at")
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok());

        Ok(IndexStats {
            sessions,
            messages,
            bytes: database_bytes(&self.path),
            reclaimable_bytes: reclaimable_bytes(&conn).unwrap_or(0),
            last_maintenance_at,
        })
    }

    /// Whether a maintenance pass is worth running: either enough free space
    /// has accumulated to matter, or some has been outstanding for a week.
    pub fn needs_maintenance(&self) -> Result<bool, String> {
        let conn = lock(&self.reader)?;
        let reclaimable = reclaimable_bytes(&conn).unwrap_or(0);
        if reclaimable == 0 {
            return Ok(false);
        }
        if reclaimable >= RECLAIM_BYTES_THRESHOLD {
            return Ok(true);
        }
        let last = read_meta(&conn, "last_maintenance_at")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<i64>().ok());
        Ok(match last {
            Some(last) => now_ms().saturating_sub(last) >= RECLAIM_MAX_AGE_MS,
            None => true,
        })
    }

    /// Return free pages to the filesystem and merge FTS5 segments, in slices,
    /// stopping once `budget` is spent. Safe to call repeatedly: each call
    /// picks up where the last left off.
    pub fn maintain(&self, budget: Duration) -> Result<MaintenanceOutcome, String> {
        let conn = lock(&self.writer)?;
        let started = Instant::now();
        let before = database_bytes(&self.path);
        let mut outcome = MaintenanceOutcome::default();

        loop {
            if started.elapsed() >= budget {
                outcome.incomplete = true;
                break;
            }
            let free: i64 = conn
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))
                .unwrap_or(0);
            if free <= 0 {
                break;
            }
            conn.execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_SLICE_PAGES})"))
                .map_err(|e| format!("Failed to reclaim index space: {e}"))?;
        }

        if started.elapsed() < budget {
            // Bounded segment merge. Failure here is not fatal — a fragmented
            // index is slower, not wrong.
            let _ = conn.execute(
                "INSERT INTO msg_fts(msg_fts, rank) VALUES('merge', ?1)",
                params![MERGE_SLICE_PAGES],
            );
        } else {
            outcome.incomplete = true;
        }

        write_meta(&conn, "last_maintenance_at", &now_ms().to_string())?;
        outcome.reclaimed_bytes = before.saturating_sub(database_bytes(&self.path));
        Ok(outcome)
    }

    /// Delete the index from disk. Used when the feature is switched off and
    /// the user asks for the space back, and after a long disabled period.
    pub fn delete_files(path: &Path) {
        remove_database_files(path);
    }
}

/// How long a switched-off index is kept before being deleted outright.
///
/// Keeping the file means flipping the feature back on costs an incremental
/// sync rather than a full rebuild. Past a month that trade stops paying, and
/// the space is better returned.
pub const DISABLED_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn default_index_path() -> PathBuf {
    crate::paths::get_app_config_dir().join("sessions-index.db")
}

/// Whether an index file exists, without creating one.
pub fn index_exists() -> bool {
    default_index_path().exists()
}

/// Drop a session from the index after it has been deleted from disk.
///
/// A no-op when no index exists, so this can sit on the deletion path
/// unconditionally without bringing an index into being for users who have the
/// feature switched off. Failures are swallowed: a deletion must not fail
/// because a search cache could not be updated, and the next sync reconciles it.
pub fn forget_session(source_path: &str) {
    if !index_exists() {
        return;
    }
    if let Ok(index) = SessionIndex::open() {
        let _ = index.remove_session(source_path);
    }
}

/// Delete a long-disabled index, returning whether anything was removed.
///
/// Called at startup: the index is dead weight while the feature is off, but
/// deleting it the moment the switch flips would punish anyone toggling it.
pub fn expire_disabled_index(enabled: bool, disabled_at: Option<i64>) -> bool {
    if enabled || !index_exists() {
        return false;
    }
    let Some(disabled_at) = disabled_at else {
        return false;
    };
    if now_ms().saturating_sub(disabled_at) < DISABLED_RETENTION_MS {
        return false;
    }
    remove_database_files(&default_index_path());
    true
}

/// Open a connection and make sure it carries the current schema, creating it
/// on a fresh file. Returns an error for a version mismatch so the caller can
/// rebuild.
fn open_versioned(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;

    // `auto_vacuum` only takes effect while the database is still empty, so it
    // has to be set before the schema is created. Getting this wrong is
    // expensive to correct later: the only remedy is a full `VACUUM`, which
    // rewrites the entire file and needs twice its size in free disk.
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|e| e.to_string())?;
    // WAL + NORMAL trades durability of the last few transactions for far
    // cheaper commits during a rebuild. Losing them costs a re-scan of a few
    // sessions, which the mtime check performs anyway.
    conn.execute_batch("PRAGMA synchronous = NORMAL;")
        .map_err(|e| e.to_string())?;
    // Connections are opened per operation rather than held, so two of them can
    // briefly overlap — a search starting while a sync commits. Wait for the
    // lock instead of failing the operation outright.
    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(|e| e.to_string())?;

    let has_schema: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'doc'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .unwrap_or(false);

    if !has_schema {
        create_schema(&conn)?;
        return Ok(conn);
    }

    let version = read_meta(&conn, "index_version")?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(-1);
    if version != INDEX_VERSION {
        return Err(format!(
            "Index version mismatch: found {version}, expected {INDEX_VERSION}"
        ));
    }

    Ok(conn)
}

fn open_reader(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 5000;")
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn create_schema(conn: &Connection) -> Result<(), String> {
    // The `msg` triggers, rather than foreign keys, keep the external-content
    // FTS5 table in step. An external-content table does not follow its content
    // table automatically, and its `'delete'` command needs the *pre-delete*
    // body to remove the right terms — which is exactly what `old.body` gives.
    // Doing it here also sidesteps the question of whether `ON DELETE CASCADE`
    // fires triggers, which depends on `PRAGMA recursive_triggers`.
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE meta(
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE doc(
             id          INTEGER PRIMARY KEY,
             provider_id TEXT NOT NULL,
             session_id  TEXT NOT NULL,
             source_path TEXT NOT NULL UNIQUE,
             mtime       INTEGER NOT NULL,
             size        INTEGER NOT NULL,
             indexed_at  INTEGER NOT NULL
         );
         CREATE TABLE msg(
             id     INTEGER PRIMARY KEY,
             doc_id INTEGER NOT NULL,
             ord    INTEGER NOT NULL,
             role   TEXT NOT NULL,
             ts     INTEGER,
             body   TEXT NOT NULL
         );
         CREATE INDEX msg_doc_id ON msg(doc_id);
         CREATE VIRTUAL TABLE msg_fts USING fts5(
             body,
             content = 'msg',
             content_rowid = 'id',
             tokenize = 'trigram case_sensitive 0'
         );
         CREATE TRIGGER msg_after_insert AFTER INSERT ON msg BEGIN
             INSERT INTO msg_fts(rowid, body) VALUES (new.id, new.body);
         END;
         CREATE TRIGGER msg_after_delete AFTER DELETE ON msg BEGIN
             INSERT INTO msg_fts(msg_fts, rowid, body)
             VALUES ('delete', old.id, old.body);
         END;
         COMMIT;",
    )
    .map_err(|e| format!("Failed to create index schema: {e}"))?;

    write_meta(conn, "index_version", &INDEX_VERSION.to_string())?;
    Ok(())
}

fn replace_document(
    conn: &Connection,
    meta: &SessionMeta,
    source_path: &str,
    mtime: i64,
    size: i64,
    messages: &[SessionMessage],
) -> Result<(), String> {
    delete_document(conn, source_path)?;

    conn.execute(
        "INSERT INTO doc(provider_id, session_id, source_path, mtime, size, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            meta.provider_id,
            meta.session_id,
            source_path,
            mtime,
            size,
            now_ms()
        ],
    )
    .map_err(|e| format!("Failed to record indexed session: {e}"))?;
    let doc_id = conn.last_insert_rowid();

    let mut insert = conn
        .prepare_cached("INSERT INTO msg(doc_id, ord, role, ts, body) VALUES (?1, ?2, ?3, ?4, ?5)")
        .map_err(|e| format!("Failed to prepare message insert: {e}"))?;

    // `ord` counts every message, not just the indexed ones, so a hit can be
    // mapped back onto the position the transcript view will render it at.
    for (ord, message) in messages.iter().enumerate() {
        if !is_indexable_role(&message.role) {
            continue;
        }
        let body = truncate_chars(&message.content, BODY_MAX_CHARS);
        if body.trim().is_empty() {
            continue;
        }
        insert
            .execute(params![doc_id, ord as i64, message.role, message.ts, body])
            .map_err(|e| format!("Failed to index message: {e}"))?;
    }

    Ok(())
}

fn delete_document(conn: &Connection, source_path: &str) -> Result<(), String> {
    let doc_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM doc WHERE source_path = ?1",
            params![source_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Index lookup failed: {e}"))?;

    let Some(doc_id) = doc_id else {
        return Ok(());
    };

    // Deleting the `msg` rows fires the trigger that clears the FTS entries;
    // the `doc` row must go last so nothing dangles if this is interrupted.
    conn.execute("DELETE FROM msg WHERE doc_id = ?1", params![doc_id])
        .map_err(|e| format!("Failed to clear indexed messages: {e}"))?;
    conn.execute("DELETE FROM doc WHERE id = ?1", params![doc_id])
        .map_err(|e| format!("Failed to clear indexed session: {e}"))?;
    Ok(())
}

/// Remove documents whose session is no longer on disk — sessions deleted
/// outside OcHub, or from a machine whose files have since moved.
fn prune_missing(conn: &mut Connection, seen: &HashSet<String>) -> Result<usize, String> {
    let stale: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT source_path FROM doc")
            .map_err(|e| format!("Index lookup failed: {e}"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("Index lookup failed: {e}"))?;
        rows.filter_map(Result::ok)
            .filter(|path| !seen.contains(path))
            .collect()
    };

    if stale.is_empty() {
        return Ok(0);
    }

    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to open index transaction: {e}"))?;
    for source_path in &stale {
        delete_document(&tx, source_path)?;
    }
    tx.commit()
        .map_err(|e| format!("Failed to commit index transaction: {e}"))?;

    Ok(stale.len())
}

/// The (mtime, size) pair that decides whether a session needs reindexing.
///
/// SQLite-backed providers address rows inside a shared database with a
/// `sqlite:` prefix instead of a file, so there is nothing to stat; their last
/// activity timestamp stands in as the version.
fn source_version(source_path: &str, meta: &SessionMeta) -> (i64, i64) {
    if let Ok(metadata) = std::fs::metadata(source_path) {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|delta| delta.as_millis() as i64)
            .unwrap_or(0);
        return (mtime, metadata.len() as i64);
    }
    (meta.last_active_at.or(meta.created_at).unwrap_or(0), 0)
}

fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| format!("Index metadata read failed: {e}"))
}

fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(|e| format!("Index metadata write failed: {e}"))?;
    Ok(())
}

fn reclaimable_bytes(conn: &Connection) -> Result<u64, String> {
    let free: i64 = conn
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    Ok((free.max(0) as u64) * (page_size.max(0) as u64))
}

/// Size on disk, counting the write-ahead log: right after a large sync most
/// of the growth is still in the WAL, and reporting only the main file would
/// understate what the index costs.
fn database_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        total += std::fs::metadata(PathBuf::from(sidecar))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    total
}

fn remove_database_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
}

/// Neutralise `LIKE` wildcards in user input, paired with `ESCAPE '\'` on the
/// narrowing half of the search. Without it, typing `%` matches every session
/// and `_` silently widens the search — `rate_limit` would also find
/// `rate-limit`.
fn escape_like(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for ch in query.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => text[..byte_index].to_string(),
        None => text.to_string(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, String> {
    mutex
        .lock()
        .map_err(|e| format!("Index lock poisoned: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn meta(session_id: &str, source_path: &str) -> SessionMeta {
        SessionMeta {
            provider_id: "codex".to_string(),
            session_id: session_id.to_string(),
            title: None,
            summary: None,
            project_dir: None,
            created_at: Some(1),
            last_active_at: Some(1),
            source_path: Some(source_path.to_string()),
            resume_command: None,
        }
    }

    fn message(role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            ts: Some(1),
        }
    }

    /// Writes a real file so `source_version` has something to stat.
    fn touch(dir: &TempDir, name: &str, contents: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write session file");
        path.to_string_lossy().to_string()
    }

    fn index_in(dir: &TempDir) -> SessionIndex {
        SessionIndex::open_at(dir.path().join("index.db")).expect("open index")
    }

    fn sync(
        index: &SessionIndex,
        sessions: &[SessionMeta],
        messages: Vec<SessionMessage>,
    ) -> SyncOutcome {
        index
            .sync_with(
                sessions,
                |_| Ok(messages.clone()),
                |_, _| {},
                &AtomicBool::new(false),
            )
            .expect("sync")
    }

    #[test]
    fn finds_cjk_substrings_below_trigram_length() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![message("user", "帮我重构转发站的限流逻辑")],
        );

        // Three characters or more goes through the trigram index...
        assert_eq!(index.search("转发站", 10).expect("search").len(), 1);
        // ...and two characters, which the index cannot serve, still matches.
        assert_eq!(index.search("限流", 10).expect("search").len(), 1);
        assert!(index.search("不存在的词", 10).expect("search").is_empty());
    }

    #[test]
    fn matches_ascii_case_insensitively() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![message("assistant", "Refactor the RelayStation limiter")],
        );

        assert_eq!(index.search("relaystation", 10).expect("search").len(), 1);
        assert_eq!(index.search("LIMITER", 10).expect("search").len(), 1);
    }

    #[test]
    fn skips_tool_and_system_messages() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![
                message("user", "look at the config"),
                message("tool", "SECRET_TOOL_OUTPUT"),
                message("system", "SECRET_SYSTEM_PROMPT"),
            ],
        );

        assert_eq!(index.search("config", 10).expect("search").len(), 1);
        assert!(index
            .search("SECRET_TOOL_OUTPUT", 10)
            .expect("search")
            .is_empty());
        assert!(index
            .search("SECRET_SYSTEM_PROMPT", 10)
            .expect("search")
            .is_empty());
    }

    #[test]
    fn reports_position_within_the_full_transcript() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![
                message("user", "first"),
                message("tool", "noise"),
                message("assistant", "the needle is here"),
            ],
        );

        let hits = index.search("needle", 10).expect("search");
        assert_eq!(hits.len(), 1);
        // Index 2 in the full transcript, not index 1 among indexed messages.
        assert_eq!(hits[0].ord, 2);
        assert_eq!(hits[0].role, "assistant");
        assert!(hits[0].snippet.contains("needle"));
    }

    #[test]
    fn returns_one_hit_per_session() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![
                message("user", "needle one"),
                message("assistant", "needle two"),
                message("user", "needle three"),
            ],
        );

        let hits = index.search("needle", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ord, 0, "expected the earliest match");
    }

    #[test]
    fn treats_like_wildcards_as_literal_text() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![message("user", "plain text without wildcards")],
        );

        // The indexed half of the query leaves these acting as wildcards, so
        // this is really a test that the escaped half narrows it back down.
        assert!(index.search("%", 10).expect("search").is_empty());
        assert!(index.search("_", 10).expect("search").is_empty());
        assert!(index.search("plain%text", 10).expect("search").is_empty());
        // `_` must not stand in for the hyphen.
        assert!(index.search("plain_text", 10).expect("search").is_empty());
    }

    #[test]
    fn finds_wildcard_characters_when_they_are_really_in_the_text() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![message("user", "usage hit 100% of the rate_limit budget")],
        );

        assert_eq!(index.search("100%", 10).expect("search").len(), 1);
        let hits = index.search("rate_limit", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("rate_limit"),
            "the snippet should be centred on the match, got {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn blank_query_matches_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(&index, &[meta("s1", &path)], vec![message("user", "hello")]);

        assert!(index.search("", 10).expect("search").is_empty());
        assert!(index.search("   ", 10).expect("search").is_empty());
    }

    #[test]
    fn unchanged_sessions_are_skipped_on_resync() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        let sessions = [meta("s1", &path)];

        let first = sync(&index, &sessions, vec![message("user", "hello")]);
        assert_eq!(first.indexed, 1);
        assert_eq!(first.skipped, 0);

        let second = sync(&index, &sessions, vec![message("user", "hello")]);
        assert_eq!(second.indexed, 0);
        assert_eq!(second.skipped, 1);
    }

    #[test]
    fn changed_sessions_are_reindexed_without_leaving_stale_hits() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        let sessions = [meta("s1", &path)];

        sync(&index, &sessions, vec![message("user", "original wording")]);
        assert_eq!(index.search("original", 10).expect("search").len(), 1);

        // Change the file so the (mtime, size) version no longer matches.
        std::fs::write(&path, "aa").expect("rewrite session file");
        sync(&index, &sessions, vec![message("user", "revised wording")]);

        assert_eq!(index.search("revised", 10).expect("search").len(), 1);
        assert!(
            index.search("original", 10).expect("search").is_empty(),
            "the superseded body must leave the FTS index too"
        );
    }

    #[test]
    fn prunes_sessions_that_vanished_from_disk() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let kept = touch(&dir, "kept.jsonl", "a");
        let gone = touch(&dir, "gone.jsonl", "a");

        sync(
            &index,
            &[meta("s1", &kept), meta("s2", &gone)],
            vec![message("user", "shared needle")],
        );
        assert_eq!(index.search("needle", 10).expect("search").len(), 2);

        let outcome = sync(
            &index,
            &[meta("s1", &kept)],
            vec![message("user", "shared needle")],
        );
        assert_eq!(outcome.removed, 1);
        assert_eq!(index.search("needle", 10).expect("search").len(), 1);
    }

    #[test]
    fn cancelled_sync_keeps_unvisited_sessions() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let first = touch(&dir, "a.jsonl", "a");
        let second = touch(&dir, "b.jsonl", "a");
        let sessions = [meta("s1", &first), meta("s2", &second)];

        sync(&index, &sessions, vec![message("user", "needle")]);
        assert_eq!(index.search("needle", 10).expect("search").len(), 2);

        // Cancel before the second session is reached; the pass has not proven
        // that session is gone, so pruning it would be wrong.
        let cancel = AtomicBool::new(true);
        let outcome = index
            .sync_with(&sessions, |_| Ok(vec![]), |_, _| {}, &cancel)
            .expect("sync");
        assert!(outcome.cancelled);
        assert_eq!(outcome.removed, 0);
        assert_eq!(index.search("needle", 10).expect("search").len(), 2);
    }

    #[test]
    fn removing_a_session_clears_its_hits() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &path)],
            vec![message("user", "needle")],
        );

        index.remove_session(&path).expect("remove");
        assert!(index.search("needle", 10).expect("search").is_empty());

        // Removing an unknown path is a no-op, not an error: the caller may be
        // deleting a session that was never indexed.
        index
            .remove_session("/nonexistent")
            .expect("remove missing");
    }

    #[test]
    fn failed_transcript_load_keeps_previous_hits() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        let sessions = [meta("s1", &path)];

        sync(&index, &sessions, vec![message("user", "needle")]);
        std::fs::write(&path, "aa").expect("rewrite session file");

        index
            .sync_with(
                &sessions,
                |_| Err("transient read error".to_string()),
                |_, _| {},
                &AtomicBool::new(false),
            )
            .expect("sync");

        assert_eq!(
            index.search("needle", 10).expect("search").len(),
            1,
            "a read error must not silently drop a session from search"
        );
    }

    #[test]
    fn incremental_auto_vacuum_is_armed_before_any_data_lands() {
        // Set after the first table exists, this pragma silently does nothing
        // and the only way back is a full VACUUM.
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let conn = index.reader.lock().expect("reader");
        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .expect("read auto_vacuum");
        assert_eq!(mode, 2, "expected INCREMENTAL auto_vacuum");
    }

    #[test]
    fn maintenance_returns_freed_pages_and_records_the_pass() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);

        let bulk: Vec<SessionMeta> = (0..40)
            .map(|i| {
                let path = touch(&dir, &format!("s{i}.jsonl"), "a");
                meta(&format!("s{i}"), &path)
            })
            .collect();
        let body = "needle ".repeat(4_000);
        sync(&index, &bulk, vec![message("user", &body)]);

        // Delete everything, which frees a large number of pages.
        for session in &bulk {
            index
                .remove_session(session.source_path.as_deref().expect("source path"))
                .expect("remove");
        }
        assert!(index.needs_maintenance().expect("needs maintenance"));

        index.maintain(Duration::from_secs(10)).expect("maintain");

        let stats = index.stats().expect("stats");
        assert_eq!(stats.reclaimable_bytes, 0, "free pages should be returned");
        assert!(stats.last_maintenance_at.is_some());
        assert!(!index.needs_maintenance().expect("needs maintenance"));
    }

    #[test]
    fn a_version_mismatch_rebuilds_instead_of_failing() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("index.db");
        {
            let index = SessionIndex::open_at(&path).expect("open index");
            let file = touch(&dir, "a.jsonl", "a");
            sync(
                &index,
                &[meta("s1", &file)],
                vec![message("user", "needle")],
            );
            let conn = index.writer.lock().expect("writer");
            write_meta(&conn, "index_version", "-999").expect("write version");
        }

        let reopened = SessionIndex::open_at(&path).expect("reopen index");
        assert_eq!(reopened.stats().expect("stats").sessions, 0);
        // Rebuilt empty rather than erroring out, and usable straight away.
        assert!(reopened.search("needle", 10).expect("search").is_empty());
    }

    #[test]
    fn a_corrupt_database_is_replaced() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("index.db");
        std::fs::write(&path, "this is not a sqlite database").expect("write junk");

        let index = SessionIndex::open_at(&path).expect("open index over junk");
        let file = touch(&dir, "a.jsonl", "a");
        sync(
            &index,
            &[meta("s1", &file)],
            vec![message("user", "needle")],
        );
        assert_eq!(index.search("needle", 10).expect("search").len(), 1);
    }

    #[test]
    fn oversized_messages_are_capped() {
        let dir = TempDir::new().expect("tempdir");
        let index = index_in(&dir);
        let path = touch(&dir, "a.jsonl", "a");
        let body = format!("{}NEEDLE_PAST_THE_CAP", "x".repeat(BODY_MAX_CHARS));
        sync(&index, &[meta("s1", &path)], vec![message("user", &body)]);

        assert!(index
            .search("NEEDLE_PAST_THE_CAP", 10)
            .expect("search")
            .is_empty());
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Byte-slicing multibyte text would panic rather than truncate.
        assert_eq!(truncate_chars("转发站限流", 3), "转发站");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
