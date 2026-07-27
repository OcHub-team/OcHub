//! LiteLLM-backed model pricing catalog.
//!
//! The bundled snapshot gives every install an offline last-known-good
//! catalog. Network refreshes replace only these tables; user-authored prices
//! remain in `model_pricing` and always win during resolution.

use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use once_cell::sync::{Lazy, OnceCell};
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH, USER_AGENT};
use reqwest::StatusCode;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use super::usage_stats::{find_model_pricing_row_for_requirements, model_pricing_candidates};
use crate::db::{lock_conn, Database};
use crate::error::AppError;

pub const LITELLM_PRICING_SOURCE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const LITELLM_LICENSE_URL: &str = "https://github.com/BerriAI/litellm/blob/main/LICENSE";
const REFRESH_INTERVAL_SECONDS: i64 = 24 * 60 * 60;
const FAILED_REFRESH_THROTTLE_SECONDS: i64 = 5 * 60;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const SUPPORTED_MODES: &[&str] = &["chat", "completion"];
const SPECIAL_PRICE_MARKERS: &[&str] = &[
    "_above_",
    "_priority",
    "_flex",
    "_batches",
    "_batch",
    "_fast",
];
const BASE_PRICE_FIELDS: &[&str] = &[
    "input_cost_per_token",
    "output_cost_per_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
];

static REFRESH_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static LAST_REFRESH_ATTEMPT: AtomicI64 = AtomicI64::new(0);
static REFRESH_TRIGGER: OnceCell<mpsc::Sender<()>> = OnceCell::new();

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingCatalogSource {
    pub name: String,
    pub url: String,
    pub revision: String,
    pub generated_at: String,
    pub license: String,
    pub license_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingCatalogEntry {
    pub model_key: String,
    pub provider: String,
    pub mode: String,
    pub input_cost_per_million: String,
    pub output_cost_per_million: String,
    pub cache_read_cost_per_million: Option<String>,
    pub cache_creation_cost_per_million: Option<String>,
    #[serde(default)]
    pub special_pricing_fields: Vec<String>,
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PricingCatalogSnapshot {
    pub schema_version: u32,
    pub source: PricingCatalogSource,
    pub entries: Vec<PricingCatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MissingPricingModel {
    pub model_id: String,
    pub request_count: u64,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PricingCatalogStatus {
    pub entry_count: u32,
    pub source_url: Option<String>,
    pub source_revision: Option<String>,
    pub source_generated_at: Option<String>,
    pub updated_at: Option<i64>,
    pub checked_at: Option<i64>,
    pub missing_models: Vec<MissingPricingModel>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PricingCatalogRefreshKind {
    Skipped,
    NotModified,
    Updated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCatalogRefreshOutcome {
    pub kind: PricingCatalogRefreshKind,
    pub entry_count: u32,
    pub source_revision: Option<String>,
    pub backfilled_rows: u64,
}

#[derive(Debug, Clone)]
struct PricingCatalogMeta {
    source_url: String,
    source_revision: String,
    source_generated_at: String,
    etag: Option<String>,
    updated_at: i64,
    checked_at: i64,
}

#[derive(Debug, Clone)]
pub struct PricingCatalogInstallOutcome {
    pub installed: bool,
    pub entry_count: u32,
    pub source_revision: String,
}

impl Database {
    /// Install a packaged snapshot unless the local catalog is the same or
    /// newer. A catalog refreshed after the app was packaged is never rolled
    /// back on the next launch.
    pub fn install_bundled_pricing_catalog(
        &self,
        json: &str,
    ) -> Result<PricingCatalogInstallOutcome, AppError> {
        let snapshot = parse_snapshot(json)?;
        self.replace_pricing_catalog(&snapshot, None, 0, true)
    }

    pub fn get_pricing_catalog_status(&self) -> Result<PricingCatalogStatus, AppError> {
        let conn = lock_conn!(self.conn);
        let meta = pricing_catalog_meta_on_conn(&conn)?;
        let entry_count = conn
            .query_row("SELECT COUNT(*) FROM litellm_pricing_catalog", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| AppError::Database(format!("读取 LiteLLM 目录数量失败: {error}")))?
            as u32;
        let missing_models = collect_missing_pricing_models(&conn)?;

        Ok(PricingCatalogStatus {
            entry_count,
            source_url: meta.as_ref().map(|item| item.source_url.clone()),
            source_revision: meta.as_ref().map(|item| item.source_revision.clone()),
            source_generated_at: meta.as_ref().map(|item| item.source_generated_at.clone()),
            updated_at: meta.as_ref().map(|item| item.updated_at),
            checked_at: meta.as_ref().map(|item| item.checked_at),
            missing_models,
        })
    }

    /// Page through the installed LiteLLM catalog. This is a local-only read;
    /// callers must invoke [`refresh_pricing_catalog`] explicitly to access
    /// the network.
    pub fn list_pricing_catalog(
        &self,
        query: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<PricingCatalogEntry>, AppError> {
        let conn = lock_conn!(self.conn);
        let pattern = format!("%{}%", query.unwrap_or_default().trim());
        let limit = limit.clamp(1, 1_000);
        let mut statement = conn
            .prepare(
                "SELECT model_key, provider, mode,
                        input_cost_per_million, output_cost_per_million,
                        cache_read_cost_per_million, cache_creation_cost_per_million,
                        special_pricing_fields, source_url
                 FROM litellm_pricing_catalog
                 WHERE model_key LIKE ?1 COLLATE NOCASE
                    OR provider LIKE ?1 COLLATE NOCASE
                 ORDER BY model_key ASC
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(|error| AppError::Database(format!("读取 LiteLLM 价格目录失败: {error}")))?;
        let rows = statement
            .query_map(params![pattern, limit, offset], |row| {
                let special: String = row.get(7)?;
                Ok(PricingCatalogEntry {
                    model_key: row.get(0)?,
                    provider: row.get(1)?,
                    mode: row.get(2)?,
                    input_cost_per_million: row.get(3)?,
                    output_cost_per_million: row.get(4)?,
                    cache_read_cost_per_million: row.get(5)?,
                    cache_creation_cost_per_million: row.get(6)?,
                    special_pricing_fields: serde_json::from_str(&special).unwrap_or_default(),
                    source_url: row.get(8)?,
                })
            })
            .map_err(|error| AppError::Database(format!("读取 LiteLLM 价格目录失败: {error}")))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Database(format!("读取 LiteLLM 价格目录失败: {error}")))
    }

    fn replace_pricing_catalog(
        &self,
        snapshot: &PricingCatalogSnapshot,
        etag: Option<&str>,
        checked_at: i64,
        only_if_newer: bool,
    ) -> Result<PricingCatalogInstallOutcome, AppError> {
        validate_snapshot(snapshot)?;
        let mut conn = lock_conn!(self.conn);

        if only_if_newer {
            if let Some(current) = pricing_catalog_meta_on_conn(&conn)? {
                let current_count =
                    conn.query_row("SELECT COUNT(*) FROM litellm_pricing_catalog", [], |row| {
                        row.get::<_, i64>(0)
                    })? as u32;
                if current_count > 0
                    && (current.source_revision == snapshot.source.revision
                        || current.source_generated_at >= snapshot.source.generated_at)
                {
                    return Ok(PricingCatalogInstallOutcome {
                        installed: false,
                        entry_count: current_count,
                        source_revision: current.source_revision,
                    });
                }
            }
        }

        let now = Utc::now().timestamp();
        let transaction = conn
            .transaction()
            .map_err(|error| AppError::Database(format!("启动价格目录事务失败: {error}")))?;
        transaction
            .execute("DELETE FROM litellm_pricing_aliases", [])
            .map_err(|error| AppError::Database(format!("清空价格目录别名失败: {error}")))?;
        transaction
            .execute("DELETE FROM litellm_pricing_catalog", [])
            .map_err(|error| AppError::Database(format!("清空价格目录失败: {error}")))?;

        {
            let mut entry_statement = transaction
                .prepare(
                    "INSERT INTO litellm_pricing_catalog (
                        model_key, provider, mode,
                        input_cost_per_million, output_cost_per_million,
                        cache_read_cost_per_million, cache_creation_cost_per_million,
                        special_pricing_fields, source_url
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(|error| AppError::Database(format!("准备价格目录写入失败: {error}")))?;
            let mut alias_statement = transaction
                .prepare(
                    "INSERT OR IGNORE INTO litellm_pricing_aliases (alias, model_key)
                     VALUES (?1, ?2)",
                )
                .map_err(|error| {
                    AppError::Database(format!("准备价格目录别名写入失败: {error}"))
                })?;

            for entry in &snapshot.entries {
                let special_pricing_fields = serde_json::to_string(&entry.special_pricing_fields)
                    .map_err(|error| {
                    AppError::Config(format!("序列化特殊计价字段失败: {error}"))
                })?;
                entry_statement
                    .execute(params![
                        entry.model_key,
                        entry.provider,
                        entry.mode,
                        entry.input_cost_per_million,
                        entry.output_cost_per_million,
                        entry.cache_read_cost_per_million,
                        entry.cache_creation_cost_per_million,
                        special_pricing_fields,
                        entry.source_url,
                    ])
                    .map_err(|error| {
                        AppError::Database(format!(
                            "写入 LiteLLM 模型 {} 失败: {error}",
                            entry.model_key
                        ))
                    })?;

                for alias in model_pricing_candidates(&entry.model_key) {
                    alias_statement
                        .execute(params![alias, entry.model_key])
                        .map_err(|error| {
                            AppError::Database(format!(
                                "写入 LiteLLM 模型别名 {} 失败: {error}",
                                entry.model_key
                            ))
                        })?;
                }
            }
        }

        transaction
            .execute(
                "INSERT INTO litellm_pricing_meta (
                    singleton, source_url, source_revision, source_generated_at,
                    etag, updated_at, checked_at
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(singleton) DO UPDATE SET
                    source_url = excluded.source_url,
                    source_revision = excluded.source_revision,
                    source_generated_at = excluded.source_generated_at,
                    etag = excluded.etag,
                    updated_at = excluded.updated_at,
                    checked_at = excluded.checked_at",
                params![
                    snapshot.source.url,
                    snapshot.source.revision,
                    snapshot.source.generated_at,
                    etag,
                    now,
                    checked_at,
                ],
            )
            .map_err(|error| AppError::Database(format!("写入价格目录元数据失败: {error}")))?;
        transaction
            .commit()
            .map_err(|error| AppError::Database(format!("提交价格目录事务失败: {error}")))?;

        Ok(PricingCatalogInstallOutcome {
            installed: true,
            entry_count: snapshot.entries.len() as u32,
            source_revision: snapshot.source.revision.clone(),
        })
    }

    fn mark_pricing_catalog_checked(
        &self,
        checked_at: i64,
        etag: Option<&str>,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE litellm_pricing_meta
             SET checked_at = ?1, etag = COALESCE(?2, etag)
             WHERE singleton = 1",
            params![checked_at, etag],
        )
        .map_err(|error| AppError::Database(format!("更新价格目录检查时间失败: {error}")))?;
        Ok(())
    }

    fn pricing_catalog_meta(&self) -> Result<Option<PricingCatalogMeta>, AppError> {
        let conn = lock_conn!(self.conn);
        pricing_catalog_meta_on_conn(&conn)
    }
}

/// Fetch the stable LiteLLM catalog with conditional HTTP and atomically
/// replace the local catalog on success.
pub async fn refresh_pricing_catalog(
    db: Arc<Database>,
    force: bool,
) -> Result<PricingCatalogRefreshOutcome, AppError> {
    let _guard = REFRESH_LOCK.lock().await;
    let now = Utc::now().timestamp();
    let last_attempt = LAST_REFRESH_ATTEMPT.load(Ordering::Relaxed);
    let meta = db.pricing_catalog_meta()?;

    if !force
        && ((meta
            .as_ref()
            .is_some_and(|item| now - item.checked_at < REFRESH_INTERVAL_SECONDS))
            || (last_attempt > 0 && now - last_attempt < FAILED_REFRESH_THROTTLE_SECONDS))
    {
        let status = db.get_pricing_catalog_status()?;
        return Ok(PricingCatalogRefreshOutcome {
            kind: PricingCatalogRefreshKind::Skipped,
            entry_count: status.entry_count,
            source_revision: status.source_revision,
            backfilled_rows: 0,
        });
    }
    LAST_REFRESH_ATTEMPT.store(now, Ordering::Relaxed);

    let mut request = crate::http_client::get()
        .get(LITELLM_PRICING_SOURCE_URL)
        .header(USER_AGENT, "OcHub pricing catalog")
        .header(ACCEPT, "application/json")
        .timeout(Duration::from_secs(30));
    if let Some(etag) = meta.as_ref().and_then(|item| item.etag.as_deref()) {
        request = request.header(IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .await
        .map_err(|error| AppError::msg(format!("同步 LiteLLM 价格目录失败: {error}")))?;
    let response_etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    if response.status() == StatusCode::NOT_MODIFIED {
        db.mark_pricing_catalog_checked(now, response_etag.as_deref())?;
        let status = db.get_pricing_catalog_status()?;
        return Ok(PricingCatalogRefreshOutcome {
            kind: PricingCatalogRefreshKind::NotModified,
            entry_count: status.entry_count,
            source_revision: status.source_revision,
            backfilled_rows: 0,
        });
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::HttpStatus {
            status,
            body: body.chars().take(512).collect(),
        });
    }

    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(AppError::Config(format!(
            "LiteLLM 价格目录超过大小限制: {} bytes",
            response.content_length().unwrap_or_default()
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| AppError::msg(format!("读取 LiteLLM 价格目录失败: {error}")))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            return Err(AppError::Config(format!(
                "LiteLLM 价格目录超过大小限制: >{MAX_CATALOG_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let revision = response_etag
        .as_deref()
        .map(|value| value.trim_matches('"').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("fetched-{now}"));
    let snapshot = snapshot_from_upstream(&bytes, &revision, now)?;
    let installed = db.replace_pricing_catalog(&snapshot, response_etag.as_deref(), now, false)?;
    let backfilled_rows = db.backfill_missing_usage_costs()?;

    Ok(PricingCatalogRefreshOutcome {
        kind: PricingCatalogRefreshKind::Updated,
        entry_count: installed.entry_count,
        source_revision: Some(installed.source_revision),
        backfilled_rows,
    })
}

/// Start the one process-wide refresh worker. Request-path misses only signal
/// this bounded channel; they never perform network I/O themselves.
pub fn start_background_pricing_sync(db: Arc<Database>) {
    if REFRESH_TRIGGER.get().is_some() {
        return;
    }
    let (sender, mut receiver) = mpsc::channel(1);
    if REFRESH_TRIGGER.set(sender).is_err() {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = refresh_pricing_catalog(db.clone(), false).await {
            log::warn!("initial LiteLLM pricing refresh failed: {error}");
        }

        let start =
            tokio::time::Instant::now() + Duration::from_secs(REFRESH_INTERVAL_SECONDS as u64);
        let mut interval =
            tokio::time::interval_at(start, Duration::from_secs(REFRESH_INTERVAL_SECONDS as u64));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                message = receiver.recv() => {
                    if message.is_none() {
                        break;
                    }
                    while receiver.try_recv().is_ok() {}
                }
            }
            if let Err(error) = refresh_pricing_catalog(db.clone(), false).await {
                log::warn!("LiteLLM pricing refresh failed: {error}");
            }
        }
    });
}

pub(crate) fn notify_pricing_catalog_miss() {
    if let Some(sender) = REFRESH_TRIGGER.get() {
        let _ = sender.try_send(());
    }
}

fn parse_snapshot(json: &str) -> Result<PricingCatalogSnapshot, AppError> {
    let snapshot: PricingCatalogSnapshot = serde_json::from_str(json)
        .map_err(|error| AppError::Config(format!("解析 LiteLLM 价格快照失败: {error}")))?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn validate_snapshot(snapshot: &PricingCatalogSnapshot) -> Result<(), AppError> {
    if snapshot.schema_version != 1 {
        return Err(AppError::Config(format!(
            "不支持的 LiteLLM 价格快照版本: {}",
            snapshot.schema_version
        )));
    }
    if snapshot.entries.is_empty() {
        return Err(AppError::Config("LiteLLM 价格快照为空".to_string()));
    }
    if snapshot.source.revision.trim().is_empty() {
        return Err(AppError::Config(
            "LiteLLM 价格快照缺少 revision".to_string(),
        ));
    }

    for entry in &snapshot.entries {
        if entry.model_key.trim().is_empty()
            || entry.provider.trim().is_empty()
            || !SUPPORTED_MODES.contains(&entry.mode.as_str())
        {
            return Err(AppError::Config(format!(
                "LiteLLM 价格条目无效: {}",
                entry.model_key
            )));
        }
        validate_price(&entry.model_key, &entry.input_cost_per_million)?;
        validate_price(&entry.model_key, &entry.output_cost_per_million)?;
        if let Some(value) = entry.cache_read_cost_per_million.as_deref() {
            validate_price(&entry.model_key, value)?;
        }
        if let Some(value) = entry.cache_creation_cost_per_million.as_deref() {
            validate_price(&entry.model_key, value)?;
        }
    }
    Ok(())
}

fn validate_price(model_key: &str, value: &str) -> Result<(), AppError> {
    let price = Decimal::from_str(value)
        .map_err(|error| AppError::Config(format!("LiteLLM 模型 {model_key} 价格无效: {error}")))?;
    if price < Decimal::ZERO {
        return Err(AppError::Config(format!(
            "LiteLLM 模型 {model_key} 价格不能为负数"
        )));
    }
    Ok(())
}

fn pricing_catalog_meta_on_conn(
    conn: &rusqlite::Connection,
) -> Result<Option<PricingCatalogMeta>, AppError> {
    conn.query_row(
        "SELECT source_url, source_revision, source_generated_at,
                etag, updated_at, checked_at
         FROM litellm_pricing_meta
         WHERE singleton = 1",
        [],
        |row| {
            Ok(PricingCatalogMeta {
                source_url: row.get(0)?,
                source_revision: row.get(1)?,
                source_generated_at: row.get(2)?,
                etag: row.get(3)?,
                updated_at: row.get(4)?,
                checked_at: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(|error| AppError::Database(format!("读取价格目录元数据失败: {error}")))
}

fn collect_missing_pricing_models(
    conn: &rusqlite::Connection,
) -> Result<Vec<MissingPricingModel>, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT COALESCE(NULLIF(pricing_model, ''), NULLIF(model, ''),
                            NULLIF(request_model, '')) AS billing_model,
                    COUNT(*) AS request_count,
                    MAX(created_at) AS last_seen_at,
                    MAX(CASE WHEN cache_read_tokens > 0 THEN 1 ELSE 0 END) AS needs_cache_read,
                    MAX(CASE WHEN cache_creation_tokens > 0 THEN 1 ELSE 0 END) AS needs_cache_creation
             FROM usage_logs
             WHERE CAST(total_cost_usd AS REAL) <= 0
               AND (input_tokens > 0 OR output_tokens > 0
                    OR cache_read_tokens > 0 OR cache_creation_tokens > 0)
             GROUP BY billing_model
             HAVING billing_model IS NOT NULL
             ORDER BY last_seen_at DESC
             LIMIT 500",
        )
        .map_err(|error| AppError::Database(format!("准备缺价模型查询失败: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })
        .map_err(|error| AppError::Database(format!("查询缺价模型失败: {error}")))?;

    let mut missing = Vec::new();
    for row in rows {
        let (model_id, request_count, last_seen_at, needs_cache_read, needs_cache_creation) =
            row.map_err(|error| AppError::Database(format!("读取缺价模型失败: {error}")))?;
        if find_model_pricing_row_for_requirements(
            conn,
            &model_id,
            needs_cache_read,
            needs_cache_creation,
        )?
        .is_none()
        {
            missing.push(MissingPricingModel {
                model_id,
                request_count: request_count as u64,
                last_seen_at,
            });
        }
    }
    Ok(missing)
}

fn snapshot_from_upstream(
    bytes: &[u8],
    revision: &str,
    now: i64,
) -> Result<PricingCatalogSnapshot, AppError> {
    let upstream: Value = serde_json::from_slice(bytes)
        .map_err(|error| AppError::Config(format!("解析 LiteLLM 上游目录失败: {error}")))?;
    let object = upstream
        .as_object()
        .ok_or_else(|| AppError::Config("LiteLLM 上游目录不是 JSON 对象".to_string()))?;
    let mut entries = Vec::new();

    for (model_key, model) in object {
        if model_key == "sample_spec" {
            continue;
        }
        let Some(model) = model.as_object() else {
            continue;
        };
        let Some(mode) = model.get("mode").and_then(Value::as_str) else {
            continue;
        };
        let Some(provider) = model.get("litellm_provider").and_then(Value::as_str) else {
            continue;
        };
        if !SUPPORTED_MODES.contains(&mode) || provider.trim().is_empty() {
            continue;
        }
        let Some(input) = json_price_per_million(model.get("input_cost_per_token")) else {
            continue;
        };
        let Some(output) = json_price_per_million(model.get("output_cost_per_token")) else {
            continue;
        };

        let mut special_pricing_fields = model
            .keys()
            .filter(|field| {
                BASE_PRICE_FIELDS.iter().any(|base| field.starts_with(base))
                    && SPECIAL_PRICE_MARKERS
                        .iter()
                        .any(|marker| field.contains(marker))
            })
            .cloned()
            .collect::<Vec<_>>();
        special_pricing_fields.sort();

        entries.push(PricingCatalogEntry {
            model_key: model_key.clone(),
            provider: provider.to_string(),
            mode: mode.to_string(),
            input_cost_per_million: input,
            output_cost_per_million: output,
            cache_read_cost_per_million: json_price_per_million(
                model.get("cache_read_input_token_cost"),
            ),
            cache_creation_cost_per_million: json_price_per_million(
                model.get("cache_creation_input_token_cost"),
            ),
            special_pricing_fields,
            source_url: model
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    entries.sort_by_cached_key(|entry| entry.model_key.to_ascii_lowercase());
    if entries.len() < 1000 {
        return Err(AppError::Config(format!(
            "LiteLLM 可用价格条目异常偏少: {}",
            entries.len()
        )));
    }

    Ok(PricingCatalogSnapshot {
        schema_version: 1,
        source: PricingCatalogSource {
            name: "LiteLLM".to_string(),
            url: LITELLM_PRICING_SOURCE_URL.to_string(),
            revision: revision.to_string(),
            generated_at: chrono::DateTime::from_timestamp(now, 0)
                .unwrap_or_else(Utc::now)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            license: "MIT".to_string(),
            license_url: LITELLM_LICENSE_URL.to_string(),
        },
        entries,
    })
}

fn json_price_per_million(value: Option<&Value>) -> Option<String> {
    let number = value?.as_number()?.to_string();
    let decimal = Decimal::from_scientific(&number)
        .or_else(|_| Decimal::from_str(&number))
        .ok()?;
    if decimal < Decimal::ZERO {
        return None;
    }
    Some(
        (decimal * Decimal::from(1_000_000u64))
            .normalize()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(revision: &str, generated_at: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "source": {
                "name": "LiteLLM",
                "url": LITELLM_PRICING_SOURCE_URL,
                "revision": revision,
                "generated_at": generated_at,
                "license": "MIT",
                "license_url": LITELLM_LICENSE_URL
            },
            "entries": [{
                "model_key": "test/model-v1",
                "provider": "test",
                "mode": "chat",
                "input_cost_per_million": "1.25",
                "output_cost_per_million": "5",
                "cache_read_cost_per_million": null,
                "cache_creation_cost_per_million": null,
                "special_pricing_fields": []
            }]
        })
        .to_string()
    }

    #[test]
    fn bundled_snapshot_installs_and_does_not_roll_back() {
        let db = Database::memory().unwrap();
        let first = db
            .install_bundled_pricing_catalog(&snapshot("new", "2026-07-26T00:00:00Z"))
            .unwrap();
        assert!(first.installed);
        assert_eq!(first.entry_count, 1);

        let older = db
            .install_bundled_pricing_catalog(&snapshot("old", "2026-07-25T00:00:00Z"))
            .unwrap();
        assert!(!older.installed);
        assert_eq!(older.source_revision, "new");
    }

    #[test]
    fn upstream_filter_keeps_chat_token_prices() {
        let upstream = serde_json::json!({
            "sample_spec": {},
            "chat-model": {
                "mode": "chat",
                "litellm_provider": "test",
                "input_cost_per_token": 0.00000125,
                "output_cost_per_token": 0.000005
            },
            "image-model": {
                "mode": "image_generation",
                "litellm_provider": "test",
                "input_cost_per_token": 1,
                "output_cost_per_token": 1
            }
        });
        let bytes = serde_json::to_vec(&upstream).unwrap();
        let result = snapshot_from_upstream(&bytes, "revision", 1);
        assert!(
            result.is_err(),
            "production guard rejects tiny upstream data"
        );

        assert_eq!(
            json_price_per_million(Some(&serde_json::json!(0.00000125))),
            Some("1.25".to_string())
        );
    }

    #[test]
    fn catalog_prices_require_present_cache_rates_and_manual_prices_win() {
        let db = Database::memory().unwrap();
        db.install_bundled_pricing_catalog(&snapshot("catalog", "2026-07-26T00:00:00Z"))
            .unwrap();
        let conn = db.conn.lock().expect("lock in-memory database");

        let catalog = find_model_pricing_row_for_requirements(&conn, "test/model-v1", false, false)
            .unwrap()
            .unwrap();
        assert_eq!(catalog.0, "1.25");
        assert!(
            find_model_pricing_row_for_requirements(&conn, "test/model-v1", true, false)
                .unwrap()
                .is_none(),
            "a missing catalog cache rate must not silently become zero"
        );

        conn.execute(
            "INSERT INTO model_pricing (
                model_id, display_name, input_cost_per_million,
                output_cost_per_million, cache_read_cost_per_million,
                cache_creation_cost_per_million
             ) VALUES ('model-v1', 'Manual', '9', '10', '1', '2')",
            [],
        )
        .unwrap();
        let manual = find_model_pricing_row_for_requirements(&conn, "test/model-v1", true, true)
            .unwrap()
            .unwrap();
        assert_eq!(manual, ("9".into(), "10".into(), "1".into(), "2".into()));
    }

    #[test]
    fn provider_qualified_exact_match_wins_and_ambiguous_alias_stays_missing() {
        let db = Database::memory().unwrap();
        let json = serde_json::json!({
            "schema_version": 1,
            "source": {
                "name": "LiteLLM",
                "url": LITELLM_PRICING_SOURCE_URL,
                "revision": "regions",
                "generated_at": "2026-07-26T00:00:00Z",
                "license": "MIT",
                "license_url": LITELLM_LICENSE_URL
            },
            "entries": [
                {
                    "model_key": "us.anthropic.claude-test-v1:0",
                    "provider": "bedrock",
                    "mode": "chat",
                    "input_cost_per_million": "1",
                    "output_cost_per_million": "2",
                    "cache_read_cost_per_million": "0.1",
                    "cache_creation_cost_per_million": "1.25",
                    "special_pricing_fields": []
                },
                {
                    "model_key": "eu.anthropic.claude-test-v1:0",
                    "provider": "bedrock",
                    "mode": "chat",
                    "input_cost_per_million": "1.1",
                    "output_cost_per_million": "2.2",
                    "cache_read_cost_per_million": "0.11",
                    "cache_creation_cost_per_million": "1.375",
                    "special_pricing_fields": []
                }
            ]
        })
        .to_string();
        db.install_bundled_pricing_catalog(&json).unwrap();
        let conn = db.conn.lock().expect("lock in-memory database");

        let exact = find_model_pricing_row_for_requirements(
            &conn,
            "us.anthropic.claude-test-v1:0",
            false,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(exact.0, "1");

        conn.execute(
            "INSERT INTO model_pricing (
                model_id, display_name, input_cost_per_million,
                output_cost_per_million, cache_read_cost_per_million,
                cache_creation_cost_per_million
             ) VALUES (
                'us.anthropic.claude-test-v1:0', 'US manual',
                '9', '10', '0.9', '11.25'
             )",
            [],
        )
        .unwrap();
        let manual_exact = find_model_pricing_row_for_requirements(
            &conn,
            "US.ANTHROPIC.CLAUDE-TEST-V1:0",
            true,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            manual_exact,
            ("9".into(), "10".into(), "0.9".into(), "11.25".into())
        );

        assert!(
            find_model_pricing_row_for_requirements(&conn, "claude-test", false, false)
                .unwrap()
                .is_none(),
            "different regional rates must not be guessed through an alias"
        );
    }
}
