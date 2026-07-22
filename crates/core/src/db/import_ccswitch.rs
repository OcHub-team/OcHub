//! cc-switch → OCHUB 一次性数据导入。
//!
//! 只在全新 OCHUB 数据库首次初始化时运行：以只读方式 ATTACH
//! `~/.cc-switch/cc-switch.db`，把数据翻译进 OCHUB 自己的 schema。
//! 对 `~/.cc-switch/` 零写入，原版 cc-switch 可继续照常使用。
//!
//! 兼容策略（宽容读取）：cc-switch 的历史迁移全部是加表/加列（additive），
//! 因此逐表按「目标列 ∩ 源列」的交集拷贝即可同时兼容 v11..v16 乃至更新的
//! 源库 —— 未知表/列自然被忽略，缺失列落到目标默认值。源版本高于已验证
//! 范围时仅告警，不拒绝导入。

use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use super::{lock_conn, Database};
use crate::error::AppError;

/// 已验证过导入兼容性的最高 cc-switch schema 版本。
const MAX_VERIFIED_SOURCE_VERSION: i32 = 16;

/// OCHUB 认识的 app_type 值（`AppType` 的全部字符串形态 + 历史别名）。
/// 带 app_type 语义行的表按此过滤，避免导入 OCHUB 无法解析的行
/// （例如新版 cc-switch 的 grokbuild）。
const KNOWN_APP_TYPES: &str = "('claude','claude-desktop','claude_desktop','claudeDesktop','codex','gemini','opencode','openclaw','hermes')";

/// proxy_config 表受 CHECK 约束限制的 app_type 集合。
const PROXY_CONFIG_APP_TYPES: &str = "('claude','codex','gemini')";

/// 单表导入结果。
#[derive(Debug, Clone, Serialize)]
pub struct TableImport {
    pub table: String,
    pub rows: usize,
}

/// 一次性导入报告，持久化到 settings（key = `ccswitch_import_report`）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub source_path: String,
    pub source_schema_version: i32,
    pub imported_at: i64,
    pub tables: Vec<TableImport>,
    pub skipped_tables: Vec<String>,
    pub warnings: Vec<String>,
}

impl ImportReport {
    pub fn total_rows(&self) -> usize {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// 导入表清单：(表名, 行过滤 WHERE 子句, 冲突策略)。
///
/// 明确排除：
/// - `proxy_live_backup` — cc-switch 的运行时 live-config 接管状态，导入会让
///   OCHUB 误以为持有待恢复的备份；
/// - `stream_check_logs` — 瞬态测速日志，无长期价值；
/// - `sqlite_sequence` — SQLite 内部表。
const IMPORT_TABLES: &[(&str, Option<&str>, &str)] = &[
    ("providers", Some(KNOWN_APP_TYPES), "OR REPLACE"),
    ("provider_endpoints", Some(KNOWN_APP_TYPES), "OR REPLACE"),
    ("provider_health", Some(KNOWN_APP_TYPES), "OR REPLACE"),
    ("mcp_servers", None, "OR REPLACE"),
    ("prompts", Some(KNOWN_APP_TYPES), "OR REPLACE"),
    ("skills", None, "OR REPLACE"),
    ("skill_repos", None, "OR REPLACE"),
    ("settings", None, "OR REPLACE"),
    ("proxy_config", Some(PROXY_CONFIG_APP_TYPES), "OR REPLACE"),
    // model_pricing：REPLACE 覆盖内置种子 —— 源库可能带用户自定义定价与更新的价目
    ("model_pricing", None, "OR REPLACE"),
    ("proxy_request_logs", None, "OR REPLACE"),
    ("usage_daily_rollups", None, "OR REPLACE"),
    ("session_log_sync", None, "OR REPLACE"),
    ("profiles", None, "OR REPLACE"),
];

/// 随数据库一并导入的旁路文件（app 配置目录下）。
/// 注意不包含 `app_paths.json`：那是数据目录重定向引导文件，复制过来会把
/// OCHUB 重新指回 `~/.cc-switch`。
const IMPORT_SIDE_FILES: &[&str] = &[
    "settings.json",
    "copilot_auth.json",
    "codex_oauth_auth.json",
];

impl Database {
    /// 从旧 cc-switch 数据库一次性导入数据。
    ///
    /// 返回 `Ok(None)` 表示没有可导入的源（首次全新安装）。仅应在全新
    /// OCHUB 数据库上调用；所有插入使用 INSERT OR REPLACE，因此对种子
    /// 数据（官方 provider、内置定价、proxy_config 三行）是覆盖语义。
    pub fn import_from_ccswitch(&self) -> Result<Option<ImportReport>, AppError> {
        let source_path = crate::paths::get_legacy_ccswitch_database_path();
        if !source_path.exists() {
            return Ok(None);
        }

        let conn = lock_conn!(self.conn);
        let report = Self::import_from_ccswitch_on_conn(&conn, &source_path)?;
        drop(conn);

        // 报告持久化 + 旁路文件复制都在数据导入成功之后进行
        self.persist_import_report(&report)?;
        copy_side_files();

        Ok(Some(report))
    }

    fn import_from_ccswitch_on_conn(
        conn: &Connection,
        source_path: &Path,
    ) -> Result<ImportReport, AppError> {
        let attach_uri = format!(
            "file:{}?mode=ro",
            source_path.to_string_lossy().replace('?', "%3f")
        );
        conn.execute("ATTACH DATABASE ?1 AS ccswitch;", [attach_uri.as_str()])
            .map_err(|e| AppError::Database(format!("只读附加 cc-switch 数据库失败: {e}")))?;

        let result = Self::copy_ccswitch_tables(conn, source_path);

        conn.execute("DETACH DATABASE ccswitch;", [])
            .map_err(|e| AppError::Database(format!("分离 cc-switch 数据库失败: {e}")))
            .and(result)
    }

    fn copy_ccswitch_tables(
        conn: &Connection,
        source_path: &Path,
    ) -> Result<ImportReport, AppError> {
        let source_version: i32 = conn
            .query_row("PRAGMA ccswitch.user_version;", [], |row| row.get(0))
            .map_err(|e| AppError::Database(format!("读取源数据库版本失败: {e}")))?;

        let mut warnings = Vec::new();
        if source_version > MAX_VERIFIED_SOURCE_VERSION {
            let msg = format!(
                "源数据库 schema v{source_version} 高于已验证的 v{MAX_VERIFIED_SOURCE_VERSION}，\
                 采用宽容模式导入（未知表/列将被忽略）"
            );
            log::warn!("{msg}");
            warnings.push(msg);
        }

        conn.execute("SAVEPOINT ccswitch_import;", [])
            .map_err(|e| AppError::Database(format!("开启导入 savepoint 失败: {e}")))?;

        let result = (|| {
            let mut tables = Vec::new();
            let mut skipped = Vec::new();

            for (table, row_filter, conflict) in IMPORT_TABLES {
                if !Self::table_exists_in(conn, "ccswitch", table)? {
                    skipped.push(format!("{table}（源库中不存在）"));
                    continue;
                }

                let columns = Self::common_columns(conn, table)?;
                if columns.is_empty() {
                    skipped.push(format!("{table}（无共同列）"));
                    continue;
                }

                let column_list = columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let where_clause = row_filter
                    .map(|set| format!(" WHERE app_type IN {set}"))
                    .unwrap_or_default();
                let sql = format!(
                    "INSERT {conflict} INTO main.\"{table}\" ({column_list})
                     SELECT {column_list} FROM ccswitch.\"{table}\"{where_clause}"
                );
                let rows = conn
                    .execute(&sql, [])
                    .map_err(|e| AppError::Database(format!("导入表 {table} 失败: {e}")))?;

                tables.push(TableImport {
                    table: (*table).to_string(),
                    rows,
                });
            }

            Ok((tables, skipped))
        })();

        match result {
            Ok((tables, skipped_tables)) => {
                conn.execute("RELEASE ccswitch_import;", [])
                    .map_err(|e| AppError::Database(format!("提交导入 savepoint 失败: {e}")))?;
                let imported_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Ok(ImportReport {
                    source_path: source_path.to_string_lossy().to_string(),
                    source_schema_version: source_version,
                    imported_at,
                    tables,
                    skipped_tables,
                    warnings,
                })
            }
            Err(e) => {
                conn.execute("ROLLBACK TO ccswitch_import;", []).ok();
                conn.execute("RELEASE ccswitch_import;", []).ok();
                Err(e)
            }
        }
    }

    /// 目标表与源表（ccswitch attach）列名交集，按目标表列序。
    fn common_columns(conn: &Connection, table: &str) -> Result<Vec<String>, AppError> {
        let target = Self::column_names(conn, "main", table)?;
        let source = Self::column_names(conn, "ccswitch", table)?;
        Ok(target.into_iter().filter(|c| source.contains(c)).collect())
    }

    fn column_names(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>, AppError> {
        let sql = format!("PRAGMA {schema}.table_info(\"{table}\")");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(format!("读取 {schema}.{table} 列信息失败: {e}")))?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| AppError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(names)
    }

    fn table_exists_in(conn: &Connection, schema: &str, table: &str) -> Result<bool, AppError> {
        let sql =
            format!("SELECT COUNT(*) FROM {schema}.sqlite_master WHERE type='table' AND name=?1");
        let count: i64 = conn
            .query_row(&sql, [table], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    /// 把导入报告写进 settings，供 UI / 排查溯源。
    fn persist_import_report(&self, report: &ImportReport) -> Result<(), AppError> {
        let json = super::to_json_string(report)?;
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('ccswitch_import_report', ?1)",
            [json.as_str()],
        )
        .map_err(|e| AppError::Database(format!("写入导入报告失败: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use rusqlite::Connection;

    /// 造一个模拟 cc-switch v13+ 源库：
    /// - providers 带 OCHUB 不认识的额外列（enabled_grokbuild）和 grokbuild 行
    /// - 一张 OCHUB 完全不认识的表（grok_things）
    /// - proxy_live_backup 带数据（必须被跳过）
    fn build_fake_ccswitch_db(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 13;
             CREATE TABLE providers (
                 id TEXT NOT NULL, app_type TEXT NOT NULL, name TEXT NOT NULL,
                 settings_config TEXT NOT NULL, is_current BOOLEAN NOT NULL DEFAULT 0,
                 enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0,
                 PRIMARY KEY (id, app_type)
             );
             INSERT INTO providers VALUES ('p1', 'claude', 'My Claude', '{}', 1, 0);
             INSERT INTO providers VALUES ('p2', 'grokbuild', 'Grok', '{}', 0, 1);
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO settings VALUES ('language', 'zh');
             CREATE TABLE proxy_live_backup (
                 app_type TEXT PRIMARY KEY, original_config TEXT NOT NULL, backed_up_at TEXT NOT NULL
             );
             INSERT INTO proxy_live_backup VALUES ('claude', '{}', 'now');
             CREATE TABLE grok_things (id TEXT PRIMARY KEY);
             INSERT INTO grok_things VALUES ('g1');",
        )
        .unwrap();
    }

    #[test]
    fn import_is_tolerant_and_filters_unknown_app_types() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cc-switch.db");
        build_fake_ccswitch_db(&source);

        let db = Database::memory().unwrap();
        let conn = db.conn.lock().unwrap();
        let report = Database::import_from_ccswitch_on_conn(&conn, &source).unwrap();

        assert_eq!(report.source_schema_version, 13);

        // providers：只导入 claude 行，grokbuild 行被过滤
        let provider_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(provider_count, 1);
        let name: String = conn
            .query_row("SELECT name FROM providers WHERE id='p1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "My Claude");

        // settings 正常导入
        let lang: String = conn
            .query_row("SELECT value FROM settings WHERE key='language'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(lang, "zh");

        // proxy_live_backup 不在导入清单里，目标库保持为空
        let backup_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_live_backup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(backup_count, 0);

        // 源库中缺失的表记入 skipped
        assert!(report
            .skipped_tables
            .iter()
            .any(|s| s.starts_with("mcp_servers")));

        // 源库未被写入（journal 模式下写入会产生 -wal/-journal 文件或修改内容）
        let source_version: i32 = Connection::open(&source)
            .unwrap()
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(source_version, 13);
    }

    #[test]
    fn import_returns_none_without_source() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        let db = Database::memory().unwrap();
        let conn = db.conn.lock().unwrap();
        let result = Database::import_from_ccswitch_on_conn(&conn, &missing);
        // mode=ro 打开不存在的文件必然失败——上层 import_from_ccswitch 在
        // 调用前已用 exists() 短路，这里验证底层不会静默建库
        assert!(result.is_err());
        assert!(!missing.exists());
    }
}

/// 复制 app 配置目录下的旁路文件（设备设置、托管 OAuth 账户）。
/// 尽力而为：单个文件失败只告警，不影响导入结果。
fn copy_side_files() {
    let source_dir = crate::paths::get_legacy_ccswitch_dir();
    let target_dir = crate::paths::get_app_config_dir();
    for name in IMPORT_SIDE_FILES {
        let source = source_dir.join(name);
        let target = target_dir.join(name);
        if !source.exists() || target.exists() {
            continue;
        }
        match std::fs::copy(&source, &target) {
            Ok(_) => log::info!("imported cc-switch side file: {name}"),
            Err(e) => log::warn!("failed to copy cc-switch side file {name}: {e}"),
        }
    }
}
