//! cc-switch → OcHub 一次性数据导入。
//!
//! 由用户在首启弹窗里确认后运行。cc-switch 有两代存储，两代都能作为来源：
//!
//! | 来源 | 路径 | 说明 |
//! | --- | --- | --- |
//! | [`ImportSourceKind::Database`] | `~/.cc-switch/cc-switch.db` | v3.x 起的 SQLite 库 |
//! | [`ImportSourceKind::ConfigJson`] | `~/.cc-switch/config.json` | 更早的 JSON 配置文件 |
//!
//! 数据库是 JSON 的超集（还带用量历史、备用端点、计价与设置），检测到就优先
//! 用它。两条路径都对 `~/.cc-switch/` 零写入，原版 cc-switch 可继续照常使用。
//!
//! 兼容策略（宽容读取）：cc-switch 的历史迁移全部是加表/加列（additive），
//! 因此逐表按「目标列 ∩ 源列」的交集拷贝即可同时兼容 v11..v16 乃至更新的
//! 源库 —— 未知表/列自然被忽略，缺失列落到目标默认值。源版本高于已验证
//! 范围时仅告警，不拒绝导入。JSON 路径同理：认不出的配置段记进报告后跳过，
//! 单个供应商解析失败只丢它自己，不连累整份文件。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::legacy_json::{CommonConfigSnippets, McpRoot, MultiAppConfig, SkillStore};
use super::{Database, lock_conn};
use crate::app_type::AppType;
use crate::error::AppError;
use crate::model::{Provider, ProviderManager};

/// 已验证过导入兼容性的最高 cc-switch schema 版本。
const MAX_VERIFIED_SOURCE_VERSION: i32 = 16;

/// 记录用户对首启导入的选择（`imported` / `skipped`）。存在即表示已经问过，
/// 首启弹窗不再提第二次；「设置 → 数据」里的手动入口不看这个键。
pub const IMPORT_DECISION_KEY: &str = "ccswitch_import_decision";
/// 导入报告的 settings 键。
pub const IMPORT_REPORT_KEY: &str = "ccswitch_import_report";

/// Gemini CLI 的写入端已经移除，但历史数据仍可读（见 `spec/ARCHITECTURE.md`
/// 集成规则 4），所以它不是 [`AppType`] 却仍然是合法的导入 app_type。
const LEGACY_GEMINI_APP: &str = "gemini";

/// OcHub 认识的 app_type 值（`AppType` 的全部字符串形态 + 历史别名）。
/// 带 app_type 语义行的表按此过滤，避免导入 OcHub 无法解析的行
/// （例如新版 cc-switch 的 grokbuild）。
const KNOWN_APP_TYPES: &str = "('claude','claude-desktop','claude_desktop','claudeDesktop','codex','gemini','opencode','openclaw','hermes')";

/// 旧版 proxy_config 表受 CHECK 约束限制的 app_type 集合。
const LEGACY_USAGE_CONFIG_APP_TYPES: &str = "('claude','codex','gemini')";

/// 单表导入结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableImport {
    pub table: String,
    pub rows: usize,
}

/// cc-switch 的两代存储形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSourceKind {
    /// `cc-switch.db`：v3.x 起的 SQLite 库，内容是 JSON 配置的超集。
    Database,
    /// `config.json`：更早的 `MultiAppConfig` JSON 配置文件。
    ConfigJson,
}

/// 磁盘上找到的一份 cc-switch 数据，附带够首启弹窗说清「会带来什么」的清点。
///
/// 清点是只读的：检测本身不写任何一边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedSource {
    pub kind: ImportSourceKind,
    pub path: PathBuf,
    pub providers: usize,
    pub mcp_servers: usize,
    pub skill_repos: usize,
}

impl DetectedSource {
    /// 是否清点到了任何值得导入的东西。空壳来源（例如刚装好、还没配过的
    /// cc-switch）不值得在首启时打扰用户。
    pub fn is_empty(&self) -> bool {
        self.providers == 0 && self.mcp_servers == 0 && self.skill_repos == 0
    }
}

/// 一次性导入报告，持久化到 settings（key = [`IMPORT_REPORT_KEY`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub source_kind: ImportSourceKind,
    pub source_path: String,
    /// 数据库来源是 `PRAGMA user_version`；JSON 来源是文件里的 `version` 字段。
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

    /// 报告里某张目标表的行数（没导入过该表就是 0）。
    pub fn rows_for(&self, table: &str) -> usize {
        self.tables
            .iter()
            .find(|t| t.table == table)
            .map(|t| t.rows)
            .unwrap_or(0)
    }
}

/// 导入表清单：(源表名, 目标表名, 行过滤 WHERE 子句, 冲突策略)。
///
/// 明确排除：
/// - `proxy_live_backup` — cc-switch 的运行时 live-config 接管状态，导入会让
///   OcHub 误以为持有待恢复的备份；
/// - `sqlite_sequence` — SQLite 内部表。
const IMPORT_TABLES: &[(&str, &str, Option<&str>, &str)] = &[
    (
        "providers",
        "providers",
        Some(KNOWN_APP_TYPES),
        "OR REPLACE",
    ),
    (
        "provider_endpoints",
        "provider_endpoints",
        Some(KNOWN_APP_TYPES),
        "OR REPLACE",
    ),
    ("mcp_servers", "mcp_servers", None, "OR REPLACE"),
    ("skills", "skills", None, "OR REPLACE"),
    ("skill_repos", "skill_repos", None, "OR REPLACE"),
    ("settings", "settings", None, "OR REPLACE"),
    // Only usage pricing survives from the retired proxy configuration.
    (
        "proxy_config",
        "usage_config",
        Some(LEGACY_USAGE_CONFIG_APP_TYPES),
        "OR REPLACE",
    ),
    // Imported pricing has no source marker in cc-switch, so preserve it as
    // manual overrides. LiteLLM catalog rows live in separate local-only tables.
    ("model_pricing", "model_pricing", None, "OR REPLACE"),
    // Preserve historical usage under the gateway-neutral target table name.
    ("proxy_request_logs", "usage_logs", None, "OR REPLACE"),
    (
        "usage_daily_rollups",
        "usage_daily_rollups",
        None,
        "OR REPLACE",
    ),
    ("session_log_sync", "session_log_sync", None, "OR REPLACE"),
    ("profiles", "profiles", None, "OR REPLACE"),
];

/// 随数据库一并导入的旁路文件（app 配置目录下）。
/// 注意不包含 `app_paths.json`：那是数据目录重定向引导文件，复制过来会把
/// OcHub 重新指回 `~/.cc-switch`。
const IMPORT_SIDE_FILES: &[&str] = &[
    "settings.json",
    "copilot_auth.json",
    "codex_oauth_auth.json",
];

impl Database {
    /// 从一份已检测到的 cc-switch 数据一次性导入。
    ///
    /// 所有插入使用 INSERT OR REPLACE，因此对种子数据（官方 provider、内置
    /// 定价、用量定价设置）以及重复导入的同 id 记录都是覆盖语义。
    pub fn import_from_ccswitch_source(
        &self,
        source: &DetectedSource,
    ) -> Result<ImportReport, AppError> {
        // Importing into an install that already holds providers merges
        // cc-switch's records over whatever shares their ids. A snapshot first
        // makes that reversible. The test is what the database actually holds,
        // not whether this is the first launch: a user can add providers and
        // then run the import from Settings within the same session.
        if self.has_provider_rows().unwrap_or(false) {
            match self.backup_database_file() {
                Ok(Some(path)) => log::info!("pre-import database backup: {}", path.display()),
                Ok(None) => {}
                Err(e) => log::warn!("pre-import database backup failed, continuing: {e}"),
            }
        }

        let report = match source.kind {
            ImportSourceKind::Database => {
                let conn = lock_conn!(self.conn);
                Self::import_from_ccswitch_on_conn(&conn, &source.path)?
            }
            ImportSourceKind::ConfigJson => self.import_from_ccswitch_json(&source.path)?,
        };

        // 报告持久化 + 旁路文件复制都在数据导入成功之后进行
        self.persist_import_report(&report)?;
        copy_side_files();

        Ok(report)
    }

    /// 目标库里已经有供应商记录 —— 也就是「这次导入会覆盖到东西」。
    fn has_provider_rows(&self) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    /// 用户对首启导入的选择；`None` = 还没问过。
    pub fn ccswitch_import_decision(&self) -> Result<Option<String>, AppError> {
        self.get_setting(IMPORT_DECISION_KEY)
    }

    /// 记下用户的选择，让首启弹窗不再提第二次。
    pub fn set_ccswitch_import_decision(&self, decision: &str) -> Result<(), AppError> {
        self.set_setting(IMPORT_DECISION_KEY, decision)
    }

    /// 最近一次导入报告的原始 JSON。
    pub fn ccswitch_import_report_json(&self) -> Result<Option<String>, AppError> {
        self.get_setting(IMPORT_REPORT_KEY)
    }

    /// 从旧 `config.json`（`MultiAppConfig`）一次性导入。
    ///
    /// 复用 JSON → SQLite 的迁移实现，整体包在 savepoint 里：任何一步失败都
    /// 回滚到导入前，不留半份数据。
    fn import_from_ccswitch_json(&self, source_path: &Path) -> Result<ImportReport, AppError> {
        let loaded = load_legacy_config(source_path)?;

        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(format!("开启导入事务失败: {e}")))?;
        Self::migrate_from_json_tx(&tx, &loaded.config)?;
        tx.commit()
            .map_err(|e| AppError::Database(format!("提交导入事务失败: {e}")))?;
        drop(conn);

        Ok(ImportReport {
            source_kind: ImportSourceKind::ConfigJson,
            source_path: source_path.to_string_lossy().to_string(),
            source_schema_version: loaded.config.version as i32,
            imported_at: now_unix(),
            tables: loaded.counts(),
            skipped_tables: loaded.skipped,
            warnings: loaded.warnings,
        })
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

            for (source_table, target_table, row_filter, conflict) in IMPORT_TABLES {
                if !Self::table_exists_in(conn, "ccswitch", source_table)? {
                    skipped.push(format!("{source_table}（源库中不存在）"));
                    continue;
                }

                let columns = Self::common_columns(conn, source_table, target_table)?;
                if columns.is_empty() {
                    skipped.push(format!("{source_table} → {target_table}（无共同列）"));
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
                    "INSERT {conflict} INTO main.\"{target_table}\" ({column_list})
                     SELECT {column_list} FROM ccswitch.\"{source_table}\"{where_clause}"
                );
                let rows = conn.execute(&sql, []).map_err(|e| {
                    AppError::Database(format!("导入表 {source_table} → {target_table} 失败: {e}"))
                })?;

                tables.push(TableImport {
                    table: (*target_table).to_string(),
                    rows,
                });
            }

            // Generic settings import intentionally carries unknown future keys, but the
            // retired local-routing settings are known dead state and must not reappear.
            conn.execute(
                "DELETE FROM main.settings
                 WHERE key = 'global_proxy_url'
                    OR key LIKE 'proxy_takeover_%'
                    OR key IN (
                        'rectifier_config',
                        'optimizer_config',
                        'copilot_optimizer_config',
                        'log_config'
                    )",
                [],
            )
            .map_err(|e| AppError::Database(format!("清理旧代理设置失败: {e}")))?;

            Ok((tables, skipped))
        })();

        match result {
            Ok((tables, skipped_tables)) => {
                conn.execute("RELEASE ccswitch_import;", [])
                    .map_err(|e| AppError::Database(format!("提交导入 savepoint 失败: {e}")))?;
                Ok(ImportReport {
                    source_kind: ImportSourceKind::Database,
                    source_path: source_path.to_string_lossy().to_string(),
                    source_schema_version: source_version,
                    imported_at: now_unix(),
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
    fn common_columns(
        conn: &Connection,
        source_table: &str,
        target_table: &str,
    ) -> Result<Vec<String>, AppError> {
        let target = Self::column_names(conn, "main", target_table)?;
        let source = Self::column_names(conn, "ccswitch", source_table)?;
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
        self.set_setting(IMPORT_REPORT_KEY, &json)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 来源检测
// ---------------------------------------------------------------------------

/// 在默认位置（`~/.cc-switch/`）找一份可导入的 cc-switch 数据。
///
/// 数据库优先：它是 JSON 配置的超集。数据库存在但读不动（损坏、权限）时
/// 退回 JSON，而不是直接判定「无可导入」。
pub fn detect_source() -> Option<DetectedSource> {
    let db_path = crate::paths::get_legacy_ccswitch_database_path();
    if db_path.exists() {
        match count_database_source(&db_path) {
            Ok(source) => return Some(source),
            Err(e) => log::warn!("cc-switch 数据库无法清点，尝试回退到 config.json: {e}"),
        }
    }

    let json_path = crate::paths::get_legacy_ccswitch_config_path();
    if json_path.exists() {
        match count_config_json_source(&json_path) {
            Ok(source) => return Some(source),
            Err(e) => log::warn!("cc-switch config.json 无法清点: {e}"),
        }
    }

    None
}

/// 清点用户手动选中的一份文件。按扩展名/文件名判定属于哪一代存储。
pub fn detect_source_at(path: &Path) -> Result<DetectedSource, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!("文件不存在: {}", path.display())));
    }
    let looks_like_json = path
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if looks_like_json {
        count_config_json_source(path)
    } else {
        count_database_source(path)
    }
}

fn count_database_source(path: &Path) -> Result<DetectedSource, AppError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| AppError::Database(format!("只读打开 cc-switch 数据库失败: {e}")))?;

    Ok(DetectedSource {
        kind: ImportSourceKind::Database,
        path: path.to_path_buf(),
        providers: count_rows(&conn, "providers", Some(KNOWN_APP_TYPES)),
        mcp_servers: count_rows(&conn, "mcp_servers", None),
        skill_repos: count_rows(&conn, "skill_repos", None),
    })
}

/// 清点单表行数。表不存在或查询失败都算 0：清点只用于弹窗里的摘要，
/// 不该因为一张表缺失就让整个来源看起来不可用。
fn count_rows(conn: &Connection, table: &str, app_types: Option<&str>) -> usize {
    let where_clause = app_types
        .map(|set| format!(" WHERE app_type IN {set}"))
        .unwrap_or_default();
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"{where_clause}");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as usize)
        .unwrap_or(0)
}

fn count_config_json_source(path: &Path) -> Result<DetectedSource, AppError> {
    let loaded = load_legacy_config(path)?;
    Ok(DetectedSource {
        kind: ImportSourceKind::ConfigJson,
        path: path.to_path_buf(),
        providers: loaded.provider_count(),
        mcp_servers: loaded.mcp_count(),
        skill_repos: loaded.config.skills.repos.len(),
    })
}

// ---------------------------------------------------------------------------
// 旧版 config.json 的宽容读取
// ---------------------------------------------------------------------------

/// 一份读进来的旧配置，连同读取过程中攒下的告警与跳过项。
struct LoadedLegacyConfig {
    config: MultiAppConfig,
    skipped: Vec<String>,
    warnings: Vec<String>,
}

impl LoadedLegacyConfig {
    fn provider_count(&self) -> usize {
        self.config.apps.values().map(|m| m.providers.len()).sum()
    }

    fn mcp_count(&self) -> usize {
        self.config.mcp.servers.as_ref().map_or(0, HashMap::len)
    }

    /// 迁移写入的行数。`migrate_from_json_tx` 对每条记录都是一次 INSERT OR
    /// REPLACE，所以入参条数就是写入条数。
    fn counts(&self) -> Vec<TableImport> {
        let snippets = &self.config.common_config_snippets;
        let settings_rows = [&snippets.claude, &snippets.codex, &snippets.gemini]
            .iter()
            .filter(|value| value.is_some())
            .count();

        [
            ("providers", self.provider_count()),
            ("provider_endpoints", self.endpoint_count()),
            ("mcp_servers", self.mcp_count()),
            ("skill_repos", self.config.skills.repos.len()),
            ("settings", settings_rows),
        ]
        .into_iter()
        .filter(|(_, rows)| *rows > 0)
        .map(|(table, rows)| TableImport {
            table: table.to_string(),
            rows,
        })
        .collect()
    }

    fn endpoint_count(&self) -> usize {
        self.config
            .apps
            .values()
            .flat_map(|manager| manager.providers.values())
            .map(|provider| {
                provider
                    .meta
                    .as_ref()
                    .map_or(0, |meta| meta.custom_endpoints.len())
            })
            .sum()
    }
}

/// 一个 app 段的宽容形态。
///
/// [`ProviderManager`] 的两个字段都没有 serde 默认值，缺一个就整段读不出来；
/// `providers` 收成 [`Value`] 则让坏掉的单个供应商只丢自己。
#[derive(Deserialize)]
struct LegacyAppSection {
    #[serde(default)]
    providers: IndexMap<String, Value>,
    #[serde(default)]
    current: String,
}

/// 把 cc-switch 的 app 段名归一成 OcHub 的 app_type。
///
/// `None` = 不是 app 段（`prompts` 之类）或 OcHub 不认识的应用。
fn canonical_app_key(key: &str) -> Option<String> {
    if key == LEGACY_GEMINI_APP {
        return Some(LEGACY_GEMINI_APP.to_string());
    }
    key.parse::<AppType>()
        .ok()
        .map(|app| app.as_str().to_string())
}

/// 读一份旧版 `config.json`，任何一段读不出来都不至于让整份文件作废。
///
/// [`MultiAppConfig`] 用 `#[serde(flatten)]` 收 app 段，于是每个它没声明的
/// 顶层键都会被喂给 [`ProviderManager`] —— 真实配置里都有的 `prompts` 过不了
/// 这一关，会把整份文件一起带走。所以这里手工遍历对象：认不出的键留在 app
/// 表外面，坏掉的段降级成一条告警。
fn load_legacy_config(path: &Path) -> Result<LoadedLegacyConfig, AppError> {
    let text = std::fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;
    let root: Value = serde_json::from_str(&text).map_err(|e| AppError::json(path, e))?;
    let Value::Object(root) = root else {
        return Err(AppError::Config(format!(
            "{} 不是一个 JSON 对象",
            path.display()
        )));
    };

    let mut config = MultiAppConfig {
        version: root
            .get("version")
            .and_then(Value::as_u64)
            .unwrap_or(2)
            .min(u32::MAX as u64) as u32,
        apps: HashMap::new(),
        mcp: McpRoot {
            servers: Some(HashMap::new()),
            ..McpRoot::default()
        },
        // `SkillStore::default()` 带四个内置仓库；这里要的是「文件里写了什么」，
        // 凭空多出来的仓库会被当成用户自己配的一起导进去。
        skills: SkillStore {
            skills: HashMap::new(),
            repos: Vec::new(),
        },
        common_config_snippets: CommonConfigSnippets::default(),
        claude_common_config_snippet: None,
    };
    let mut skipped = Vec::new();
    let mut warnings = Vec::new();

    for (key, value) in root {
        match key.as_str() {
            "version" => {}
            "mcp" => match serde_json::from_value::<McpRoot>(value) {
                Ok(mcp) => config.mcp = mcp,
                Err(e) => warnings.push(format!("mcp 段解析失败，已跳过: {e}")),
            },
            "skills" => match serde_json::from_value::<SkillStore>(value) {
                Ok(skills) => config.skills = skills,
                Err(e) => warnings.push(format!("skills 段解析失败，已跳过: {e}")),
            },
            "common_config_snippets" => {
                match serde_json::from_value::<CommonConfigSnippets>(value) {
                    Ok(snippets) => config.common_config_snippets = snippets,
                    Err(e) => {
                        warnings.push(format!("common_config_snippets 解析失败，已跳过: {e}"))
                    }
                }
            }
            "claude_common_config_snippet" => {
                config.claude_common_config_snippet = value.as_str().map(str::to_string);
            }
            other => match canonical_app_key(other) {
                Some(app_type) => merge_app_section(
                    &mut config.apps,
                    &app_type,
                    other,
                    value,
                    &mut warnings,
                    &mut skipped,
                ),
                None => skipped.push(format!("{other}（不是 OcHub 认识的应用配置段）")),
            },
        }
    }

    Ok(LoadedLegacyConfig {
        config,
        skipped,
        warnings,
    })
}

/// 把一个 app 段并进 app 表。
///
/// 归一化会让 `claude_desktop` 与 `claude-desktop` 落到同一个键上，所以这里
/// 是合并而不是覆盖：供应商累加，`current` 以先到的非空值为准。
fn merge_app_section(
    apps: &mut HashMap<String, ProviderManager>,
    app_type: &str,
    source_key: &str,
    value: Value,
    warnings: &mut Vec<String>,
    skipped: &mut Vec<String>,
) {
    let section = match serde_json::from_value::<LegacyAppSection>(value) {
        Ok(section) => section,
        Err(e) => {
            warnings.push(format!("{source_key} 段解析失败，已跳过: {e}"));
            return;
        }
    };

    if section.providers.is_empty() {
        skipped.push(format!("{source_key}（没有供应商）"));
        return;
    }

    let manager = apps.entry(app_type.to_string()).or_default();
    if manager.current.is_empty() {
        manager.current = section.current;
    }
    for (id, raw) in section.providers {
        match serde_json::from_value::<Provider>(raw) {
            Ok(provider) => {
                manager.providers.insert(id, provider);
            }
            Err(e) => warnings.push(format!("{source_key} 的供应商 {id} 解析失败，已跳过: {e}")),
        }
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

#[cfg(test)]
mod json_tests {
    use super::*;
    use crate::db::Database;

    /// A legacy `config.json` shaped like the ones cc-switch actually wrote:
    /// a `prompts` section that is not an app, a retired `gemini` app, an
    /// underscore spelling of claude-desktop, and one unparseable provider.
    const LEGACY_CONFIG: &str = r#"{
      "version": 2,
      "claude": {
        "current": "c1",
        "providers": {
          "c1": {
            "id": "c1",
            "name": "My Claude",
            "settingsConfig": {"env": {"ANTHROPIC_AUTH_TOKEN": "sk-1"}},
            "websiteUrl": "https://example.com",
            "meta": {"custom_endpoints": {"https://a.example": {"url": "https://a.example", "addedAt": 42}}}
          },
          "broken": {"name": "no id here"}
        }
      },
      "claude_desktop": {
        "current": "d1",
        "providers": {
          "d1": {"id": "d1", "name": "Desktop", "settingsConfig": {}}
        }
      },
      "gemini": {"current": "", "providers": {}},
      "grokbuild": {
        "current": "g1",
        "providers": {"g1": {"id": "g1", "name": "Grok", "settingsConfig": {}}}
      },
      "prompts": {"claude": {"prompts": {}}, "codex": {"prompts": {}}},
      "mcp": {
        "servers": {
          "ctx7": {
            "id": "ctx7",
            "name": "Context7",
            "server": {"command": "npx"},
            "apps": {"claude": true}
          }
        }
      },
      "skills": {"repos": [{"owner": "anthropics", "name": "skills", "branch": "main", "enabled": true}]},
      "common_config_snippets": {"claude": "{\"model\":\"opus\"}"},
      "future_section": {"anything": 1}
    }"#;

    fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("config.json");
        std::fs::write(&path, LEGACY_CONFIG).unwrap();
        path
    }

    /// The reason `load_legacy_config` walks the object by hand: serde's
    /// `flatten` feeds every undeclared key — including the `prompts` section
    /// real configs carry — to `ProviderManager`, which has no field defaults.
    /// One such key loses the entire file, providers and all.
    #[test]
    fn naive_multi_app_config_parse_chokes_on_prompts() {
        let with_prompts = r#"{
          "version": 2,
          "claude": {"current": "c1", "providers": {}},
          "prompts": {"claude": {"prompts": {}}}
        }"#;
        let err = serde_json::from_str::<MultiAppConfig>(with_prompts)
            .expect_err("prompts must break the flattened parse");
        assert!(
            err.to_string().contains("missing field `providers`"),
            "unexpected failure: {err}"
        );

        // Same file without that one section parses fine — `prompts` is the
        // whole difference.
        let without_prompts = r#"{"version": 2, "claude": {"current": "c1", "providers": {}}}"#;
        assert!(serde_json::from_str::<MultiAppConfig>(without_prompts).is_ok());
    }

    #[test]
    fn tolerant_loader_keeps_apps_and_drops_everything_else() {
        let loaded = {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(dir.path());
            load_legacy_config(&path).unwrap()
        };

        // Alias spellings land on the canonical app_type.
        let mut apps: Vec<_> = loaded.config.apps.keys().cloned().collect();
        apps.sort();
        assert_eq!(apps, ["claude", "claude-desktop", "grokbuild"]);

        // `gemini` had no providers and `prompts` is not an app at all.
        assert!(loaded.skipped.iter().any(|s| s.starts_with("gemini")));
        assert!(loaded.skipped.iter().any(|s| s.starts_with("prompts")));
        assert!(
            loaded
                .skipped
                .iter()
                .any(|s| s.starts_with("future_section"))
        );

        // The one bad provider is reported and skipped; its siblings survive.
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("broken"));
        assert_eq!(loaded.provider_count(), 3);
        assert_eq!(loaded.mcp_count(), 1);
        assert_eq!(loaded.config.skills.repos.len(), 1);
    }

    /// An absent `skills` section must not conjure `SkillStore::default()`'s
    /// four built-in repos into the user's data.
    #[test]
    fn missing_sections_import_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"version": 2}"#).unwrap();

        let loaded = load_legacy_config(&path).unwrap();
        assert!(loaded.config.skills.repos.is_empty());
        assert_eq!(loaded.mcp_count(), 0);
        assert!(loaded.counts().is_empty());
    }

    #[test]
    fn config_json_import_writes_providers_endpoints_and_snippets() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());

        let db = Database::memory().unwrap();
        let report = db.import_from_ccswitch_json(&path).unwrap();

        assert_eq!(report.source_kind, ImportSourceKind::ConfigJson);
        assert_eq!(report.source_schema_version, 2);
        assert_eq!(report.rows_for("providers"), 3);
        assert_eq!(report.rows_for("provider_endpoints"), 1);
        assert_eq!(report.rows_for("mcp_servers"), 1);
        assert_eq!(report.rows_for("skill_repos"), 1);

        let conn = db.conn.lock().unwrap();
        let name: String = conn
            .query_row(
                "SELECT name FROM providers WHERE id='c1' AND app_type='claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "My Claude");

        // The `claude_desktop` spelling was normalized on the way in.
        let desktop: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM providers WHERE app_type='claude-desktop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(desktop, 1);

        let is_current: i64 = conn
            .query_row("SELECT is_current FROM providers WHERE id='c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(is_current, 1);

        let url: String = conn
            .query_row(
                "SELECT url FROM provider_endpoints WHERE provider_id='c1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(url, "https://a.example");

        let snippet: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key='common_config_claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(snippet, "{\"model\":\"opus\"}");
    }

    /// The Settings-page entry can run against a populated database, so a
    /// second pass has to overwrite rather than accumulate.
    #[test]
    fn repeated_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());

        let db = Database::memory().unwrap();
        db.import_from_ccswitch_json(&path).unwrap();
        db.import_from_ccswitch_json(&path).unwrap();

        let conn = db.conn.lock().unwrap();
        let providers: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(providers, 3);
        let endpoints: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_endpoints", [], |r| r.get(0))
            .unwrap();
        assert_eq!(endpoints, 1, "endpoints must not accumulate across imports");
    }

    #[test]
    fn detect_at_counts_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        let source = detect_source_at(&path).unwrap();
        assert_eq!(source.kind, ImportSourceKind::ConfigJson);
        assert_eq!(source.providers, 3);
        assert_eq!(source.mcp_servers, 1);
        assert_eq!(source.skill_repos, 1);
        assert!(!source.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn empty_source_is_reported_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"version": 2, "claude": {"current": "", "providers": {}}}"#,
        )
        .unwrap();

        assert!(detect_source_at(&path).unwrap().is_empty());
    }

    #[test]
    fn detect_at_rejects_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_source_at(&dir.path().join("nope.json")).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use rusqlite::Connection;

    /// 造一个模拟 cc-switch v13+ 源库：
    /// - providers 带 OcHub 不认识的额外列（enabled_grokbuild）和 grokbuild 行
    /// - 旧代理定价与请求日志要迁入中性的用量表
    /// - 一张 OcHub 完全不认识的表（grok_things）
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
             INSERT INTO settings VALUES ('global_proxy_url', 'socks5://127.0.0.1:1080');
             INSERT INTO settings VALUES ('proxy_takeover_claude', 'true');
             CREATE TABLE proxy_config (
                 app_type TEXT PRIMARY KEY,
                 default_cost_multiplier TEXT NOT NULL DEFAULT '1',
                 pricing_model_source TEXT NOT NULL DEFAULT 'response'
             );
             INSERT INTO proxy_config VALUES ('claude', '1.5', 'request');
             CREATE TABLE proxy_request_logs (
                 request_id TEXT PRIMARY KEY,
                 provider_id TEXT NOT NULL,
                 app_type TEXT NOT NULL,
                 model TEXT NOT NULL,
                 latency_ms INTEGER NOT NULL,
                 status_code INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             INSERT INTO proxy_request_logs
                 VALUES ('legacy-request', 'p1', 'claude', 'claude-test', 12, 200, 123);
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
        let retired_settings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM settings
                 WHERE key = 'global_proxy_url' OR key LIKE 'proxy_takeover_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retired_settings, 0);

        // 旧定价和请求记录迁入新的用量结构。
        let multiplier: String = conn
            .query_row(
                "SELECT default_cost_multiplier FROM usage_config WHERE app_type='claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(multiplier, "1.5");
        let usage_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_logs WHERE request_id='legacy-request'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(usage_count, 1);

        // 运行时接管备份不属于网关数据，不能导入。
        assert!(!Database::table_exists_in(&conn, "main", "proxy_live_backup").unwrap());

        // 源库中缺失的表记入 skipped
        assert!(
            report
                .skipped_tables
                .iter()
                .any(|s| s.starts_with("mcp_servers"))
        );

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
