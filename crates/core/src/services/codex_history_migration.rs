//! Codex 第三方历史会话归桶迁移。
//!
//! 只迁移本机 `~/.codex` 历史数据；完成标记写入设备级 `settings.json`，
//! 失败时不写标记，下一次启动自动重试。

use crate::apps::codex::{
    OCHUB_CODEX_MODEL_PROVIDER_ID, get_codex_config_dir, read_codex_config_text,
};
use crate::db::{Database, is_official_seed_id};
use crate::error::AppError;
use crate::paths::{atomic_write, copy_file, get_app_config_dir, get_home_dir};
use crate::settings::{
    CodexOfficialHistoryUnifyMigration, CodexProviderTemplateMigration,
    CodexThirdPartyHistoryProviderBucketMigration,
};
use chrono::{Local, Utc};
use rusqlite::{Connection, backup::Backup, params_from_iter};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use toml_edit::DocumentMut;

const MIGRATION_NAME: &str = "codex-history-provider-migration-v1";
const OFFICIAL_UNIFY_MIGRATION_NAME: &str = "codex-official-history-unify-v1";
/// 还原操作自身的备份目录（与迁移备份分开，保持迁移账本目录纯净）。
const OFFICIAL_UNIFY_RESTORE_BACKUP_NAME: &str = "codex-official-history-unify-restore-v1";
const CODEX_STATE_DB_FILENAME: &str = "state_5.sqlite";
/// SQLite 变量上限保守值，IN 列表按此分块。
const STATE_DB_ID_CHUNK: usize = 500;

/// 串行化官方历史的迁移与还原：开启迁移（启动重试 + 设置保存后台任务）和
/// 关闭还原可能在毫秒级先后被触发，对同一批 jsonl / state DB 双向改写。
static CODEX_OFFICIAL_HISTORY_OP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_codex_official_history_op() -> std::sync::MutexGuard<'static, ()> {
    CODEX_OFFICIAL_HISTORY_OP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
/// Codex 内建默认 provider id：config.toml 没有 `model_provider` 键时会话归入此桶。
/// 官方订阅（ChatGPT OAuth / OpenAI API key）的历史会话都记录这个 id。
const OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID: &str = "openai";
const LEGACY_CC_SWITCH_CODEX_MODEL_PROVIDER_ID: &str = "ccswitch";
// If a Codex preset ever used a temporary routing key, keep that old key here
// so local history can be bucketed under the current custom provider id.
const CC_SWITCH_LEGACY_CODEX_MODEL_PROVIDER_IDS: &[&str] = &[
    LEGACY_CC_SWITCH_CODEX_MODEL_PROVIDER_ID,
    "aicodemirror",
    "aicoding",
    "aigocode",
    "aihubmix",
    "ark_agentplan",
    "bailian",
    "bailing",
    "byteplus",
    "claudecn",
    "compshare",
    "compshare_coding",
    "crazyrouter",
    "ctok",
    "cubence",
    "deepseek",
    "dmxapi",
    "doubaoseed",
    "eflowcode",
    "kimi",
    "lemondata",
    "longcat",
    "micu",
    "minimax",
    "minimax_en",
    "modelscope",
    "novita",
    "nvidia",
    "openrouter",
    "packycode",
    "patewayai",
    "pipellm",
    "qianfan_coding",
    "relaxycode",
    "rightcode",
    "runapi",
    "shengsuanyun",
    "siliconflow",
    "siliconflow_en",
    "sssaicode",
    "stepfun",
    "stepfun_en",
    "therouter",
    "xiaomi_mimo",
    "xiaomi_mimo_token_plan",
    "zhipu_glm",
    "zhipu_glm_en",
];

#[derive(Debug, Clone, Default)]
pub struct CodexHistoryProviderBucketMigrationOutcome {
    pub source_provider_ids: Vec<String>,
    pub migrated_jsonl_files: usize,
    pub migrated_state_rows: usize,
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexProviderTemplateBucketMigrationOutcome {
    pub migrated_provider_ids: Vec<String>,
    pub skipped_reason: Option<String>,
}

pub fn maybe_migrate_codex_third_party_history_provider_bucket(
    db: &Database,
) -> Result<CodexHistoryProviderBucketMigrationOutcome, AppError> {
    if crate::settings::is_codex_third_party_history_provider_bucket_migrated() {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("already_migrated".to_string()),
            ..Default::default()
        });
    }

    let source_provider_ids = collect_source_model_provider_ids(db)?;
    if source_provider_ids.is_empty() {
        crate::settings::mark_codex_third_party_history_provider_bucket_migrated(
            CodexThirdPartyHistoryProviderBucketMigration {
                completed_at: Utc::now().to_rfc3339(),
                target_provider_id: OCHUB_CODEX_MODEL_PROVIDER_ID.to_string(),
                source_provider_ids: Vec::new(),
                migrated_jsonl_files: 0,
                migrated_state_rows: 0,
                scanned_history_files: true,
            },
        )?;
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("no_third_party_provider_ids".to_string()),
            ..Default::default()
        });
    }

    let backup_root = migration_backup_root(MIGRATION_NAME);
    let codex_dir = get_codex_config_dir();
    let migrated_jsonl_files =
        migrate_codex_jsonl_files(&codex_dir, &source_provider_ids, &backup_root)?;
    let migrated_state_rows =
        migrate_codex_state_dbs(&codex_dir, &source_provider_ids, &backup_root)?;

    let source_provider_ids_vec: Vec<String> = source_provider_ids.iter().cloned().collect();
    crate::settings::mark_codex_third_party_history_provider_bucket_migrated(
        CodexThirdPartyHistoryProviderBucketMigration {
            completed_at: Utc::now().to_rfc3339(),
            target_provider_id: OCHUB_CODEX_MODEL_PROVIDER_ID.to_string(),
            source_provider_ids: source_provider_ids_vec.clone(),
            migrated_jsonl_files,
            migrated_state_rows,
            scanned_history_files: true,
        },
    )?;

    Ok(CodexHistoryProviderBucketMigrationOutcome {
        source_provider_ids: source_provider_ids_vec,
        migrated_jsonl_files,
        migrated_state_rows,
        skipped_reason: None,
    })
}

pub fn maybe_migrate_codex_provider_template_bucket(
    db: &Database,
) -> Result<CodexProviderTemplateBucketMigrationOutcome, AppError> {
    if crate::settings::is_codex_provider_template_migrated() {
        return Ok(CodexProviderTemplateBucketMigrationOutcome {
            skipped_reason: Some("already_migrated".to_string()),
            ..Default::default()
        });
    }

    let backup_root = migration_backup_root(MIGRATION_NAME);
    let outcome = migrate_codex_provider_templates_to_custom(db, &backup_root)?;
    crate::settings::mark_codex_provider_template_migrated(CodexProviderTemplateMigration {
        completed_at: Utc::now().to_rfc3339(),
        migrated_provider_ids: outcome.migrated_provider_ids.clone(),
    })?;

    Ok(outcome)
}

/// 统一会话开关的存量迁移：把官方会话（内建 "openai" 桶）迁入共享 "custom" 桶。
///
/// 仅当用户在开启弹窗里勾选了"迁入既有官方会话"（`unify_codex_migrate_existing`）
/// 且本轮未完成时执行；开关关闭时标记与勾选意愿都会被清除（见 `save_settings`），
/// 重新开启并再次勾选即可补迁关闭期间产生的官方会话。
/// custom 桶里官方与第三方会话无法区分，自动逻辑绝不反向搬回；
/// 用户可在关闭开关时选择按备份账本精确还原（见 `restore_codex_official_history_from_backups`）。
/// 迁移前 jsonl / state DB 均备份到 `~/.cc-switch/backups/codex-official-history-unify-v1/`。
pub fn maybe_migrate_codex_official_history_to_unified_bucket()
-> Result<CodexHistoryProviderBucketMigrationOutcome, AppError> {
    if !crate::settings::unify_codex_session_history() {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("unify_toggle_off".to_string()),
            ..Default::default()
        });
    }
    if !crate::settings::unify_codex_migrate_existing_requested() {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("stock_migration_not_requested".to_string()),
            ..Default::default()
        });
    }
    let _op_guard = lock_codex_official_history_op();
    let codex_dir = get_codex_config_dir();
    // marker 绑定迁移时的 Codex 目录：切换 codex_config_dir 后旧 marker 不再
    // 挡住新目录的迁移（迁移幂等，重跑无害）。
    let codex_dir_key = canonical_dir_string(&codex_dir);
    if crate::settings::is_codex_official_history_unify_migrated_for_dir(&codex_dir_key) {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("already_migrated".to_string()),
            ..Default::default()
        });
    }
    // live 必须已实际路由到共享 custom 桶才允许迁移：官方配置的注入可能被拒
    // （已有显式 model_provider / 形态冲突的 custom 表，见
    // `inject_codex_unified_session_bucket`），历史 live 也可能不带统一
    // 路由（注入只进备份）。这些状态下新会话仍落 "openai" 桶，迁移只会把
    // 历史搬进当前 live 看不见的桶里。开关与迁移意愿保持不动，待 live 真正
    // 统一后（下次切换或启动重试）再迁。
    if !codex_config_text_routes_custom(&read_codex_config_text().unwrap_or_default()) {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("live_not_unified".to_string()),
            ..Default::default()
        });
    }

    let source_provider_ids: BTreeSet<String> =
        std::iter::once(OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID.to_string()).collect();
    let backup_root = migration_backup_root(OFFICIAL_UNIFY_MIGRATION_NAME);
    let migrated_jsonl_files =
        migrate_codex_jsonl_files(&codex_dir, &source_provider_ids, &backup_root)?;
    let migrated_state_rows =
        migrate_codex_state_dbs(&codex_dir, &source_provider_ids, &backup_root)?;
    // 备份代际记录来源目录，restore 据此只取当前目录的账本。
    write_backup_generation_meta(&backup_root, &codex_dir_key)?;

    let outcome = CodexHistoryProviderBucketMigrationOutcome {
        source_provider_ids: source_provider_ids.into_iter().collect(),
        migrated_jsonl_files,
        migrated_state_rows,
        skipped_reason: None,
    };

    // 条件写入在 settings 写锁内原子完成："迁移期间开关被关掉"时不写完成标记，
    // 避免下一次开启被标记挡住而漏迁"关闭期间"新产生的 openai 桶会话。
    // 与关闭路径（update_settings + 清标记）共用同一把锁，无检查-写入窗口。
    let marker_written = crate::settings::mark_codex_official_history_unify_migrated_if_enabled(
        CodexOfficialHistoryUnifyMigration {
            completed_at: Utc::now().to_rfc3339(),
            target_provider_id: OCHUB_CODEX_MODEL_PROVIDER_ID.to_string(),
            migrated_jsonl_files,
            migrated_state_rows,
            codex_config_dir: Some(codex_dir_key),
        },
    )?;
    if !marker_written {
        return Ok(CodexHistoryProviderBucketMigrationOutcome {
            skipped_reason: Some("toggle_disabled_during_migration".to_string()),
            ..outcome
        });
    }

    Ok(outcome)
}

/// live config.toml 是否路由到共享 custom 桶（会话分桶只看这个实态：
/// base_url / 路由方式都不影响 session_meta 记录的 model_provider）。
fn codex_config_text_routes_custom(config_text: &str) -> bool {
    config_text
        .parse::<DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("model_provider")
                .and_then(|item| item.as_str())
                .map(|id| id.trim() == OCHUB_CODEX_MODEL_PROVIDER_ID)
        })
        .unwrap_or(false)
}

/// 目录的规范化字符串形式，用作 marker / 备份代际的目录身份。
/// canonicalize 失败（目录尚不存在等）时退回原始路径字符串。
fn canonical_dir_string(dir: &Path) -> String {
    fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// 在备份代际根目录写入 meta.json，记录这批备份来自哪个 Codex 目录。
/// 代际目录不存在（本轮没有任何文件被迁移）时跳过。
fn write_backup_generation_meta(backup_root: &Path, codex_dir_key: &str) -> Result<(), AppError> {
    if !backup_root.exists() {
        return Ok(());
    }
    let payload = serde_json::json!({ "codexConfigDir": codex_dir_key });
    let bytes =
        serde_json::to_vec_pretty(&payload).map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(&backup_root.join("meta.json"), &bytes)
}

#[derive(Debug, Clone, Default)]
pub struct CodexOfficialHistoryRestoreOutcome {
    pub restored_jsonl_files: usize,
    pub restored_state_rows: usize,
    pub skipped_reason: Option<String>,
}

/// 统一会话开关迁移备份的父目录（其下每次迁移一个时间戳代际目录）。
fn official_history_unify_backup_parent() -> PathBuf {
    get_app_config_dir()
        .join("backups")
        .join(OFFICIAL_UNIFY_MIGRATION_NAME)
}

/// 是否存在可用于还原的迁移备份（给前端决定要不要显示"恢复备份"勾选）。
/// 与 restore 的账本收集共用同一目录匹配口径：只认属于当前 Codex 目录的
/// 代际，避免切换 codex_config_dir 后弹出注定空跑的勾选。
/// 精确账本内容仍在真正还原时才解析。
pub fn has_codex_official_history_unify_backup() -> bool {
    has_official_history_unify_backup_for_dir(
        &official_history_unify_backup_parent(),
        &canonical_dir_string(&get_codex_config_dir()),
    )
}

fn has_official_history_unify_backup_for_dir(ledger_parent: &Path, codex_dir_key: &str) -> bool {
    let Ok(entries) = fs::read_dir(ledger_parent) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let generation = entry.path();
        generation.is_dir() && backup_generation_matches_dir(&generation, codex_dir_key)
    })
}

/// 关闭统一会话开关时的可选还原：按迁移备份账本，把当时迁入共享 custom 桶的
/// 官方会话精确翻回 "openai" 桶。
///
/// 备份是唯一可信的归属证据：备份里 model_provider=="openai" 的会话必定源自
/// 官方桶。开启期间新产生的会话不在任何备份里，**永不触碰**——它们可能来自
/// 第三方，方向无法判定（产品决策：宁可留在第三方历史）。
/// 扫描全部备份代际取并集，多次开关循环后仍能还原早期迁入的会话；
/// 还原前改动目标先备份到独立的 restore 目录（保持迁移账本目录纯净），
/// 且只改写当前仍为 custom 的目标，重复执行无害。
pub fn restore_codex_official_history_from_backups()
-> Result<CodexOfficialHistoryRestoreOutcome, AppError> {
    let _op_guard = lock_codex_official_history_op();
    // 开关已（重新）开启时拒绝还原：live 正路由 custom，把账本会话翻回
    // openai 桶等于亲手制造分裂。覆盖"关闭保存成功后用户立刻重新开启，
    // 还原排在重开迁移之后才拿到 op lock"的时序。
    if crate::settings::unify_codex_session_history() {
        return Ok(CodexOfficialHistoryRestoreOutcome {
            skipped_reason: Some("unify_toggle_on".to_string()),
            ..Default::default()
        });
    }
    let config_text = read_codex_config_text().unwrap_or_default();
    restore_codex_official_history_inner(
        &get_codex_config_dir(),
        &official_history_unify_backup_parent(),
        &migration_backup_root(OFFICIAL_UNIFY_RESTORE_BACKUP_NAME),
        &config_text,
    )
}

fn restore_codex_official_history_inner(
    codex_dir: &Path,
    ledger_parent: &Path,
    restore_backup_root: &Path,
    config_text: &str,
) -> Result<CodexOfficialHistoryRestoreOutcome, AppError> {
    let codex_dir_key = canonical_dir_string(codex_dir);
    let (official_session_ids, official_thread_ids) =
        collect_official_ledger(ledger_parent, &codex_dir_key)?;
    if official_session_ids.is_empty() && official_thread_ids.is_empty() {
        return Ok(CodexOfficialHistoryRestoreOutcome {
            skipped_reason: Some("no_backup_ledger".to_string()),
            ..Default::default()
        });
    }

    let mut files = Vec::new();
    collect_jsonl_files(&codex_dir.join("sessions"), &mut files, 0, 8);
    collect_jsonl_files(&codex_dir.join("archived_sessions"), &mut files, 0, 4);
    let mut restored_jsonl_files = 0;
    for file_path in files {
        if rewrite_codex_session_file_lines(&file_path, codex_dir, restore_backup_root, |line| {
            rewrite_codex_session_meta_line_for_restore(line, &official_session_ids)
        })? {
            restored_jsonl_files += 1;
        }
    }

    let mut restored_state_rows = 0;
    for db_path in codex_state_db_paths(codex_dir, config_text) {
        restored_state_rows += restore_codex_state_db_official_threads(
            &db_path,
            codex_dir,
            &official_thread_ids,
            restore_backup_root,
        )?;
    }

    if restored_jsonl_files == 0 && restored_state_rows == 0 {
        // 账本非空但没有任何"当前仍为 custom"的目标（如重复还原）：
        // 以 reason 告知前端，避免误报"已还原 0 项"为成功。
        return Ok(CodexOfficialHistoryRestoreOutcome {
            skipped_reason: Some("nothing_to_restore".to_string()),
            ..Default::default()
        });
    }

    Ok(CodexOfficialHistoryRestoreOutcome {
        restored_jsonl_files,
        restored_state_rows,
        skipped_reason: None,
    })
}

/// 从备份代际收集官方会话账本：jsonl 备份里 session_meta 为 "openai" 的
/// 会话 id + state DB 备份里 model_provider 为 "openai" 的 thread id。
/// 只采纳 meta.json 目录与当前 Codex 目录一致的代际，避免切换
/// codex_config_dir 后拿旧目录的账本作用到新目录。
/// 还原操作自身的备份（restore 目录）天然不会混入：那些副本里的 id 都是
/// custom，解析后贡献为空。
fn collect_official_ledger(
    ledger_parent: &Path,
    codex_dir_key: &str,
) -> Result<(HashSet<String>, BTreeSet<String>), AppError> {
    let mut session_ids = HashSet::new();
    let mut thread_ids = BTreeSet::new();
    let entries = match fs::read_dir(ledger_parent) {
        Ok(entries) => entries,
        Err(_) => return Ok((session_ids, thread_ids)),
    };
    for entry in entries.flatten() {
        let generation = entry.path();
        if !generation.is_dir() {
            continue;
        }
        if !backup_generation_matches_dir(&generation, codex_dir_key) {
            continue;
        }
        let mut backup_files = Vec::new();
        collect_jsonl_files(&generation.join("jsonl"), &mut backup_files, 0, 10);
        for backup_file in backup_files {
            collect_official_session_ids_from_backup(&backup_file, &mut session_ids);
        }
        let mut backup_dbs = Vec::new();
        collect_files_with_extension(&generation.join("state"), "sqlite", &mut backup_dbs, 0, 4);
        for backup_db in backup_dbs {
            collect_official_thread_ids_from_backup(&backup_db, &mut thread_ids);
        }
    }
    Ok((session_ids, thread_ids))
}

/// 备份代际是否属于指定 Codex 目录。无 meta.json 或解析失败时宽容接受：
/// 早期版本的备份没有 meta，而那个时期不存在切目录场景；误纳的代价也被
/// "按会话 id 精确匹配 + 仅改写 custom"双重条件兜底。
fn backup_generation_matches_dir(generation: &Path, codex_dir_key: &str) -> bool {
    let Ok(text) = fs::read_to_string(generation.join("meta.json")) else {
        return true;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("codexConfigDir")
                .and_then(Value::as_str)
                .map(|dir| dir == codex_dir_key)
        })
        .unwrap_or(true)
}

fn collect_official_session_ids_from_backup(path: &Path, session_ids: &mut HashSet<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        log::debug!("Failed to read unify backup file {}", path.display());
        return;
    };
    for line in content.lines() {
        if !line.contains("\"session_meta\"") || !line.contains("\"model_provider\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("model_provider").and_then(Value::as_str)
            != Some(OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID)
        {
            continue;
        }
        if let Some(session_id) = payload.get("id").and_then(Value::as_str) {
            session_ids.insert(session_id.to_string());
        }
    }
}

fn collect_official_thread_ids_from_backup(db_path: &Path, thread_ids: &mut BTreeSet<String>) {
    let conn =
        match Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(err) => {
                log::debug!(
                    "Failed to open unify backup state DB {}: {err}",
                    db_path.display()
                );
                return;
            }
        };
    let has_threads = Database::table_exists(&conn, "threads").unwrap_or(false)
        && Database::has_column(&conn, "threads", "model_provider").unwrap_or(false);
    if !has_threads {
        return;
    }
    let Ok(mut stmt) = conn.prepare("SELECT id FROM threads WHERE model_provider = ?1") else {
        return;
    };
    let Ok(rows) = stmt.query_map([OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID], |row| {
        row.get::<_, String>(0)
    }) else {
        return;
    };
    for thread_id in rows.flatten() {
        thread_ids.insert(thread_id);
    }
}

fn collect_files_with_extension(
    dir: &Path,
    extension: &str,
    files: &mut Vec<PathBuf>,
    depth: u8,
    max_depth: u8,
) {
    if depth > max_depth || !dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_extension(&path, extension, files, depth + 1, max_depth);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
            files.push(path);
        }
    }
}

fn rewrite_codex_session_meta_line_for_restore(
    line: &str,
    official_session_ids: &HashSet<String>,
) -> Option<String> {
    if !line.contains("\"session_meta\"") || !line.contains("\"model_provider\"") {
        return None;
    }
    let mut value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get_mut("payload")?.as_object_mut()?;
    if payload.get("model_provider")?.as_str()? != OCHUB_CODEX_MODEL_PROVIDER_ID {
        return None;
    }
    let session_id = payload.get("id")?.as_str()?;
    if !official_session_ids.contains(session_id) {
        return None;
    }
    payload.insert(
        "model_provider".to_string(),
        Value::String(OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID.to_string()),
    );
    serde_json::to_string(&value).ok()
}

fn restore_codex_state_db_official_threads(
    db_path: &Path,
    codex_dir: &Path,
    official_thread_ids: &BTreeSet<String>,
    backup_root: &Path,
) -> Result<usize, AppError> {
    if !db_path.exists() || official_thread_ids.is_empty() {
        return Ok(0);
    }

    let mut conn = Connection::open(db_path)
        .map_err(|e| AppError::Database(format!("打开 Codex state DB 失败: {e}")))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| AppError::Database(format!("设置 Codex state DB busy_timeout 失败: {e}")))?;

    if !Database::table_exists(&conn, "threads")?
        || !Database::has_column(&conn, "threads", "model_provider")?
    {
        return Ok(0);
    }

    let ids: Vec<&String> = official_thread_ids.iter().collect();
    let mut matching_rows: i64 = 0;
    for chunk in ids.chunks(STATE_DB_ID_CHUNK) {
        let placeholders = placeholders(chunk.len());
        let count_sql = format!(
            "SELECT COUNT(*) FROM threads WHERE model_provider = ? AND id IN ({placeholders})"
        );
        let mut values = Vec::with_capacity(chunk.len() + 1);
        values.push(OCHUB_CODEX_MODEL_PROVIDER_ID.to_string());
        values.extend(chunk.iter().map(|id| (*id).clone()));
        let count: i64 = conn
            .query_row(&count_sql, params_from_iter(values.iter()), |row| {
                row.get(0)
            })
            .map_err(|e| AppError::Database(format!("统计 Codex state DB 待还原行失败: {e}")))?;
        matching_rows += count;
    }
    if matching_rows == 0 {
        return Ok(0);
    }

    backup_codex_state_db(db_path, codex_dir, backup_root, &conn)?;

    let tx = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("开启 Codex state DB 还原事务失败: {e}")))?;
    let mut changed = 0;
    for chunk in ids.chunks(STATE_DB_ID_CHUNK) {
        let placeholders = placeholders(chunk.len());
        let update_sql = format!(
            "UPDATE threads SET model_provider = ? WHERE model_provider = ? AND id IN ({placeholders})"
        );
        let mut values = Vec::with_capacity(chunk.len() + 2);
        values.push(OFFICIAL_OPENAI_CODEX_MODEL_PROVIDER_ID.to_string());
        values.push(OCHUB_CODEX_MODEL_PROVIDER_ID.to_string());
        values.extend(chunk.iter().map(|id| (*id).clone()));
        changed += tx
            .execute(&update_sql, params_from_iter(values.iter()))
            .map_err(|e| AppError::Database(format!("还原 Codex state DB provider 失败: {e}")))?;
    }
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 Codex state DB 还原事务失败: {e}")))?;
    Ok(changed)
}

fn migrate_codex_provider_templates_to_custom(
    db: &Database,
    backup_root: &Path,
) -> Result<CodexProviderTemplateBucketMigrationOutcome, AppError> {
    let providers = db.get_all_providers("codex")?;
    let mut migrated_provider_ids = Vec::new();

    for (_, provider) in providers {
        if provider.category.as_deref() == Some("official")
            || is_official_seed_id(&provider.id)
            || provider.is_codex_oauth()
        {
            continue;
        }

        let Some(config_text) = provider
            .settings_config
            .get("config")
            .and_then(|value| value.as_str())
        else {
            continue;
        };

        let Some(migrated_config_text) = migrate_provider_config_template_to_custom(config_text)?
        else {
            continue;
        };

        let mut settings = provider.settings_config.clone();
        let Some(obj) = settings.as_object_mut() else {
            log::warn!(
                "Skipping Codex provider template migration for {}: settings_config is not an object",
                provider.id
            );
            continue;
        };
        backup_provider_settings_config(&provider.id, &provider.settings_config, backup_root)?;
        obj.insert("config".to_string(), Value::String(migrated_config_text));
        db.update_provider_settings_config("codex", &provider.id, &settings)?;
        migrated_provider_ids.push(provider.id);
    }

    Ok(CodexProviderTemplateBucketMigrationOutcome {
        migrated_provider_ids,
        skipped_reason: None,
    })
}

fn collect_source_model_provider_ids(db: &Database) -> Result<BTreeSet<String>, AppError> {
    let providers = db.get_all_providers("codex")?;
    let mut ids = BTreeSet::new();

    for provider in providers.values() {
        if provider.category.as_deref() == Some("official")
            || is_official_seed_id(&provider.id)
            || provider.is_codex_oauth()
        {
            continue;
        }

        insert_known_cc_switch_legacy_source_id(&mut ids, &provider.id);

        let Some(config_text) = provider
            .settings_config
            .get("config")
            .and_then(|value| value.as_str())
        else {
            continue;
        };

        for provider_id in trusted_legacy_codex_model_provider_ids_from_config(config_text) {
            insert_known_cc_switch_legacy_source_id(&mut ids, &provider_id);
        }
        if let Some(provider_id) =
            legacy_codex_model_provider_id_from_normalized_config(config_text)
        {
            insert_known_cc_switch_legacy_source_id(&mut ids, &provider_id);
        }
    }

    Ok(ids)
}

fn insert_known_cc_switch_legacy_source_id(ids: &mut BTreeSet<String>, provider_id: &str) {
    let trimmed = provider_id.trim();
    if is_known_cc_switch_legacy_codex_model_provider_id(trimmed) {
        ids.insert(trimmed.to_string());
    }
}

fn migration_backup_root(migration_name: &str) -> PathBuf {
    get_app_config_dir()
        .join("backups")
        .join(migration_name)
        .join(Local::now().format("%Y%m%d_%H%M%S").to_string())
}

fn is_known_cc_switch_legacy_codex_model_provider_id(provider_id: &str) -> bool {
    CC_SWITCH_LEGACY_CODEX_MODEL_PROVIDER_IDS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(provider_id))
}

fn legacy_codex_model_provider_id_from_normalized_config(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<DocumentMut>().ok()?;
    let provider_id = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)?;
    if provider_id != OCHUB_CODEX_MODEL_PROVIDER_ID
        && provider_id != LEGACY_CC_SWITCH_CODEX_MODEL_PROVIDER_ID
    {
        return None;
    }

    let name = doc
        .get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get(provider_id))
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("name"))
        .and_then(|item| item.as_str())?
        .trim();

    normalized_legacy_codex_provider_name(name).map(str::to_string)
}

fn normalized_legacy_codex_provider_name(name: &str) -> Option<&'static str> {
    if is_known_cc_switch_legacy_codex_model_provider_id(name) {
        return CC_SWITCH_LEGACY_CODEX_MODEL_PROVIDER_IDS
            .iter()
            .copied()
            .find(|known| known.eq_ignore_ascii_case(name));
    }

    match name {
        "E-FlowCode" => Some("eflowcode"),
        "PIPELLM" => Some("pipellm"),
        _ => None,
    }
}

fn trusted_legacy_codex_model_provider_ids_from_config(config_text: &str) -> BTreeSet<String> {
    let Ok(doc) = config_text.parse::<DocumentMut>() else {
        return BTreeSet::new();
    };

    trusted_legacy_codex_model_provider_ids_from_doc(&doc)
}

fn trusted_legacy_codex_model_provider_ids_from_doc(doc: &DocumentMut) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    insert_trusted_legacy_config_model_provider_id(&mut ids, doc, doc.get("model_provider"));

    if let Some(profiles) = doc.get("profiles").and_then(|item| item.as_table_like()) {
        for (_, profile_item) in profiles.iter() {
            if let Some(profile_table) = profile_item.as_table_like() {
                insert_trusted_legacy_config_model_provider_id(
                    &mut ids,
                    doc,
                    profile_table.get("model_provider"),
                );
            }
        }
    }

    ids
}

fn insert_trusted_legacy_config_model_provider_id(
    ids: &mut BTreeSet<String>,
    doc: &DocumentMut,
    item: Option<&toml_edit::Item>,
) {
    let Some(provider_id) = item.and_then(|item| item.as_str()).map(str::trim) else {
        return;
    };
    if provider_id.is_empty()
        || !is_known_cc_switch_legacy_codex_model_provider_id(provider_id)
        || !config_defines_model_provider(doc, provider_id)
    {
        return;
    }
    ids.insert(provider_id.to_string());
}

fn config_defines_model_provider(doc: &DocumentMut, provider_id: &str) -> bool {
    doc.get("model_providers")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get(provider_id))
        .and_then(|item| item.as_table())
        .is_some()
}

fn migrate_provider_config_template_to_custom(
    config_text: &str,
) -> Result<Option<String>, AppError> {
    if config_text.trim().is_empty() {
        return Ok(None);
    }

    let mut doc = config_text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Message(format!("Invalid Codex config.toml: {e}")))?;

    let source_provider_ids = trusted_legacy_codex_model_provider_ids_from_doc(&doc);
    if source_provider_ids.is_empty() {
        return Ok(None);
    }

    let active_provider_id = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|provider_id| !provider_id.is_empty())
        .map(str::to_string);

    let custom_table_exists = config_defines_model_provider(&doc, OCHUB_CODEX_MODEL_PROVIDER_ID);
    let source_provider_id_to_move = active_provider_id
        .as_deref()
        .filter(|provider_id| source_provider_ids.contains(*provider_id))
        .map(str::to_string)
        .or_else(|| {
            if custom_table_exists {
                None
            } else {
                source_provider_ids.iter().next().cloned()
            }
        });

    let mut changed = false;

    if let Some(source_provider_id) = source_provider_id_to_move {
        let Some(model_providers) = doc
            .get_mut("model_providers")
            .and_then(|item| item.as_table_mut())
        else {
            return Ok(None);
        };

        let Some(provider_table) = model_providers.remove(source_provider_id.as_str()) else {
            return Ok(None);
        };
        model_providers[OCHUB_CODEX_MODEL_PROVIDER_ID] = provider_table;
        changed = true;
    }

    if active_provider_id
        .as_deref()
        .is_some_and(|provider_id| source_provider_ids.contains(provider_id))
    {
        doc["model_provider"] = toml_edit::value(OCHUB_CODEX_MODEL_PROVIDER_ID);
        changed = true;
    }

    for source_provider_id in source_provider_ids {
        if rewrite_legacy_provider_profile_refs(&mut doc, source_provider_id.as_str()) {
            changed = true;
        }
    }

    if changed {
        Ok(Some(doc.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_legacy_provider_profile_refs(doc: &mut DocumentMut, source_provider_id: &str) -> bool {
    let Some(profiles) = doc
        .get_mut("profiles")
        .and_then(|item| item.as_table_like_mut())
    else {
        return false;
    };

    let mut changed = false;
    let profile_keys: Vec<String> = profiles.iter().map(|(key, _)| key.to_string()).collect();
    for profile_key in profile_keys {
        let Some(profile_table) = profiles
            .get_mut(&profile_key)
            .and_then(|item| item.as_table_like_mut())
        else {
            continue;
        };

        let references_legacy = profile_table
            .get("model_provider")
            .and_then(|item| item.as_str())
            == Some(source_provider_id);
        if references_legacy {
            profile_table.insert(
                "model_provider",
                toml_edit::value(OCHUB_CODEX_MODEL_PROVIDER_ID),
            );
            changed = true;
        }
    }
    changed
}

fn migrate_codex_jsonl_files(
    codex_dir: &Path,
    source_provider_ids: &BTreeSet<String>,
    backup_root: &Path,
) -> Result<usize, AppError> {
    let mut files = Vec::new();
    collect_jsonl_files(&codex_dir.join("sessions"), &mut files, 0, 8);
    collect_jsonl_files(&codex_dir.join("archived_sessions"), &mut files, 0, 4);

    let source_provider_ids: HashSet<String> = source_provider_ids.iter().cloned().collect();
    let mut migrated = 0;
    for file_path in files {
        if rewrite_codex_session_file_for_provider_bucket(
            &file_path,
            codex_dir,
            &source_provider_ids,
            backup_root,
        )? {
            migrated += 1;
        }
    }
    Ok(migrated)
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>, depth: u8, max_depth: u8) {
    if depth > max_depth || !dir.is_dir() {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            log::debug!(
                "Failed to read Codex session directory {}: {err}",
                dir.display()
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files, depth + 1, max_depth);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn rewrite_codex_session_file_for_provider_bucket(
    path: &Path,
    codex_dir: &Path,
    source_provider_ids: &HashSet<String>,
    backup_root: &Path,
) -> Result<bool, AppError> {
    rewrite_codex_session_file_lines(path, codex_dir, backup_root, |line| {
        rewrite_codex_session_meta_line(line, source_provider_ids)
    })
}

fn rewrite_codex_session_file_lines(
    path: &Path,
    codex_dir: &Path,
    backup_root: &Path,
    rewrite_line: impl Fn(&str) -> Option<String>,
) -> Result<bool, AppError> {
    let metadata_before = fs::metadata(path).map_err(|e| AppError::io(path, e))?;
    let modified_before = metadata_before.modified().ok();
    let len_before = metadata_before.len();
    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;

    let mut rewritten = String::with_capacity(content.len());
    let mut changed = false;
    for segment in content.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map(|line| (line, "\n"))
            .unwrap_or((segment, ""));
        if let Some(next_line) = rewrite_line(line) {
            rewritten.push_str(&next_line);
            changed = true;
        } else {
            rewritten.push_str(line);
        }
        rewritten.push_str(newline);
    }

    if !changed {
        return Ok(false);
    }

    ensure_codex_session_file_unchanged(path, modified_before, len_before)?;
    backup_codex_jsonl_file(path, codex_dir, backup_root)?;
    ensure_codex_session_file_unchanged(path, modified_before, len_before)?;
    atomic_write(path, rewritten.as_bytes())?;
    Ok(true)
}

fn ensure_codex_session_file_unchanged(
    path: &Path,
    modified_before: Option<SystemTime>,
    len_before: u64,
) -> Result<(), AppError> {
    let metadata_after = fs::metadata(path).map_err(|e| AppError::io(path, e))?;
    if metadata_after.modified().ok() != modified_before || metadata_after.len() != len_before {
        return Err(AppError::Message(format!(
            "Codex session file changed during migration: {}",
            path.display()
        )));
    }
    Ok(())
}

fn rewrite_codex_session_meta_line(
    line: &str,
    source_provider_ids: &HashSet<String>,
) -> Option<String> {
    if !line.contains("\"session_meta\"") || !line.contains("\"model_provider\"") {
        return None;
    }

    let mut value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }

    let payload = value.get_mut("payload")?.as_object_mut()?;
    let current_provider = payload.get("model_provider")?.as_str()?;
    if !source_provider_ids.contains(current_provider) {
        return None;
    }

    payload.insert(
        "model_provider".to_string(),
        Value::String(OCHUB_CODEX_MODEL_PROVIDER_ID.to_string()),
    );
    serde_json::to_string(&value).ok()
}

fn migrate_codex_state_dbs(
    codex_dir: &Path,
    source_provider_ids: &BTreeSet<String>,
    backup_root: &Path,
) -> Result<usize, AppError> {
    let config_text = read_codex_config_text().unwrap_or_default();
    let mut migrated = 0;
    for db_path in codex_state_db_paths(codex_dir, &config_text) {
        migrated += migrate_codex_state_db_provider_bucket(
            &db_path,
            codex_dir,
            source_provider_ids,
            backup_root,
        )?;
    }
    Ok(migrated)
}

fn codex_state_db_paths(codex_dir: &Path, config_text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    push_unique_path(&mut paths, codex_dir.join(CODEX_STATE_DB_FILENAME));
    // Codex lets SQLite state move away from CODEX_HOME; config takes precedence.
    if let Some(sqlite_home) = sqlite_home_from_codex_config(config_text) {
        push_unique_path(&mut paths, sqlite_home.join(CODEX_STATE_DB_FILENAME));
    } else if let Some(sqlite_home) = sqlite_home_from_env() {
        push_unique_path(&mut paths, sqlite_home.join(CODEX_STATE_DB_FILENAME));
    }
    paths
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn sqlite_home_from_codex_config(config_text: &str) -> Option<PathBuf> {
    let doc = config_text.parse::<DocumentMut>().ok()?;
    let raw = doc.get("sqlite_home")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(resolve_user_path(raw))
}

fn sqlite_home_from_env() -> Option<PathBuf> {
    let raw = std::env::var("CODEX_SQLITE_HOME").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(resolve_user_path(raw))
}

fn resolve_user_path(raw: &str) -> PathBuf {
    if raw == "~" {
        return get_home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return get_home_dir().join(rest);
    }
    if let Some(rest) = raw.strip_prefix("~\\") {
        return get_home_dir().join(rest);
    }
    PathBuf::from(raw)
}

fn migrate_codex_state_db_provider_bucket(
    db_path: &Path,
    codex_dir: &Path,
    source_provider_ids: &BTreeSet<String>,
    backup_root: &Path,
) -> Result<usize, AppError> {
    if !db_path.exists() || source_provider_ids.is_empty() {
        return Ok(0);
    }

    let mut conn = Connection::open(db_path)
        .map_err(|e| AppError::Database(format!("打开 Codex state DB 失败: {e}")))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| AppError::Database(format!("设置 Codex state DB busy_timeout 失败: {e}")))?;

    if !Database::table_exists(&conn, "threads")?
        || !Database::has_column(&conn, "threads", "model_provider")?
    {
        return Ok(0);
    }

    let placeholders = placeholders(source_provider_ids.len());
    let count_sql =
        format!("SELECT COUNT(*) FROM threads WHERE model_provider IN ({placeholders})");
    let matching_rows: i64 = conn
        .query_row(
            &count_sql,
            params_from_iter(source_provider_ids.iter()),
            |row| row.get(0),
        )
        .map_err(|e| AppError::Database(format!("统计 Codex state DB 待迁移行失败: {e}")))?;
    if matching_rows == 0 {
        return Ok(0);
    }

    backup_codex_state_db(db_path, codex_dir, backup_root, &conn)?;

    let update_sql =
        format!("UPDATE threads SET model_provider = ? WHERE model_provider IN ({placeholders})");
    let mut values = Vec::with_capacity(source_provider_ids.len() + 1);
    values.push(OCHUB_CODEX_MODEL_PROVIDER_ID.to_string());
    values.extend(source_provider_ids.iter().cloned());
    let tx = conn
        .transaction()
        .map_err(|e| AppError::Database(format!("开启 Codex state DB 迁移事务失败: {e}")))?;
    let changed = tx
        .execute(&update_sql, params_from_iter(values.iter()))
        .map_err(|e| AppError::Database(format!("迁移 Codex state DB provider 失败: {e}")))?;
    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 Codex state DB 迁移事务失败: {e}")))?;
    Ok(changed)
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

fn backup_codex_jsonl_file(
    path: &Path,
    codex_dir: &Path,
    backup_root: &Path,
) -> Result<(), AppError> {
    let backup_path = backup_root
        .join("jsonl")
        .join(relative_backup_path(path, codex_dir));
    copy_existing_file(path, &backup_path)
}

fn backup_codex_state_db(
    db_path: &Path,
    codex_dir: &Path,
    backup_root: &Path,
    source_conn: &Connection,
) -> Result<(), AppError> {
    let backup_path = backup_root
        .join("state")
        .join(relative_backup_path(db_path, codex_dir));
    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let mut backup_conn = Connection::open(&backup_path)
        .map_err(|e| AppError::Database(format!("创建 Codex state DB 备份失败: {e}")))?;
    let backup = Backup::new(source_conn, &mut backup_conn)
        .map_err(|e| AppError::Database(format!("初始化 Codex state DB 备份失败: {e}")))?;
    backup
        .run_to_completion(5, Duration::from_millis(25), None)
        .map_err(|e| AppError::Database(format!("写入 Codex state DB 备份失败: {e}")))?;
    Ok(())
}

fn backup_provider_settings_config(
    provider_id: &str,
    settings_config: &Value,
    backup_root: &Path,
) -> Result<(), AppError> {
    let backup_path = backup_root
        .join("providers")
        .join(provider_settings_backup_filename(provider_id));
    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let payload = serde_json::json!({
        "providerId": provider_id,
        "settingsConfig": settings_config,
    });
    let bytes =
        serde_json::to_vec_pretty(&payload).map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(&backup_path, &bytes)
}

fn provider_settings_backup_filename(provider_id: &str) -> String {
    let safe_id: String = provider_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe_id = if safe_id.is_empty() {
        "provider".to_string()
    } else {
        safe_id
    };
    // Keep the hash stable across processes while avoiding collisions after sanitization.
    let digest = Sha256::digest(provider_id.as_bytes());
    let hash = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{hash}-{safe_id}.settings_config.json")
}

fn copy_existing_file(source: &Path, target: &Path) -> Result<(), AppError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    copy_file(source, target)
}

fn relative_backup_path(path: &Path, root: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_path_buf();
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    PathBuf::from("external").join(format!("{hash:016x}-{file_name}"))
}
