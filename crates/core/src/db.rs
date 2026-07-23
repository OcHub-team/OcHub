//! SQLite 数据持久化层。
//!
//! OcHub 拥有独立的数据库（`~/.ochub/ochub.db`）与独立的 schema
//! 版本线（从 v1 开始）。旧 cc-switch 数据（`~/.cc-switch/cc-switch.db`）在
//! 首次启动时通过 `import_ccswitch` 一次性只读导入，之后两者互不影响。
//!
//! ```text
//! db/
//! ├── db.rs            - Database 结构体 + 初始化（本文件）
//! ├── schema.rs        - 表结构定义 + Schema 迁移
//! ├── backup.rs        - SQL 导入导出 + 快照备份
//! ├── import_ccswitch.rs - cc-switch 数据库一次性导入
//! ├── migration.rs     - JSON → SQLite 数据迁移
//! ├── legacy_json.rs   - 旧版 config.json (MultiAppConfig) + 域结构
//! ├── stream_check_types.rs - 连通性检查类型
//! └── dao/             - 数据访问对象
//! ```

pub(crate) mod backup;
pub mod dao;
pub mod import_ccswitch;
pub mod legacy_json;
pub mod migration;
mod schema;
pub mod stream_check_types;

// DAO 类型导出供外部使用（这些是供尚未移植的服务层使用的接缝，暂时未被调用）
#[allow(unused_imports)]
pub(crate) use dao::providers_seed::{is_official_seed_id, CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID};
#[allow(unused_imports)]
pub(crate) use dao::usage_config::{
    validate_cost_multiplier, validate_pricing_source, PRICING_SOURCE_REQUEST,
    PRICING_SOURCE_RESPONSE,
};

pub use legacy_json::{
    CommonConfigSnippets, InstalledSkill, McpApps, McpConfig, McpRoot, McpServer, MultiAppConfig,
    SkillApps, SkillRepo, SkillState, SkillStore,
};
pub use stream_check_types::{HealthStatus, StreamCheckConfig, StreamCheckResult};

use crate::error::AppError;
use rusqlite::{hooks::Action, Connection};
use serde::Serialize;
use std::sync::Mutex;

/// 当前 Schema 版本号（OcHub 自有版本线，与 cc-switch 的版本序列无关）
/// 每次修改表结构时递增，并在 schema.rs 中添加相应的迁移逻辑
pub(crate) const SCHEMA_VERSION: i32 = 5;

/// 安全地序列化 JSON，避免 unwrap panic
pub(crate) fn to_json_string<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value)
        .map_err(|e| AppError::Config(format!("JSON serialization failed: {e}")))
}

/// 安全地获取 Mutex 锁，避免 unwrap panic
macro_rules! lock_conn {
    ($mutex:expr) => {
        $mutex
            .lock()
            .map_err(|e| AppError::Database(format!("Mutex lock failed: {}", e)))?
    };
}

// 导出宏供子模块使用
pub(crate) use lock_conn;

/// Notify auto-sync services that a table changed.
///
/// Fans out to the WebDAV and S3 auto-sync queues, matching cc-switch
/// `database/mod.rs::register_db_change_hook`. Each consumer filters by table
/// and debounces internally; this is a cheap try-send when a worker is running
/// and a no-op otherwise.
pub fn notify_db_changed(table: &str) {
    log::trace!("db changed: {table}");
    crate::services::webdav_auto_sync::notify_db_changed(table);
    crate::services::s3_auto_sync::notify_db_changed(table);
}

/// 数据库连接封装
///
/// 使用 Mutex 包装 Connection 以支持在多线程环境中共享。
/// rusqlite::Connection 本身不是 Sync 的，因此需要这层包装。
pub struct Database {
    pub(crate) conn: Mutex<Connection>,
}

fn register_db_change_hook(conn: &Connection) {
    // 注册失败仅意味着丢失变更通知，不影响数据库功能本身。
    let _ = conn.update_hook(Some(
        |action: Action, _database: &str, table: &str, _row_id: i64| match action {
            Action::SQLITE_INSERT | Action::SQLITE_UPDATE | Action::SQLITE_DELETE => {
                notify_db_changed(table);
            }
            _ => {}
        },
    ));
}

impl Database {
    /// 初始化数据库连接并创建表
    ///
    /// 数据库文件位于 `~/.ochub/ochub.db`。全新数据库若检测到旧的
    /// `~/.cc-switch/cc-switch.db`，会自动执行一次性只读导入。
    pub fn init() -> Result<Self, AppError> {
        let db_path = crate::paths::get_database_path();
        let db_exists = db_path.exists();

        // 确保父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }

        let conn = Connection::open(&db_path).map_err(|e| AppError::Database(e.to_string()))?;

        // 启用外键约束
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        if !db_exists {
            // For a brand-new database, configure incremental auto-vacuum
            // before anything initializes the database file (switching to WAL
            // writes the header, after which auto_vacuum can no longer change
            // without a VACUUM rebuild).
            conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
        // WAL：网关写用量日志 + UI 并发读；busy_timeout 避免瞬时锁冲突直接报错
        conn.query_row("PRAGMA journal_mode = WAL;", [], |_| Ok(()))
            .map_err(|e| AppError::Database(format!("启用 WAL 失败: {e}")))?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")
            .map_err(|e| AppError::Database(format!("设置 busy_timeout 失败: {e}")))?;
        register_db_change_hook(&conn);

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.create_tables()?;

        // Pre-migration backup: only when upgrading from an existing database
        {
            let conn = lock_conn!(db.conn);
            let version = Self::get_user_version(&conn)?;
            drop(conn);
            if version > 0 && version < SCHEMA_VERSION {
                log::info!(
                    "Creating pre-migration database backup (v{version} → v{SCHEMA_VERSION})"
                );
                if let Err(e) = db.backup_database_file() {
                    log::warn!("Pre-migration backup failed, continuing migration: {e}");
                }
            }
        }

        db.apply_schema_migrations()?;
        if let Err(e) = db.ensure_incremental_auto_vacuum() {
            log::warn!("Failed to ensure incremental auto-vacuum: {e}");
        }
        db.ensure_model_pricing_seeded()?;

        // 全新数据库：尝试从旧 cc-switch 数据一次性导入（只读，失败不阻塞启动）
        if !db_exists {
            match db.import_from_ccswitch() {
                Ok(Some(report)) => log::info!(
                    "imported cc-switch data (source schema v{}): {} rows across {} tables",
                    report.source_schema_version,
                    report.total_rows(),
                    report.tables.len()
                ),
                Ok(None) => {}
                Err(e) => {
                    log::warn!("cc-switch import failed, starting with a fresh database: {e}")
                }
            }
        }

        // Startup cleanup: prune old logs and reclaim space
        if let Err(e) = db.cleanup_old_stream_check_logs(7) {
            log::warn!("Startup stream_check_logs cleanup failed: {e}");
        }
        if let Err(e) = db.rollup_and_prune(30) {
            log::warn!("Startup rollup_and_prune failed: {e}");
        }
        // Reclaim disk space after cleanup
        {
            let conn = lock_conn!(db.conn);
            if let Err(e) = conn.execute_batch("PRAGMA incremental_vacuum;") {
                log::warn!("Startup incremental vacuum failed: {e}");
            }
        }

        Ok(db)
    }

    /// 创建内存数据库（用于测试）
    pub fn memory() -> Result<Self, AppError> {
        let conn = Connection::open_in_memory().map_err(|e| AppError::Database(e.to_string()))?;

        // 启用外键约束
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        register_db_change_hook(&conn);

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.create_tables()?;
        db.ensure_model_pricing_seeded()?;

        Ok(db)
    }

    pub(crate) fn get_auto_vacuum_mode(conn: &Connection) -> Result<i32, AppError> {
        conn.query_row("PRAGMA auto_vacuum;", [], |row| row.get(0))
            .map_err(|e| AppError::Database(format!("读取 auto_vacuum 失败: {e}")))
    }

    fn has_user_tables(conn: &Connection) -> Result<bool, AppError> {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| AppError::Database(format!("读取表数量失败: {e}")))?;
        Ok(count > 0)
    }

    pub(crate) fn ensure_incremental_auto_vacuum_on_conn(
        conn: &Connection,
    ) -> Result<bool, AppError> {
        let mode = Self::get_auto_vacuum_mode(conn)?;
        if mode == 2 {
            return Ok(false);
        }

        let has_tables = Self::has_user_tables(conn)?;
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL;", [])
            .map_err(|e| AppError::Database(format!("设置 auto_vacuum 失败: {e}")))?;

        if !has_tables {
            return Ok(false);
        }

        conn.execute("VACUUM;", [])
            .map_err(|e| AppError::Database(format!("执行 VACUUM 失败: {e}")))?;
        conn.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| AppError::Database(format!("恢复 foreign_keys 失败: {e}")))?;
        Ok(true)
    }

    pub(crate) fn ensure_incremental_auto_vacuum(&self) -> Result<bool, AppError> {
        let mode = {
            let conn = lock_conn!(self.conn);
            Self::get_auto_vacuum_mode(&conn)?
        };
        if mode == 2 {
            return Ok(false);
        }

        let has_tables = {
            let conn = lock_conn!(self.conn);
            Self::has_user_tables(&conn)?
        };
        if has_tables {
            log::info!(
                "Detected auto_vacuum={mode}, rebuilding database to enable incremental vacuum"
            );
            self.backup_database_file()?;
        }

        let rebuilt = {
            let conn = lock_conn!(self.conn);
            Self::ensure_incremental_auto_vacuum_on_conn(&conn)?
        };

        if rebuilt {
            log::info!("Incremental auto-vacuum enabled after database rebuild");
        } else {
            log::info!("Incremental auto-vacuum configured for new database");
        }

        Ok(rebuilt)
    }

    /// 检查 MCP 服务器表是否为空
    pub fn is_mcp_table_empty(&self) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mcp_servers", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count == 0)
    }
}
