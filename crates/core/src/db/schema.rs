//! Schema 定义和迁移
//!
//! 负责数据库表结构的创建和版本迁移。

use super::{lock_conn, Database, SCHEMA_VERSION};
use crate::error::AppError;
use rusqlite::Connection;

impl Database {
    /// 创建所有数据库表
    pub(crate) fn create_tables(&self) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        Self::create_tables_on_conn(&conn)
    }

    /// 在指定连接上创建表（供迁移和测试使用）
    pub(crate) fn create_tables_on_conn(conn: &Connection) -> Result<(), AppError> {
        // 1. Providers 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS providers (
                id TEXT NOT NULL,
                app_type TEXT NOT NULL,
                name TEXT NOT NULL,
                settings_config TEXT NOT NULL,
                website_url TEXT,
                category TEXT,
                created_at INTEGER,
                sort_index INTEGER,
                notes TEXT,
                icon TEXT,
                icon_color TEXT,
                meta TEXT NOT NULL DEFAULT '{}',
                is_current BOOLEAN NOT NULL DEFAULT 0,
                cost_multiplier TEXT NOT NULL DEFAULT '1.0',
                limit_daily_usd TEXT,
                limit_monthly_usd TEXT,
                provider_type TEXT,
                PRIMARY KEY (id, app_type)
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 2. Provider Endpoints 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS provider_endpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider_id TEXT NOT NULL,
                app_type TEXT NOT NULL,
                url TEXT NOT NULL,
                added_at INTEGER,
                FOREIGN KEY (provider_id, app_type) REFERENCES providers(id, app_type) ON DELETE CASCADE
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 3. MCP Servers 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS mcp_servers (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, server_config TEXT NOT NULL,
            description TEXT, homepage TEXT, docs TEXT, tags TEXT NOT NULL DEFAULT '[]',
            enabled_claude BOOLEAN NOT NULL DEFAULT 0, enabled_codex BOOLEAN NOT NULL DEFAULT 0,
            enabled_gemini BOOLEAN NOT NULL DEFAULT 0, enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0,
            enabled_opencode BOOLEAN NOT NULL DEFAULT 0,
            enabled_hermes BOOLEAN NOT NULL DEFAULT 0
        )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 4. Skills 表（v3.10.0+ 统一结构）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS skills (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            directory TEXT NOT NULL,
            repo_owner TEXT,
            repo_name TEXT,
            repo_branch TEXT DEFAULT 'main',
            readme_url TEXT,
            enabled_claude BOOLEAN NOT NULL DEFAULT 0,
            enabled_codex BOOLEAN NOT NULL DEFAULT 0,
            enabled_gemini BOOLEAN NOT NULL DEFAULT 0,
            enabled_opencode BOOLEAN NOT NULL DEFAULT 0,
            enabled_hermes BOOLEAN NOT NULL DEFAULT 0,
            installed_at INTEGER NOT NULL DEFAULT 0,
            content_hash TEXT,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 6. Skill Repos 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS skill_repos (
            owner TEXT NOT NULL, name TEXT NOT NULL, branch TEXT NOT NULL DEFAULT 'main',
            enabled BOOLEAN NOT NULL DEFAULT 1, PRIMARY KEY (owner, name)
        )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 7. Settings 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 8. Usage Logs 表
        // pricing_model = 写入时实际用于计价的模型名（pricing_model_source 解析结果），
        // 回填按它重算；NULL 表示 v11 之前的历史行，'' 表示未计价的错误行。
        conn.execute("CREATE TABLE IF NOT EXISTS usage_logs (
            request_id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, app_type TEXT NOT NULL, model TEXT NOT NULL,
            request_model TEXT,
            pricing_model TEXT,
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            input_cost_usd TEXT NOT NULL DEFAULT '0', output_cost_usd TEXT NOT NULL DEFAULT '0',
            cache_read_cost_usd TEXT NOT NULL DEFAULT '0', cache_creation_cost_usd TEXT NOT NULL DEFAULT '0',
            total_cost_usd TEXT NOT NULL DEFAULT '0', latency_ms INTEGER NOT NULL, first_token_ms INTEGER,
            duration_ms INTEGER, status_code INTEGER NOT NULL, error_message TEXT, session_id TEXT,
            provider_type TEXT, is_streaming INTEGER NOT NULL DEFAULT 0,
            cost_multiplier TEXT NOT NULL DEFAULT '1.0', created_at INTEGER NOT NULL,
            data_source TEXT NOT NULL DEFAULT 'gateway',
            input_token_semantics INTEGER NOT NULL DEFAULT 0
        )", []).map_err(|e| AppError::Database(e.to_string()))?;

        conn.execute("CREATE INDEX IF NOT EXISTS idx_request_logs_provider ON usage_logs(provider_id, app_type)", [])
            .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON usage_logs(created_at)",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_model ON usage_logs(model)",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_session ON usage_logs(session_id)",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_status ON usage_logs(status_code)",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Self::create_request_logs_usage_indexes_if_supported(conn)?;

        // 9. Manual model-pricing overrides. Remote LiteLLM data lives in the
        // separate replaceable catalog below and never overwrites these rows.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS model_pricing (
            model_id TEXT PRIMARY KEY, display_name TEXT NOT NULL,
            input_cost_per_million TEXT NOT NULL, output_cost_per_million TEXT NOT NULL,
            cache_read_cost_per_million TEXT NOT NULL DEFAULT '0',
            cache_creation_cost_per_million TEXT NOT NULL DEFAULT '0'
        )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS litellm_pricing_catalog (
                model_key TEXT PRIMARY KEY COLLATE NOCASE,
                provider TEXT NOT NULL,
                mode TEXT NOT NULL,
                input_cost_per_million TEXT NOT NULL,
                output_cost_per_million TEXT NOT NULL,
                cache_read_cost_per_million TEXT,
                cache_creation_cost_per_million TEXT,
                special_pricing_fields TEXT NOT NULL DEFAULT '[]',
                source_url TEXT
             );
             CREATE TABLE IF NOT EXISTS litellm_pricing_aliases (
                alias TEXT NOT NULL COLLATE NOCASE,
                model_key TEXT NOT NULL COLLATE NOCASE,
                PRIMARY KEY (alias, model_key),
                FOREIGN KEY (model_key) REFERENCES litellm_pricing_catalog(model_key)
                    ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS idx_litellm_pricing_alias
                 ON litellm_pricing_aliases(alias);
             CREATE TABLE IF NOT EXISTS litellm_pricing_meta (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                source_url TEXT NOT NULL,
                source_revision TEXT NOT NULL,
                source_generated_at TEXT NOT NULL,
                etag TEXT,
                updated_at INTEGER NOT NULL,
                checked_at INTEGER NOT NULL
             );",
        )
        .map_err(|e| AppError::Database(format!("创建 LiteLLM 定价目录失败: {e}")))?;

        // 10. Usage Daily Rollups 表 (日聚合统计)
        // request_model 保留网关路由的「客户端别名 → 真实模型」映射维度，
        // pricing_model 保留写入时的计价基准（request 计价模式下与 model 分叉），
        // 否则明细被 prune 后接管计费不可审计；历史行迁移时填 ''（未知）。
        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage_daily_rollups (
                date TEXT NOT NULL,
                app_type TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                model TEXT NOT NULL,
                request_model TEXT NOT NULL DEFAULT '',
                pricing_model TEXT NOT NULL DEFAULT '',
                request_count INTEGER NOT NULL DEFAULT 0,
                success_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                total_cost_usd TEXT NOT NULL DEFAULT '0',
                avg_latency_ms INTEGER NOT NULL DEFAULT 0,
                input_token_semantics INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (date, app_type, provider_id, model, request_model, pricing_model)
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 18. Session Log Sync 表 (会话日志同步状态)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS session_log_sync (
                file_path TEXT PRIMARY KEY,
                last_modified INTEGER NOT NULL,
                last_line_offset INTEGER NOT NULL DEFAULT 0,
                last_synced_at INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 19. Profiles 表（项目配置快照；对应 cc-switch v12。服务层/UI 尚未接入，
        // 建表以保证一次性导入时数据不丢失）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                payload TEXT NOT NULL,
                sort_order INTEGER,
                created_at INTEGER,
                updated_at INTEGER
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 14. Gateway 渠道表（本地网关的上游模型供应商渠道）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gateway_channels (
                id TEXT PRIMARY KEY,
                endpoint_id TEXT,
                name TEXT NOT NULL,
                dialect TEXT NOT NULL,
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                path_override TEXT,
                models TEXT NOT NULL DEFAULT '[]',
                model_override TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                weight INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                extra_headers TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                sort_index INTEGER,
                imported_from TEXT
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 15. Gateway 路由方案（按应用/客户端隔离上游、模型与思考映射）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gateway_routes (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                website_url TEXT,
                app_type TEXT,
                channel_ids TEXT NOT NULL DEFAULT '[]',
                default_model TEXT,
                model_rules TEXT NOT NULL DEFAULT '[]',
                reasoning TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                sort_index INTEGER
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 16. Gateway 本地 API key 表（一键配置分发给各应用，按 key 归因用量）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gateway_keys (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                key TEXT NOT NULL UNIQUE,
                route_id TEXT,
                model_policy TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 16. Per-app usage pricing defaults.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage_config (
                app_type TEXT PRIMARY KEY,
                default_cost_multiplier TEXT NOT NULL DEFAULT '1',
                pricing_model_source TEXT NOT NULL DEFAULT 'response'
            )",
            [],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;

        Ok(())
    }

    /// 应用 Schema 迁移
    pub(crate) fn apply_schema_migrations(&self) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        Self::apply_schema_migrations_on_conn(&conn)
    }

    /// 在指定连接上应用 Schema 迁移
    ///
    /// OcHub 拥有独立的版本线（从 v1 开始），与 cc-switch 的 user_version
    /// 序列无关。`create_tables_on_conn` 总是直接建出当前终态结构，因此
    /// user_version=0 只意味着"全新数据库"，直接打上当前版本号即可。
    /// 旧 cc-switch 数据通过一次性导入（`import_ccswitch`）进入，不走迁移。
    pub(crate) fn apply_schema_migrations_on_conn(conn: &Connection) -> Result<(), AppError> {
        conn.execute("SAVEPOINT schema_migration;", [])
            .map_err(|e| AppError::Database(format!("开启迁移 savepoint 失败: {e}")))?;

        let mut version = Self::get_user_version(conn)?;

        if version > SCHEMA_VERSION {
            conn.execute("ROLLBACK TO schema_migration;", []).ok();
            conn.execute("RELEASE schema_migration;", []).ok();
            return Err(AppError::Database(format!(
                "数据库版本过新（{version}），当前应用仅支持 {SCHEMA_VERSION}，请升级应用后再尝试。"
            )));
        }

        let result = (|| {
            while version < SCHEMA_VERSION {
                match version {
                    0 => {
                        // 全新数据库：表已按终态创建，打版本号即可
                        Self::set_user_version(conn, SCHEMA_VERSION)?;
                    }
                    1 => {
                        // v1 → v2：proxy_config 去掉 app_type 的 CHECK 枚举约束
                        //（开放应用 id）。SQLite 无法直接删 CHECK，需重建表。
                        conn.execute_batch(
                            "CREATE TABLE proxy_config_v2 (
                                app_type TEXT PRIMARY KEY,
                                proxy_enabled INTEGER NOT NULL DEFAULT 0, listen_address TEXT NOT NULL DEFAULT '127.0.0.1',
                                listen_port INTEGER NOT NULL DEFAULT 15721, enable_logging INTEGER NOT NULL DEFAULT 1,
                                enabled INTEGER NOT NULL DEFAULT 0, auto_failover_enabled INTEGER NOT NULL DEFAULT 0,
                                max_retries INTEGER NOT NULL DEFAULT 3, streaming_first_byte_timeout INTEGER NOT NULL DEFAULT 60,
                                streaming_idle_timeout INTEGER NOT NULL DEFAULT 120, non_streaming_timeout INTEGER NOT NULL DEFAULT 600,
                                circuit_failure_threshold INTEGER NOT NULL DEFAULT 4, circuit_success_threshold INTEGER NOT NULL DEFAULT 2,
                                circuit_timeout_seconds INTEGER NOT NULL DEFAULT 60, circuit_error_rate_threshold REAL NOT NULL DEFAULT 0.6,
                                circuit_min_requests INTEGER NOT NULL DEFAULT 10,
                                default_cost_multiplier TEXT NOT NULL DEFAULT '1',
                                pricing_model_source TEXT NOT NULL DEFAULT 'response',
                                live_takeover_active INTEGER NOT NULL DEFAULT 0,
                                created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                            );
                            INSERT INTO proxy_config_v2 SELECT * FROM proxy_config;
                            DROP TABLE proxy_config;
                            ALTER TABLE proxy_config_v2 RENAME TO proxy_config;",
                        )
                        .map_err(|e| {
                            AppError::Database(format!("proxy_config v1→v2 迁移失败: {e}"))
                        })?;
                        Self::set_user_version(conn, 2)?;
                    }
                    2 => {
                        // v2 → v3: retire the local proxy runtime persistence.
                        // Pricing defaults survive in their own usage domain.
                        if Self::table_exists(conn, "proxy_request_logs")? {
                            // `create_tables_on_conn` has already created the current target
                            // schema. Copy the column intersection rather than renaming the old
                            // table, so newly introduced columns receive their current defaults.
                            let target_columns = [
                                "request_id",
                                "provider_id",
                                "app_type",
                                "model",
                                "request_model",
                                "pricing_model",
                                "input_tokens",
                                "output_tokens",
                                "cache_read_tokens",
                                "cache_creation_tokens",
                                "input_cost_usd",
                                "output_cost_usd",
                                "cache_read_cost_usd",
                                "cache_creation_cost_usd",
                                "total_cost_usd",
                                "latency_ms",
                                "first_token_ms",
                                "duration_ms",
                                "status_code",
                                "error_message",
                                "session_id",
                                "provider_type",
                                "is_streaming",
                                "cost_multiplier",
                                "created_at",
                                "data_source",
                                "input_token_semantics",
                            ];
                            let mut common_columns = Vec::new();
                            for column in target_columns {
                                if Self::has_column(conn, "proxy_request_logs", column)? {
                                    common_columns.push(column);
                                }
                            }
                            if common_columns.is_empty() {
                                return Err(AppError::Database(
                                    "历史用量表没有可迁移字段".to_string(),
                                ));
                            }
                            let column_list = common_columns.join(", ");
                            let copy_sql = format!(
                                "INSERT OR IGNORE INTO usage_logs ({column_list})
                                 SELECT {column_list} FROM proxy_request_logs"
                            );
                            conn.execute(&copy_sql, []).map_err(|e| {
                                AppError::Database(format!("迁移历史用量记录失败: {e}"))
                            })?;
                            conn.execute("DROP TABLE proxy_request_logs", [])
                                .map_err(|e| {
                                    AppError::Database(format!("清理历史用量表失败: {e}"))
                                })?;
                        }
                        conn.execute_batch(
                            "CREATE TABLE IF NOT EXISTS usage_config (
                                app_type TEXT PRIMARY KEY,
                                default_cost_multiplier TEXT NOT NULL DEFAULT '1',
                                pricing_model_source TEXT NOT NULL DEFAULT 'response'
                             );
                             INSERT OR REPLACE INTO usage_config (
                                app_type, default_cost_multiplier, pricing_model_source
                             )
                             SELECT app_type, default_cost_multiplier, pricing_model_source
                             FROM proxy_config;
                             INSERT OR REPLACE INTO settings (key, value)
                             SELECT 'legacy_proxy_cleanup_pending', 'true'
                             WHERE EXISTS (SELECT 1 FROM proxy_live_backup)
                                OR EXISTS (SELECT 1 FROM proxy_config WHERE enabled = 1);
                             DELETE FROM settings
                             WHERE key = 'global_proxy_url'
                                OR key LIKE 'proxy_takeover_%'
                                OR key IN (
                                    'rectifier_config',
                                    'optimizer_config',
                                    'copilot_optimizer_config',
                                    'log_config'
                                );
                             DROP TABLE IF EXISTS provider_health;
                             DROP TABLE IF EXISTS proxy_live_backup;
                             DROP TABLE IF EXISTS proxy_config;
                             DROP INDEX IF EXISTS idx_providers_failover;",
                        )
                        .map_err(|e| AppError::Database(format!("移除旧代理数据结构失败: {e}")))?;
                        Self::create_request_logs_usage_indexes_if_supported(conn)?;
                        Self::set_user_version(conn, 3)?;
                    }
                    3 => {
                        conn.execute_batch(
                            "CREATE TABLE IF NOT EXISTS gateway_routes (
                                id TEXT PRIMARY KEY,
                                name TEXT NOT NULL,
                                app_type TEXT,
                                channel_ids TEXT NOT NULL DEFAULT '[]',
                                default_model TEXT,
                                model_rules TEXT NOT NULL DEFAULT '[]',
                                reasoning TEXT NOT NULL DEFAULT '{}',
                                enabled INTEGER NOT NULL DEFAULT 1,
                                created_at INTEGER NOT NULL,
                                sort_index INTEGER
                             );",
                        )
                        .map_err(|e| AppError::Database(format!("创建网关路由方案表失败: {e}")))?;
                        if !Self::has_column(conn, "gateway_keys", "route_id")? {
                            conn.execute("ALTER TABLE gateway_keys ADD COLUMN route_id TEXT", [])
                                .map_err(|e| {
                                    AppError::Database(format!("为网关 key 添加路由关联失败: {e}"))
                                })?;
                        }
                        Self::set_user_version(conn, 4)?;
                    }
                    4 => {
                        if Self::table_exists(conn, "gateway_channels")?
                            && !Self::has_column(conn, "gateway_channels", "imported_from")?
                        {
                            conn.execute(
                                "ALTER TABLE gateway_channels ADD COLUMN imported_from TEXT",
                                [],
                            )
                            .map_err(|e| {
                                AppError::Database(format!("为网关渠道添加导入来源失败: {e}"))
                            })?;
                        }
                        Self::set_user_version(conn, 5)?;
                    }
                    5 => {
                        if Self::table_exists(conn, "mcp_servers")?
                            && !Self::has_column(conn, "mcp_servers", "enabled_grokbuild")?
                        {
                            conn.execute(
                                "ALTER TABLE mcp_servers ADD COLUMN enabled_grokbuild BOOLEAN NOT NULL DEFAULT 0",
                                [],
                            )
                            .map_err(|e| {
                                AppError::Database(format!(
                                    "为 MCP 添加 Grok Build 启用状态失败: {e}"
                                ))
                            })?;
                        }
                        Self::set_user_version(conn, 6)?;
                    }
                    6 => {
                        // v6 and earlier mixed OcHub's seeded prices with user
                        // edits in one table and carried no provenance column.
                        // Retire only rows that still exactly match a known
                        // seed; any changed value or label is preserved as a
                        // manual override.
                        if Self::table_exists(conn, "model_pricing")? {
                            Self::remove_legacy_builtin_model_pricing(conn)?;
                            Self::remove_legacy_repaired_model_pricing(conn)?;
                        }
                        // The model connectivity probe was removed together
                        // with its settings and UI. Its transient history has
                        // no remaining reader, so retire the legacy table too.
                        conn.execute("DROP TABLE IF EXISTS stream_check_logs", [])
                            .map_err(|e| {
                                AppError::Database(format!("清理旧模型连通检测日志失败: {e}"))
                            })?;
                        Self::set_user_version(conn, 7)?;
                    }
                    7 => {
                        if Self::table_exists(conn, "gateway_channels")?
                            && !Self::has_column(conn, "gateway_channels", "endpoint_id")?
                        {
                            conn.execute(
                                "ALTER TABLE gateway_channels ADD COLUMN endpoint_id TEXT",
                                [],
                            )
                            .map_err(|e| {
                                AppError::Database(format!("为模型供应商渠道添加端点分组失败: {e}"))
                            })?;
                        }
                        if Self::table_exists(conn, "gateway_routes")?
                            && !Self::has_column(conn, "gateway_routes", "website_url")?
                        {
                            conn.execute(
                                "ALTER TABLE gateway_routes ADD COLUMN website_url TEXT",
                                [],
                            )
                            .map_err(|e| {
                                AppError::Database(format!("为模型供应商添加官网地址失败: {e}"))
                            })?;
                        }
                        Self::set_user_version(conn, 8)?;
                    }
                    8 => {
                        if Self::table_exists(conn, "gateway_keys")?
                            && !Self::has_column(conn, "gateway_keys", "model_policy")?
                        {
                            conn.execute(
                                "ALTER TABLE gateway_keys ADD COLUMN model_policy TEXT",
                                [],
                            )
                            .map_err(|e| {
                                AppError::Database(format!(
                                    "为应用模型供应商绑定添加模型策略失败: {e}"
                                ))
                            })?;
                        }
                        Self::set_user_version(conn, 9)?;
                    }
                    _ => {
                        return Err(AppError::Database(format!(
                            "未知的数据库版本 {version}，无法迁移到 {SCHEMA_VERSION}"
                        )));
                    }
                }
                version = Self::get_user_version(conn)?;
            }
            Ok(())
        })();

        match result {
            Ok(_) => {
                conn.execute("RELEASE schema_migration;", [])
                    .map_err(|e| AppError::Database(format!("提交迁移 savepoint 失败: {e}")))?;
                Ok(())
            }
            Err(e) => {
                conn.execute("ROLLBACK TO schema_migration;", []).ok();
                conn.execute("RELEASE schema_migration;", []).ok();
                Err(e)
            }
        }
    }

    /// One-time v6 → v7 retirement list for OcHub's former built-in prices.
    ///
    /// This is migration metadata, not an active price source. Exact tuple
    /// matching is the only signal the old schema gives us that a row was not
    /// edited by the user.
    fn remove_legacy_builtin_model_pricing(conn: &Connection) -> Result<(), AppError> {
        let pricing_data = [
            // Claude Fable 5（Opus 之上的新档）
            (
                "claude-fable-5",
                "Claude Fable 5",
                "10",
                "50",
                "1.00",
                "12.50",
            ),
            (
                "claude-mythos-5",
                "Claude Mythos 5",
                "10",
                "50",
                "1.00",
                "12.50",
            ),
            // Claude 4.8 系列
            (
                "claude-opus-4-8",
                "Claude Opus 4.8",
                "5",
                "25",
                "0.50",
                "6.25",
            ),
            // Claude 4.7 系列
            (
                "claude-opus-4-7",
                "Claude Opus 4.7",
                "5",
                "25",
                "0.50",
                "6.25",
            ),
            // Claude 4.6 系列
            (
                "claude-opus-4-6-20260206",
                "Claude Opus 4.6",
                "5",
                "25",
                "0.50",
                "6.25",
            ),
            (
                "claude-sonnet-4-6-20260217",
                "Claude Sonnet 4.6",
                "3",
                "15",
                "0.30",
                "3.75",
            ),
            // Claude 4.5 系列
            (
                "claude-opus-4-5-20251101",
                "Claude Opus 4.5",
                "5",
                "25",
                "0.50",
                "6.25",
            ),
            (
                "claude-sonnet-4-5-20250929",
                "Claude Sonnet 4.5",
                "3",
                "15",
                "0.30",
                "3.75",
            ),
            (
                "claude-haiku-4-5-20251001",
                "Claude Haiku 4.5",
                "1",
                "5",
                "0.10",
                "1.25",
            ),
            // Claude 4 系列 (Legacy Models)
            (
                "claude-opus-4-20250514",
                "Claude Opus 4",
                "15",
                "75",
                "1.50",
                "18.75",
            ),
            (
                "claude-opus-4-1-20250805",
                "Claude Opus 4.1",
                "15",
                "75",
                "1.50",
                "18.75",
            ),
            (
                "claude-sonnet-4-20250514",
                "Claude Sonnet 4",
                "3",
                "15",
                "0.30",
                "3.75",
            ),
            // Claude 3.5 系列
            (
                "claude-3-5-haiku-20241022",
                "Claude 3.5 Haiku",
                "0.80",
                "4",
                "0.08",
                "1",
            ),
            (
                "claude-3-5-sonnet-20241022",
                "Claude 3.5 Sonnet",
                "3",
                "15",
                "0.30",
                "3.75",
            ),
            // GPT-5.5 系列
            ("gpt-5.5", "GPT-5.5", "5", "30", "0.50", "0"),
            ("gpt-5.5-low", "GPT-5.5", "5", "30", "0.50", "0"),
            ("gpt-5.5-medium", "GPT-5.5", "5", "30", "0.50", "0"),
            ("gpt-5.5-high", "GPT-5.5", "5", "30", "0.50", "0"),
            ("gpt-5.5-xhigh", "GPT-5.5", "5", "30", "0.50", "0"),
            ("gpt-5.5-minimal", "GPT-5.5", "5", "30", "0.50", "0"),
            // GPT-5.4 系列
            ("gpt-5.4", "GPT-5.4", "2.50", "15", "0.25", "0"),
            ("gpt-5.4-mini", "GPT-5.4 Mini", "0.75", "4.50", "0.075", "0"),
            ("gpt-5.4-nano", "GPT-5.4 Nano", "0.20", "1.25", "0.02", "0"),
            // GPT-5.2 系列
            ("gpt-5.2", "GPT-5.2", "1.75", "14", "0.175", "0"),
            ("gpt-5.2-low", "GPT-5.2", "1.75", "14", "0.175", "0"),
            ("gpt-5.2-medium", "GPT-5.2", "1.75", "14", "0.175", "0"),
            ("gpt-5.2-high", "GPT-5.2", "1.75", "14", "0.175", "0"),
            ("gpt-5.2-xhigh", "GPT-5.2", "1.75", "14", "0.175", "0"),
            ("gpt-5.2-codex", "GPT-5.2 Codex", "1.75", "14", "0.175", "0"),
            (
                "gpt-5.2-codex-low",
                "GPT-5.2 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.2-codex-medium",
                "GPT-5.2 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.2-codex-high",
                "GPT-5.2 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.2-codex-xhigh",
                "GPT-5.2 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            // GPT-5.3 Codex 系列
            ("gpt-5.3-codex", "GPT-5.3 Codex", "1.75", "14", "0.175", "0"),
            (
                "gpt-5.3-codex-low",
                "GPT-5.3 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.3-codex-medium",
                "GPT-5.3 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.3-codex-high",
                "GPT-5.3 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            (
                "gpt-5.3-codex-xhigh",
                "GPT-5.3 Codex",
                "1.75",
                "14",
                "0.175",
                "0",
            ),
            // GPT-5.1 系列
            ("gpt-5.1", "GPT-5.1", "1.25", "10", "0.125", "0"),
            ("gpt-5.1-low", "GPT-5.1", "1.25", "10", "0.125", "0"),
            ("gpt-5.1-medium", "GPT-5.1", "1.25", "10", "0.125", "0"),
            ("gpt-5.1-high", "GPT-5.1", "1.25", "10", "0.125", "0"),
            ("gpt-5.1-minimal", "GPT-5.1", "1.25", "10", "0.125", "0"),
            ("gpt-5.1-codex", "GPT-5.1 Codex", "1.25", "10", "0.125", "0"),
            (
                "gpt-5.1-codex-mini",
                "GPT-5.1 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5.1-codex-max",
                "GPT-5.1 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5.1-codex-max-high",
                "GPT-5.1 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5.1-codex-max-xhigh",
                "GPT-5.1 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            // GPT-5 系列
            ("gpt-5", "GPT-5", "1.25", "10", "0.125", "0"),
            ("gpt-5-low", "GPT-5", "1.25", "10", "0.125", "0"),
            ("gpt-5-medium", "GPT-5", "1.25", "10", "0.125", "0"),
            ("gpt-5-high", "GPT-5", "1.25", "10", "0.125", "0"),
            ("gpt-5-minimal", "GPT-5", "1.25", "10", "0.125", "0"),
            ("gpt-5-codex", "GPT-5 Codex", "1.25", "10", "0.125", "0"),
            ("gpt-5-codex-low", "GPT-5 Codex", "1.25", "10", "0.125", "0"),
            (
                "gpt-5-codex-medium",
                "GPT-5 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5-codex-high",
                "GPT-5 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5-codex-mini",
                "GPT-5 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5-codex-mini-medium",
                "GPT-5 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gpt-5-codex-mini-high",
                "GPT-5 Codex",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            // OpenAI Reasoning 系列
            ("o3", "OpenAI o3", "2", "8", "0.50", "0"),
            ("o4-mini", "OpenAI o4-mini", "1.10", "4.40", "0.275", "0"),
            // GPT-4.1 系列
            ("gpt-4.1", "GPT-4.1", "2", "8", "0.50", "0"),
            ("gpt-4.1-mini", "GPT-4.1 Mini", "0.40", "1.60", "0.10", "0"),
            ("gpt-4.1-nano", "GPT-4.1 Nano", "0.10", "0.40", "0.025", "0"),
            // Gemini 3.5 系列
            (
                "gemini-3.5-flash",
                "Gemini 3.5 Flash",
                "1.50",
                "9.00",
                "0.15",
                "0",
            ),
            // Gemini 3.1 系列
            (
                "gemini-3.1-pro-preview",
                "Gemini 3.1 Pro Preview",
                "2",
                "12",
                "0.20",
                "0",
            ),
            (
                "gemini-3.1-flash-lite",
                "Gemini 3.1 Flash Lite",
                "0.25",
                "1.50",
                "0.025",
                "0",
            ),
            (
                "gemini-3.1-flash-lite-preview",
                "Gemini 3.1 Flash Lite Preview",
                "0.25",
                "1.50",
                "0.025",
                "0",
            ),
            // Gemini 3 系列
            (
                "gemini-3-pro-preview",
                "Gemini 3 Pro Preview",
                "2",
                "12",
                "0.2",
                "0",
            ),
            (
                "gemini-3-flash-preview",
                "Gemini 3 Flash Preview",
                "0.5",
                "3",
                "0.05",
                "0",
            ),
            // Gemini 2.5 系列
            (
                "gemini-2.5-pro",
                "Gemini 2.5 Pro",
                "1.25",
                "10",
                "0.125",
                "0",
            ),
            (
                "gemini-2.5-flash",
                "Gemini 2.5 Flash",
                "0.3",
                "2.5",
                "0.03",
                "0",
            ),
            (
                "gemini-2.5-flash-lite",
                "Gemini 2.5 Flash Lite",
                "0.10",
                "0.40",
                "0.01",
                "0",
            ),
            // Gemini 2.0 系列
            (
                "gemini-2.0-flash",
                "Gemini 2.0 Flash",
                "0.10",
                "0.40",
                "0.025",
                "0",
            ),
            // StepFun 系列
            (
                "step-3.7-flash",
                "Step 3.7 Flash",
                "0.19",
                "1.13",
                "0.04",
                "0",
            ),
            (
                "step-3.5-flash",
                "Step 3.5 Flash",
                "0.10",
                "0.30",
                "0.02",
                "0",
            ),
            (
                "step-3.5-flash-2603",
                "Step 3.5 Flash 2603",
                "0.10",
                "0.30",
                "0.02",
                "0",
            ),
            // ====== 国产模型 (USD/1M tokens) ======
            // Doubao (字节跳动)
            (
                "doubao-seed-code",
                "Doubao Seed Code",
                "0.17",
                "1.11",
                "0.02",
                "0",
            ),
            (
                "doubao-seed-2-0-pro",
                "Doubao Seed 2.0 Pro",
                "0.47",
                "2.37",
                "0.09",
                "0",
            ),
            (
                "doubao-seed-2-0-code",
                "Doubao Seed 2.0 Code",
                "0.47",
                "2.37",
                "0.09",
                "0",
            ),
            (
                "doubao-seed-2-0-code-preview-latest",
                "Doubao Seed 2.0 Code Preview",
                "0.47",
                "2.37",
                "0.09",
                "0",
            ),
            (
                "doubao-seed-2-0-lite",
                "Doubao Seed 2.0 Lite",
                "0.08",
                "0.50",
                "0.017",
                "0",
            ),
            (
                "doubao-seed-2-0-mini",
                "Doubao Seed 2.0 Mini",
                "0.03",
                "0.31",
                "0.0056",
                "0",
            ),
            // DeepSeek 系列
            (
                "deepseek-v3.2",
                "DeepSeek V3.2",
                "0.28",
                "0.42",
                "0.028",
                "0",
            ),
            (
                "deepseek-v3.1",
                "DeepSeek V3.1",
                "0.55",
                "1.67",
                "0.055",
                "0",
            ),
            ("deepseek-v3", "DeepSeek V3", "0.28", "1.11", "0.028", "0"),
            (
                "deepseek-chat",
                "DeepSeek Chat",
                "0.27",
                "1.10",
                "0.07",
                "0",
            ),
            (
                "deepseek-reasoner",
                "DeepSeek Reasoner",
                "0.55",
                "2.19",
                "0.14",
                "0",
            ),
            // DeepSeek V4 系列（官方 CNY 按 1 USD ≈ 7.14 折算）
            (
                "deepseek-v4-flash",
                "DeepSeek V4 Flash",
                "0.14",
                "0.28",
                "0.0028",
                "0",
            ),
            (
                "deepseek-v4-pro",
                "DeepSeek V4 Pro",
                "0.435",
                "0.87",
                "0.003625",
                "0",
            ),
            // Kimi (月之暗面)
            (
                "kimi-k2-thinking",
                "Kimi K2 Thinking",
                "0.55",
                "2.20",
                "0.10",
                "0",
            ),
            ("kimi-k2-0905", "Kimi K2", "0.55", "2.20", "0.10", "0"),
            (
                "kimi-k2-turbo",
                "Kimi K2 Turbo",
                "1.11",
                "8.06",
                "0.14",
                "0",
            ),
            ("kimi-k2.5", "Kimi K2.5", "0.60", "3.00", "0.10", "0"),
            ("kimi-k2.6", "Kimi K2.6", "0.95", "4.00", "0.16", "0"),
            (
                "kimi-k2.7-code",
                "Kimi K2.7 Code",
                "0.95",
                "4.00",
                "0.19",
                "0",
            ),
            // MiniMax 系列
            ("minimax-m2.1", "MiniMax M2.1", "0.27", "0.95", "0.03", "0"),
            (
                "minimax-m2.1-lightning",
                "MiniMax M2.1 Lightning",
                "0.27",
                "2.33",
                "0.03",
                "0",
            ),
            ("minimax-m2", "MiniMax M2", "0.27", "0.95", "0.03", "0"),
            ("minimax-m2.5", "MiniMax M2.5", "0.15", "0.95", "0.03", "0"),
            (
                "minimax-m2.5-lightning",
                "MiniMax M2.5 Lightning",
                "0.30",
                "2.40",
                "0.03",
                "0",
            ),
            (
                "minimax-m2.7",
                "MiniMax M2.7",
                "0.30",
                "1.20",
                "0.06",
                "0.375",
            ),
            (
                "minimax-m2.7-highspeed",
                "MiniMax M2.7 Highspeed",
                "0.60",
                "2.40",
                "0.06",
                "0.375",
            ),
            ("minimax-m3", "MiniMax M3", "0.60", "2.40", "0.12", "0"),
            // GLM (智谱)
            ("glm-4.7", "GLM-4.7", "0.6", "2.2", "0.11", "0"),
            ("glm-4.6", "GLM-4.6", "0.6", "2.2", "0.11", "0"),
            ("glm-5", "GLM-5", "1", "3.2", "0.2", "0"),
            ("glm-5.1", "GLM-5.1", "1.4", "4.4", "0.26", "0"),
            ("glm-5.2", "GLM-5.2", "1.4", "4.4", "0.26", "0"),
            // MiMo (小米)
            (
                "mimo-v2-flash",
                "MiMo V2 Flash",
                "0.09",
                "0.29",
                "0.009",
                "0",
            ),
            ("mimo-v2-pro", "MiMo V2 Pro", "0.435", "0.87", "0.0036", "0"),
            ("mimo-v2.5", "MiMo V2.5", "0.14", "0.29", "0.0028", "0"),
            (
                "mimo-v2.5-pro",
                "MiMo V2.5 Pro",
                "0.435",
                "0.87",
                "0.0036",
                "0",
            ),
            // Qwen 系列 (阿里巴巴)
            ("qwen3.7-max", "Qwen3.7 Max", "2.50", "7.50", "0.25", "0"),
            ("qwen3.7-plus", "Qwen3.7 Plus", "0.40", "1.60", "0.08", "0"),
            (
                "qwen3.6-plus",
                "Qwen3.6 Plus",
                "0.325",
                "1.95",
                "0.065",
                "0",
            ),
            ("qwen3.5-plus", "Qwen3.5 Plus", "0.26", "1.56", "0.052", "0"),
            ("qwen3-max", "Qwen3 Max", "0.78", "3.90", "0", "0"),
            (
                "qwen3-235b-a22b",
                "Qwen3 235B-A22B",
                "0.70",
                "8.40",
                "0",
                "0",
            ),
            (
                "qwen3-coder-plus",
                "Qwen3 Coder Plus",
                "0.65",
                "3.25",
                "0.13",
                "0",
            ),
            (
                "qwen3-coder-480b",
                "Qwen3 Coder 480B",
                "0.65",
                "3.25",
                "0",
                "0",
            ),
            (
                "qwen3-coder-480b-a35b-instruct",
                "Qwen3 Coder 480B-A35B Instruct",
                "0.65",
                "3.25",
                "0",
                "0",
            ),
            (
                "qwen3-coder-flash",
                "Qwen3 Coder Flash",
                "0.195",
                "0.975",
                "0.039",
                "0",
            ),
            (
                "qwen3-coder-next",
                "Qwen3 Coder Next",
                "0.12",
                "0.75",
                "0",
                "0",
            ),
            ("qwq-plus", "QwQ Plus", "0.80", "2.40", "0", "0"),
            ("qwq-32b", "QwQ 32B", "0.20", "0.60", "0", "0"),
            ("qwen3-32b", "Qwen3 32B", "0.16", "0.64", "0", "0"),
            // Grok 系列 (xAI)
            ("grok-4.3", "Grok 4.3", "1.25", "2.50", "0.20", "0"),
            (
                "grok-4.20-0309-reasoning",
                "Grok 4.20 Reasoning",
                "1.25",
                "2.50",
                "0.20",
                "0",
            ),
            (
                "grok-4.20-0309-non-reasoning",
                "Grok 4.20",
                "1.25",
                "2.50",
                "0.20",
                "0",
            ),
            (
                "grok-4-1-fast-reasoning",
                "Grok 4.1 Fast Reasoning",
                "0.20",
                "0.50",
                "0.05",
                "0",
            ),
            (
                "grok-4-1-fast-non-reasoning",
                "Grok 4.1 Fast",
                "0.20",
                "0.50",
                "0.05",
                "0",
            ),
            ("grok-4", "Grok 4", "3", "15", "0.75", "0"),
            (
                "grok-code-fast-1",
                "Grok Build 0.1 (Code Fast Alias)",
                "1",
                "2",
                "0.20",
                "0",
            ),
            ("grok-build-0.1", "Grok Build 0.1", "1", "2", "0.20", "0"),
            ("grok-3", "Grok 3", "3", "15", "0.75", "0"),
            ("grok-3-mini", "Grok 3 Mini", "0.25", "0.50", "0.075", "0"),
            // Mistral 系列
            (
                "mistral-medium-3.5",
                "Mistral Medium 3.5",
                "1.50",
                "7.50",
                "0",
                "0",
            ),
            (
                "mistral-small-4",
                "Mistral Small 4",
                "0.10",
                "0.30",
                "0.01",
                "0",
            ),
            (
                "devstral-small-2-2512",
                "Devstral Small 2",
                "0.10",
                "0.30",
                "0.01",
                "0",
            ),
            (
                "magistral-small",
                "Magistral Small",
                "0.50",
                "1.50",
                "0",
                "0",
            ),
            ("codestral-2508", "Codestral", "0.30", "0.90", "0.03", "0"),
            (
                "devstral-small-1.1",
                "Devstral Small 1.1",
                "0.07",
                "0.28",
                "0.01",
                "0",
            ),
            ("devstral-2-2512", "Devstral 2", "0.40", "2", "0.04", "0"),
            (
                "devstral-medium",
                "Devstral Medium",
                "0.40",
                "2",
                "0.04",
                "0",
            ),
            (
                "mistral-large-3-2512",
                "Mistral Large 3",
                "0.50",
                "1.50",
                "0.05",
                "0",
            ),
            (
                "mistral-medium-3.1",
                "Mistral Medium 3.1",
                "0.40",
                "2",
                "0.04",
                "0",
            ),
            (
                "mistral-small-3.2-24b",
                "Mistral Small 3.2",
                "0.075",
                "0.20",
                "0.01",
                "0",
            ),
            ("magistral-medium", "Magistral Medium", "2", "5", "0", "0"),
            // Cohere 系列
            ("command-a", "Cohere Command A", "2.50", "10", "0", "0"),
            (
                "command-r-plus",
                "Cohere Command R+",
                "2.50",
                "10",
                "0",
                "0",
            ),
            ("command-r", "Cohere Command R", "0.15", "0.60", "0", "0"),
            // OpenAI 补充
            ("o3-pro", "OpenAI o3-pro", "20", "80", "0", "0"),
            ("o3-mini", "OpenAI o3-mini", "0.55", "2.20", "0.55", "0"),
            ("o1", "OpenAI o1", "15", "60", "7.50", "0"),
            ("o1-mini", "OpenAI o1-mini", "0.55", "2.20", "0.55", "0"),
            ("codex-mini", "Codex Mini", "0.75", "3", "0.025", "0"),
            ("gpt-5-mini", "GPT-5 Mini", "0.25", "2", "0.025", "0"),
            ("gpt-5-nano", "GPT-5 Nano", "0.05", "0.40", "0.005", "0"),
        ];

        let mut stmt = conn
            .prepare(
                "DELETE FROM model_pricing
                 WHERE model_id = ?1
                   AND display_name = ?2
                   AND input_cost_per_million = ?3
                   AND output_cost_per_million = ?4
                   AND cache_read_cost_per_million = ?5
                   AND cache_creation_cost_per_million = ?6",
            )
            .map_err(|e| AppError::Database(format!("准备旧定价清理语句失败: {e}")))?;
        let mut removed = 0;
        for (model_id, display_name, input, output, cache_read, cache_creation) in pricing_data {
            removed += stmt
                .execute(rusqlite::params![
                    model_id,
                    display_name,
                    input,
                    output,
                    cache_read,
                    cache_creation
                ])
                .map_err(|e| AppError::Database(format!("清理旧定价 {model_id} 失败: {e}")))?;
        }

        log::info!("removed {removed} unchanged legacy built-in pricing rows");
        Ok(())
    }

    fn remove_legacy_repaired_model_pricing(conn: &Connection) -> Result<(), AppError> {
        let pricing_fixes = [
            // 2026-06-10 全量核价（厂商官方 list 价；CNY 按 ~7.14 折算）
            // GLM 4.6/4.7：旧值是第三方/OpenRouter 折扣价，统一到 Z.ai 官方（与 glm-5/5.1 一致）
            (
                "glm-4.7", "GLM-4.7", "0.6", "2.2", "0.11", "0", "0.39", "1.75", "0.04", "0",
            ),
            (
                "glm-4.6", "GLM-4.6", "0.6", "2.2", "0.11", "0", "0.28", "1.11", "0.03", "0",
            ),
            // Grok 4.20：xAI 已降价 2/6 → 1.25/2.50
            (
                "grok-4.20-0309-reasoning",
                "Grok 4.20 Reasoning",
                "1.25",
                "2.50",
                "0.20",
                "0",
                "2",
                "6",
                "0.20",
                "0",
            ),
            (
                "grok-4.20-0309-non-reasoning",
                "Grok 4.20",
                "1.25",
                "2.50",
                "0.20",
                "0",
                "2",
                "6",
                "0.20",
                "0",
            ),
            // Kimi K2.5 官方 output 3.00
            (
                "kimi-k2.5",
                "Kimi K2.5",
                "0.60",
                "3.00",
                "0.10",
                "0",
                "0.60",
                "2.50",
                "0.10",
                "0",
            ),
            // MiniMax M2.5 input 0.15
            (
                "minimax-m2.5",
                "MiniMax M2.5",
                "0.15",
                "0.95",
                "0.03",
                "0",
                "0.12",
                "0.95",
                "0.03",
                "0",
            ),
            // Mistral Devstral 2 output 0.90 → 2（与同表 devstral-medium 一致）
            (
                "devstral-2-2512",
                "Devstral 2",
                "0.40",
                "2",
                "0.04",
                "0",
                "0.40",
                "0.90",
                "0.04",
                "0",
            ),
            // Doubao Seed 2.0：lite 旧价贵 3-4 倍 + 全系补 cache 命中价
            (
                "doubao-seed-2-0-lite",
                "Doubao Seed 2.0 Lite",
                "0.08",
                "0.50",
                "0.017",
                "0",
                "0.25",
                "2",
                "0",
                "0",
            ),
            (
                "doubao-seed-2-0-pro",
                "Doubao Seed 2.0 Pro",
                "0.47",
                "2.37",
                "0.09",
                "0",
                "0.47",
                "2.37",
                "0",
                "0",
            ),
            (
                "doubao-seed-2-0-code",
                "Doubao Seed 2.0 Code",
                "0.47",
                "2.37",
                "0.09",
                "0",
                "0.47",
                "2.37",
                "0",
                "0",
            ),
            (
                "doubao-seed-2-0-code-preview-latest",
                "Doubao Seed 2.0 Code Preview",
                "0.47",
                "2.37",
                "0.09",
                "0",
                "0.47",
                "2.37",
                "0",
                "0",
            ),
            (
                "doubao-seed-2-0-mini",
                "Doubao Seed 2.0 Mini",
                "0.03",
                "0.31",
                "0.0056",
                "0",
                "0.03",
                "0.31",
                "0",
                "0",
            ),
            // MiMo：5/27 永久降价，旧值是旧价
            (
                "mimo-v2-pro",
                "MiMo V2 Pro",
                "0.435",
                "0.87",
                "0.0036",
                "0",
                "1",
                "3",
                "0",
                "0",
            ),
            (
                "mimo-v2.5",
                "MiMo V2.5",
                "0.14",
                "0.29",
                "0.0028",
                "0",
                "0.09",
                "0.29",
                "0.009",
                "0",
            ),
            (
                "mimo-v2.5-pro",
                "MiMo V2.5 Pro",
                "0.435",
                "0.87",
                "0.0036",
                "0",
                "1",
                "3",
                "0",
                "0",
            ),
            // Qwen：官方"隐式缓存 = 输入 20%"补 cache 命中价
            (
                "qwen3.6-plus",
                "Qwen3.6 Plus",
                "0.325",
                "1.95",
                "0.065",
                "0",
                "0.325",
                "1.95",
                "0",
                "0",
            ),
            (
                "qwen3.5-plus",
                "Qwen3.5 Plus",
                "0.26",
                "1.56",
                "0.052",
                "0",
                "0.26",
                "1.56",
                "0",
                "0",
            ),
            (
                "qwen3-coder-plus",
                "Qwen3 Coder Plus",
                "0.65",
                "3.25",
                "0.13",
                "0",
                "0.65",
                "3.25",
                "0",
                "0",
            ),
            (
                "qwen3-coder-flash",
                "Qwen3 Coder Flash",
                "0.195",
                "0.975",
                "0.039",
                "0",
                "0.195",
                "0.975",
                "0",
                "0",
            ),
            (
                "deepseek-v4-flash",
                "DeepSeek V4 Flash",
                "0.14",
                "0.28",
                "0.0028",
                "0",
                "0.14",
                "0.28",
                "0.028",
                "0",
            ),
            (
                "deepseek-v4-pro",
                "DeepSeek V4 Pro",
                "0.435",
                "0.87",
                "0.003625",
                "0",
                "1.68",
                "3.36",
                "0.14",
                "0",
            ),
            (
                "glm-5", "GLM-5", "1", "3.2", "0.2", "0", "0.72", "2.30", "0", "0",
            ),
            (
                "glm-5.1", "GLM-5.1", "1.4", "4.4", "0.26", "0", "0.95", "3.15", "0", "0",
            ),
            (
                "grok-code-fast-1",
                "Grok Build 0.1 (Code Fast Alias)",
                "1",
                "2",
                "0.20",
                "0",
                "0.20",
                "1.50",
                "0.02",
                "0",
            ),
        ];

        for (
            model_id,
            display_name,
            input,
            output,
            cache_read,
            cache_creation,
            old_input,
            old_output,
            old_cache_read,
            old_cache_creation,
        ) in pricing_fixes
        {
            conn.execute(
                "DELETE FROM model_pricing
                 WHERE model_id = ?1
                   AND display_name = ?2
                   AND (
                       (input_cost_per_million = ?3
                        AND output_cost_per_million = ?4
                        AND cache_read_cost_per_million = ?5
                        AND cache_creation_cost_per_million = ?6)
                       OR
                       (input_cost_per_million = ?7
                        AND output_cost_per_million = ?8
                        AND cache_read_cost_per_million = ?9
                        AND cache_creation_cost_per_million = ?10)
                   )",
                rusqlite::params![
                    model_id,
                    display_name,
                    input,
                    output,
                    cache_read,
                    cache_creation,
                    old_input,
                    old_output,
                    old_cache_read,
                    old_cache_creation
                ],
            )
            .map_err(|e| AppError::Database(format!("清理旧修复定价 {model_id} 失败: {e}")))?;
        }

        Ok(())
    }

    // --- 辅助方法 ---

    pub(crate) fn get_user_version(conn: &Connection) -> Result<i32, AppError> {
        conn.query_row("PRAGMA user_version;", [], |row| row.get(0))
            .map_err(|e| AppError::Database(format!("读取 user_version 失败: {e}")))
    }

    pub(crate) fn set_user_version(conn: &Connection, version: i32) -> Result<(), AppError> {
        if version < 0 {
            return Err(AppError::Database("user_version 不能为负数".to_string()));
        }
        let sql = format!("PRAGMA user_version = {version};");
        conn.execute(&sql, [])
            .map_err(|e| AppError::Database(format!("写入 user_version 失败: {e}")))?;
        Ok(())
    }

    fn create_request_logs_usage_indexes_if_supported(conn: &Connection) -> Result<(), AppError> {
        if !Self::table_exists(conn, "usage_logs")? {
            return Ok(());
        }

        let has_app_type = Self::has_column(conn, "usage_logs", "app_type")?;
        let has_created_at = Self::has_column(conn, "usage_logs", "created_at")?;
        if has_app_type && has_created_at {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_request_logs_app_created_at
                 ON usage_logs(app_type, created_at DESC)",
                [],
            )
            .map_err(|e| AppError::Database(format!("创建使用量应用时间索引失败: {e}")))?;
        }

        let required_columns = [
            "app_type",
            "data_source",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "created_at",
            "cache_creation_tokens",
        ];
        for column in required_columns {
            if !Self::has_column(conn, "usage_logs", column)? {
                return Ok(());
            }
        }

        conn.execute("DROP INDEX IF EXISTS idx_request_logs_dedup_lookup", [])
            .map_err(|e| AppError::Database(format!("删除旧使用量去重索引失败: {e}")))?;

        // 查询层为了兼容历史 NULL data_source 行，会使用
        // COALESCE(data_source, 'proxy')。普通 data_source 索引无法匹配该表达式，
        // 会让跨源去重子查询退化成大量扫描；表达式索引让 SQLite 能按同一表达式查找。
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_dedup_lookup_expr
             ON usage_logs(app_type, COALESCE(data_source, 'proxy'), input_tokens,
                                   output_tokens, cache_read_tokens, created_at,
                                   cache_creation_tokens)",
            [],
        )
        .map_err(|e| AppError::Database(format!("创建使用量去重表达式索引失败: {e}")))?;
        Ok(())
    }

    fn validate_identifier(s: &str, kind: &str) -> Result<(), AppError> {
        if s.is_empty() {
            return Err(AppError::Database(format!("{kind} 不能为空")));
        }
        if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(AppError::Database(format!(
                "非法{kind}: {s}，仅允许字母、数字和下划线"
            )));
        }
        Ok(())
    }

    pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool, AppError> {
        Self::validate_identifier(table, "表名")?;

        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(|e| AppError::Database(format!("读取表名失败: {e}")))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Database(format!("查询表名失败: {e}")))?;
        while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
            let name: String = row
                .get(0)
                .map_err(|e| AppError::Database(format!("解析表名失败: {e}")))?;
            if name.eq_ignore_ascii_case(table) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn has_column(
        conn: &Connection,
        table: &str,
        column: &str,
    ) -> Result<bool, AppError> {
        Self::validate_identifier(table, "表名")?;
        Self::validate_identifier(column, "列名")?;

        let sql = format!("PRAGMA table_info(\"{table}\");");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(format!("读取表结构失败: {e}")))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Database(format!("查询表结构失败: {e}")))?;
        while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
            let name: String = row
                .get(1)
                .map_err(|e| AppError::Database(format!("读取列名失败: {e}")))?;
            if name.eq_ignore_ascii_case(column) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 迁移工具函数：为未来 OcHub v1→v2+ 的加列迁移保留
    #[allow(dead_code)]
    fn add_column_if_missing(
        conn: &Connection,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<bool, AppError> {
        Self::validate_identifier(table, "表名")?;
        Self::validate_identifier(column, "列名")?;

        if !Self::table_exists(conn, table)? {
            return Err(AppError::Database(format!(
                "表 {table} 不存在，无法添加列 {column}"
            )));
        }
        if Self::has_column(conn, table, column)? {
            return Ok(false);
        }

        let sql = format!("ALTER TABLE \"{table}\" ADD COLUMN \"{column}\" {definition};");
        conn.execute(&sql, [])
            .map_err(|e| AppError::Database(format!("为表 {table} 添加列 {column} 失败: {e}")))?;
        log::info!("已为表 {table} 添加缺失列 {column}");
        Ok(true)
    }
}

#[cfg(test)]
mod schema_migration_tests {
    use super::super::{Database, SCHEMA_VERSION};
    use rusqlite::Connection;

    /// Build the minimal legacy v1 structures needed to exercise the full v1 → v4 path.
    fn v1_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE proxy_config (
                app_type TEXT PRIMARY KEY CHECK (app_type IN ('claude','codex','gemini')),
                proxy_enabled INTEGER NOT NULL DEFAULT 0, listen_address TEXT NOT NULL DEFAULT '127.0.0.1',
                listen_port INTEGER NOT NULL DEFAULT 15721, enable_logging INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 0, auto_failover_enabled INTEGER NOT NULL DEFAULT 0,
                max_retries INTEGER NOT NULL DEFAULT 3, streaming_first_byte_timeout INTEGER NOT NULL DEFAULT 60,
                streaming_idle_timeout INTEGER NOT NULL DEFAULT 120, non_streaming_timeout INTEGER NOT NULL DEFAULT 600,
                circuit_failure_threshold INTEGER NOT NULL DEFAULT 4, circuit_success_threshold INTEGER NOT NULL DEFAULT 2,
                circuit_timeout_seconds INTEGER NOT NULL DEFAULT 60, circuit_error_rate_threshold REAL NOT NULL DEFAULT 0.6,
                circuit_min_requests INTEGER NOT NULL DEFAULT 10,
                default_cost_multiplier TEXT NOT NULL DEFAULT '1',
                pricing_model_source TEXT NOT NULL DEFAULT 'response',
                live_takeover_active INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO proxy_config (
                app_type, max_retries, enabled, default_cost_multiplier, pricing_model_source
            ) VALUES ('claude', 6, 1, '1.25', 'request');
            INSERT INTO proxy_config (app_type, max_retries) VALUES ('codex', 3);
            CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO settings VALUES ('global_proxy_url', 'socks5://127.0.0.1:1080');
            INSERT INTO settings VALUES ('proxy_takeover_claude', 'true');
            INSERT INTO settings VALUES ('rectifier_config', '{}');
            CREATE TABLE proxy_live_backup (
                app_type TEXT PRIMARY KEY, original_config TEXT NOT NULL, backed_up_at TEXT NOT NULL
            );
            INSERT INTO proxy_live_backup VALUES ('claude', '{}', 'now');
            CREATE TABLE proxy_request_logs (
                request_id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL,
                app_type TEXT NOT NULL,
                model TEXT NOT NULL,
                latency_ms INTEGER NOT NULL,
                status_code INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            INSERT INTO proxy_request_logs VALUES ('legacy-1', 'p1', 'claude', 'm1', 10, 200, 1);
            PRAGMA user_version = 1;",
        )
        .unwrap();
        conn
    }

    #[test]
    fn migrates_legacy_proxy_data_to_gateway_usage_schema() {
        let conn = v1_conn();
        // v1 rejects unknown app ids
        assert!(conn
            .execute("INSERT INTO proxy_config (app_type) VALUES ('my-app')", [])
            .is_err());

        Database::create_tables_on_conn(&conn).unwrap();
        Database::apply_schema_migrations_on_conn(&conn).unwrap();
        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);

        // Pricing preferences survive in the neutral usage domain.
        let (multiplier, source): (String, String) = conn
            .query_row(
                "SELECT default_cost_multiplier, pricing_model_source
                 FROM usage_config WHERE app_type = 'claude'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((multiplier.as_str(), source.as_str()), ("1.25", "request"));

        // Historical request rows are retained under the gateway-neutral table name.
        let request_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(request_count, 1);
        let migrated_source: String = conn
            .query_row(
                "SELECT data_source FROM usage_logs WHERE request_id = 'legacy-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrated_source, "gateway");

        let cleanup_pending: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'legacy_proxy_cleanup_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cleanup_pending, "true");

        assert!(!Database::table_exists(&conn, "proxy_config").unwrap());
        assert!(!Database::table_exists(&conn, "proxy_live_backup").unwrap());
        assert!(!Database::table_exists(&conn, "proxy_request_logs").unwrap());
        for key in [
            "global_proxy_url",
            "proxy_takeover_claude",
            "rectifier_config",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM settings WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "retired setting {key} should be removed");
        }
    }

    #[test]
    fn migrates_v3_gateway_keys_to_route_profiles() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_keys (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                key TEXT NOT NULL UNIQUE,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL
             );
             INSERT INTO gateway_keys VALUES ('key-1', 'claude', 'rd-existing', 1, 10);
             PRAGMA user_version = 3;",
        )
        .unwrap();

        Database::apply_schema_migrations_on_conn(&conn).unwrap();

        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(Database::table_exists(&conn, "gateway_routes").unwrap());
        assert!(Database::has_column(&conn, "gateway_keys", "route_id").unwrap());
        let (secret, route_id): (String, Option<String>) = conn
            .query_row(
                "SELECT key, route_id FROM gateway_keys WHERE id = 'key-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(secret, "rd-existing");
        assert!(route_id.is_none());
    }

    #[test]
    fn migrates_v4_gateway_channels_to_import_origin() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_channels (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                dialect TEXT NOT NULL,
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                path_override TEXT,
                models TEXT NOT NULL DEFAULT '[]',
                model_override TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                weight INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                extra_headers TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                sort_index INTEGER
             );
             INSERT INTO gateway_channels (id, name, dialect, base_url, created_at)
             VALUES ('c1', 'relay', 'chat', 'https://x', 1);
             PRAGMA user_version = 4;",
        )
        .unwrap();

        Database::apply_schema_migrations_on_conn(&conn).unwrap();

        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(Database::has_column(&conn, "gateway_channels", "imported_from").unwrap());
        let origin: Option<String> = conn
            .query_row(
                "SELECT imported_from FROM gateway_channels WHERE id = 'c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(origin.is_none());
    }

    #[test]
    fn migrates_v6_prices_to_manual_overrides_without_reseeding() {
        let conn = Connection::open_in_memory().unwrap();
        Database::create_tables_on_conn(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO model_pricing (
                model_id, display_name, input_cost_per_million,
                output_cost_per_million, cache_read_cost_per_million,
                cache_creation_cost_per_million
             ) VALUES
                ('gpt-5', 'GPT-5', '1.25', '10', '0.125', '0'),
                ('gpt-5-mini', 'My GPT-5 Mini', '9', '9', '0', '0');
             CREATE TABLE stream_check_logs (
                id INTEGER PRIMARY KEY,
                model_id TEXT NOT NULL
             );
             PRAGMA user_version = 6;",
        )
        .unwrap();

        Database::apply_schema_migrations_on_conn(&conn).unwrap();

        let retired: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM model_pricing WHERE model_id = 'gpt-5'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let preserved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM model_pricing WHERE model_id = 'gpt-5-mini'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired, 0);
        assert_eq!(preserved, 1);
        assert!(!Database::table_exists(&conn, "stream_check_logs").unwrap());
        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migrates_v7_gateway_stations_to_endpoint_groups_and_websites() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_channels (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                dialect TEXT NOT NULL,
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                path_override TEXT,
                models TEXT NOT NULL DEFAULT '[]',
                model_override TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                weight INTEGER NOT NULL DEFAULT 1,
                enabled INTEGER NOT NULL DEFAULT 1,
                extra_headers TEXT NOT NULL DEFAULT '[]',
                imported_from TEXT,
                created_at INTEGER NOT NULL,
                sort_index INTEGER
             );
             CREATE TABLE gateway_routes (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                app_type TEXT,
                channel_ids TEXT NOT NULL DEFAULT '[]',
                default_model TEXT,
                model_rules TEXT NOT NULL DEFAULT '[]',
                reasoning TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                sort_index INTEGER
             );
             INSERT INTO gateway_channels (
                id, name, dialect, base_url, created_at
             ) VALUES ('c1', 'Relay', 'messages', 'https://api.example.com', 1);
             INSERT INTO gateway_routes (
                id, name, channel_ids, created_at
             ) VALUES ('station:c1', 'Relay', '[\"c1\"]', 1);
             PRAGMA user_version = 7;",
        )
        .unwrap();

        Database::apply_schema_migrations_on_conn(&conn).unwrap();

        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(Database::has_column(&conn, "gateway_channels", "endpoint_id").unwrap());
        assert!(Database::has_column(&conn, "gateway_routes", "website_url").unwrap());
        let endpoint_id: Option<String> = conn
            .query_row(
                "SELECT endpoint_id FROM gateway_channels WHERE id = 'c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let website_url: Option<String> = conn
            .query_row(
                "SELECT website_url FROM gateway_routes WHERE id = 'station:c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(endpoint_id.is_none());
        assert!(website_url.is_none());
    }

    #[test]
    fn migrates_v8_gateway_keys_to_per_app_model_policies() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gateway_keys (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                key TEXT NOT NULL UNIQUE,
                route_id TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL
             );
             INSERT INTO gateway_keys
                (id, name, key, route_id, enabled, created_at)
             VALUES
                ('key-1', 'codex:station:relay', 'rd-existing', 'station:relay', 1, 10);
             PRAGMA user_version = 8;",
        )
        .unwrap();

        Database::apply_schema_migrations_on_conn(&conn).unwrap();

        assert_eq!(Database::get_user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(Database::has_column(&conn, "gateway_keys", "model_policy").unwrap());
        let (secret, route_id, model_policy): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT key, route_id, model_policy
                 FROM gateway_keys WHERE id = 'key-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(secret, "rd-existing");
        assert_eq!(route_id.as_deref(), Some("station:relay"));
        assert!(model_policy.is_none());
    }
}
