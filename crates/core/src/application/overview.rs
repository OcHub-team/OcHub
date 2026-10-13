//! Passive, secret-free dashboard projections for one application runtime.
//!
//! Configured paths are not observed request paths. This endpoint does not
//! probe upstreams, import sessions, inspect live files, repair selections, or
//! backfill usage costs. Its sections are sampled independently, not an atomic
//! transaction spanning the gateway worker and SQLite.

use std::collections::HashMap;

use chrono::{Local, TimeZone, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{AppModeDto, Application, ApplicationError, ApplicationResult, UsageFilter};
use crate::db::lock_conn;
use crate::gateway::{ChannelHealth, GatewayStatus};
#[cfg(test)]
use crate::gateway::{Dialect, GatewayChannel, GatewayRoute};
use crate::services::sql_helpers::fresh_input_sql;
use crate::services::usage_stats::{
    DailyStats, ProviderStats, UsageSummary, effective_usage_log_filter,
};
use crate::{AppError, AppId, AppType};

mod routing;
use routing::project_routing;
pub use routing::*;

const DAY_SECONDS: i64 = 24 * 60 * 60;
const MAX_WINDOW_SECONDS: i64 = 366 * DAY_SECONDS;
const RECENT_REQUEST_LIMIT: i64 = 10;
const CONNECTION_PREVIEW_LIMIT: usize = 12;
const PERFORMANCE_PREVIEW_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewSnapshot {
    /// When collection started, in UTC seconds. This is not a health-probe time.
    pub generated_at: i64,
    pub window: OverviewWindow,
    pub gateway: OverviewSection<GatewayStatus>,
    pub routing: OverviewSection<OverviewRouting>,
    pub apps: OverviewSection<Vec<OverviewManagedApp>>,
    pub usage: OverviewSection<OverviewUsage>,
    /// Existing trend semantics: total_tokens is fresh input + output. Use
    /// the four counters to include caches, or graph request_count directly.
    pub trend: OverviewSection<Vec<DailyStats>>,
    /// At most 20 supplier/source groups, ordered by estimated cost. This
    /// preview must not be summed as the environment's complete source split.
    pub performance: OverviewSection<Vec<OverviewProviderPerformance>>,
    /// At most 10 surviving detail records. Daily rollups are not fabricated
    /// into individual requests; route attribution is not recorded here.
    pub recent_requests: OverviewSection<Vec<OverviewRequest>>,
}

/// Exactly one of data/error is populated. Failure is not an empty result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewSection<T> {
    pub data: Option<T>,
    pub error: Option<OverviewSectionError>,
}

impl<T> OverviewSection<T> {
    fn from_result(result: ApplicationResult<T>, section: &str) -> Self {
        match result {
            Ok(data) => Self {
                data: Some(data),
                error: None,
            },
            Err(error) => Self {
                data: None,
                // SQL errors, config parse errors and upstream errors can
                // contain private content. The overview exports no raw error.
                error: Some(OverviewSectionError {
                    code: error.code().to_string(),
                    message: format!("Unable to read {section}. Refresh or open its detail page."),
                }),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewSectionError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewWindow {
    /// Same inclusive UTC-second boundaries as the existing usage API.
    pub start: i64,
    pub end: i64,
    pub trend_bucket: OverviewTrendBucket,
    /// Daily buckets follow the serving node's local calendar. Hourly bucket
    /// labels additionally carry their own RFC 3339 offsets (including DST).
    pub node_utc_offset_seconds: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OverviewTrendBucket {
    Hour,
    Day,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewManagedApp {
    pub app: String,
    pub display_name: String,
    pub enabled: bool,
    pub mode: AppModeDto,
    pub supports_provider: bool,
    /// The adapter can resolve its config location. This does not assert the
    /// application is installed, a live file exists, or it loaded this config.
    pub config_available: bool,
    pub provider_count: usize,
    pub draft_count: usize,
    /// Device-local selection wins when that provider exists; otherwise the
    /// persisted DB selection is shown without repairing local settings.
    pub current_provider_id: Option<String>,
    pub stale_local_selection: bool,
    /// Switch apps: the selected provider. Additive apps: records explicitly
    /// marked live-managed. Neither establishes what the client has loaded.
    pub connections: Vec<OverviewConnection>,
    pub connection_count: usize,
    pub connections_truncated: bool,
    pub unverified_provider_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewConnection {
    pub id: String,
    pub name: String,
    pub current: bool,
    pub live_config_managed: Option<bool>,
    pub gateway_route_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewUsage {
    /// Deduplicated recorded usage, including imported client sessions and
    /// available daily rollups. This is not an ingress-request counter or SLA.
    pub summary: UsageSummary,
    pub currency: String,
    pub cost_is_estimate: bool,
    pub includes_imported_sessions: bool,
    /// Empty windows have no measured success rate, rather than a 0% failure.
    pub recorded_success_rate: Option<f32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverviewUsageSource {
    Captured,
    Session,
    Unknown,
}

/// This existing aggregate groups by supplier display name and source. It is
/// intentionally not given a provider ID: names may represent several channel
/// IDs and must not be joined to configured routes as historical attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewProviderPerformance {
    pub provider_name: String,
    pub source: OverviewUsageSource,
    pub request_count: u64,
    pub estimated_cost_usd: String,
    pub avg_latency_ms: Option<u64>,
    /// Detail records only; old daily rollups do not retain this measurement.
    pub avg_first_token_ms: Option<u64>,
    pub avg_tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRequest {
    pub request_id: String,
    pub app: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub created_at: i64,
    pub source: OverviewUsageSource,
    pub status_code: u16,
    pub total_tokens: u64,
    pub estimated_cost_usd: String,
    /// Session imports have no reliable request timing.
    pub latency_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
}

impl Application {
    /// Collect one environment's passive overview, defaulting to the last 24h.
    /// Only invalid query bounds fail the whole request; data-source failures
    /// stay inside their own section. All times are UTC seconds, not millis.
    pub async fn overview_snapshot(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> ApplicationResult<OverviewSnapshot> {
        let generated_at = Utc::now().timestamp();
        let window = overview_window(start, end, generated_at)?;
        let filter = UsageFilter {
            start: Some(window.start),
            end: Some(window.end),
            ..Default::default()
        };
        let gateway =
            OverviewSection::from_result(Ok(self.state.gateway.status().await), "gateway");
        let health = self.state.gateway.health_snapshot().await;
        let routing = OverviewSection::from_result(self.overview_routing(&health), "routing");
        let apps = OverviewSection::from_result(self.overview_apps(), "managed connections");
        let usage = OverviewSection::from_result(
            self.usage_summary(&filter).map(|summary| OverviewUsage {
                recorded_success_rate: (summary.total_requests > 0).then_some(summary.success_rate),
                summary,
                currency: "USD".to_string(),
                cost_is_estimate: true,
                includes_imported_sessions: true,
            }),
            "recorded usage",
        );
        let trend = OverviewSection::from_result(self.usage_trend(&filter, "day"), "usage trend");
        let performance = OverviewSection::from_result(
            self.usage_provider_stats(&filter).map(|rows| {
                rows.into_iter()
                    .take(PERFORMANCE_PREVIEW_LIMIT)
                    .map(overview_performance)
                    .collect()
            }),
            "recorded performance",
        );
        let recent_requests = OverviewSection::from_result(
            self.overview_recent_requests(window.start, window.end),
            "recent requests",
        );
        Ok(OverviewSnapshot {
            generated_at,
            window,
            gateway,
            routing,
            apps,
            usage,
            trend,
            performance,
            recent_requests,
        })
    }

    fn overview_routing(
        &self,
        health: &HashMap<String, ChannelHealth>,
    ) -> ApplicationResult<OverviewRouting> {
        let channels = self.state.db.get_gateway_channels()?;
        let routes = self.state.db.get_gateway_routes()?;
        let mut routing = project_routing(routes, channels, health);
        // Explicit projection: never load the API key secret for this view.
        let conn = lock_conn!(self.state.db.conn);
        let mut stmt = conn.prepare("SELECT id, name, route_id, model_policy, enabled FROM gateway_keys WHERE route_id IS NOT NULL ORDER BY name, id").map_err(AppError::from)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })
            .map_err(AppError::from)?;
        for row in rows {
            let (id, name, route_id, policy, enabled) = row.map_err(AppError::from)?;
            let model_policy = policy
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|_| {
                    ApplicationError::InvalidInput("Invalid stored model policy".into())
                })?;
            routing.bindings.push(OverviewRouteBinding {
                id,
                name,
                route_id,
                model_policy,
                enabled,
            });
        }
        Ok(routing)
    }

    fn overview_apps(&self) -> ApplicationResult<Vec<OverviewManagedApp>> {
        let apps = self.list_apps()?;
        // Project all apps in two indexed-table reads, without loading native
        // settings_config, endpoint tables, or the bodies of editing drafts.
        let conn = lock_conn!(self.state.db.conn);
        let mut stmt = conn
            .prepare(
                "SELECT app_type, id, name, is_current, meta FROM providers
                 ORDER BY app_type, COALESCE(sort_index, 999999), created_at, id",
            )
            .map_err(AppError::from)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(AppError::from)?;
        let mut providers: HashMap<String, Vec<OverviewConnection>> = HashMap::new();
        for row in rows {
            let (app, id, name, current, meta) = row.map_err(AppError::from)?;
            let meta = serde_json::from_str::<serde_json::Value>(&meta).unwrap_or_default();
            providers.entry(app).or_default().push(OverviewConnection {
                id,
                name,
                current,
                live_config_managed: meta
                    .get("liveConfigManaged")
                    .and_then(|value| value.as_bool()),
                gateway_route_id: meta
                    .get("gatewayRouteId")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            });
        }
        let mut stmt = conn
            .prepare("SELECT app_id, COUNT(*) FROM editor_drafts GROUP BY app_id")
            .map_err(AppError::from)?;
        let drafts = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })
            .map_err(AppError::from)?
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(AppError::from)?;
        Ok(apps
            .into_iter()
            .map(|app| {
                let mut stored = providers.remove(&app.id).unwrap_or_default();
                let provider_count = stored.len();
                let db_current = stored
                    .iter()
                    .find(|item| item.current)
                    .map(|item| item.id.clone());
                let local_current = AppId::parse(&app.id)
                    .ok()
                    .and_then(|id| AppType::from_app_id(&id))
                    .and_then(|app| crate::settings::get_current_provider(&app));
                let stale_local_selection = app.mode == AppModeDto::Switch
                    && local_current
                        .as_ref()
                        .is_some_and(|id| !stored.iter().any(|item| &item.id == id));
                let current_provider_id = if app.mode == AppModeDto::Switch {
                    local_current
                        .filter(|id| stored.iter().any(|item| &item.id == id))
                        .or(db_current)
                } else {
                    None
                };
                let unverified_provider_count = if app.mode == AppModeDto::Additive {
                    stored
                        .iter()
                        .filter(|item| item.live_config_managed.is_none())
                        .count()
                } else {
                    0
                };
                for item in &mut stored {
                    item.current = current_provider_id.as_deref() == Some(item.id.as_str());
                }
                stored.retain(|item| match app.mode {
                    AppModeDto::Switch => item.current,
                    AppModeDto::Additive => item.live_config_managed == Some(true),
                });
                let connection_count = stored.len();
                stored.truncate(CONNECTION_PREVIEW_LIMIT);
                OverviewManagedApp {
                    draft_count: drafts.get(&app.id).copied().unwrap_or(0),
                    app: app.id,
                    display_name: app.display_name,
                    enabled: app.enabled,
                    mode: app.mode,
                    supports_provider: app.supports_provider,
                    config_available: app.config_error.is_none(),
                    provider_count,
                    current_provider_id,
                    stale_local_selection,
                    connections: stored,
                    connection_count,
                    connections_truncated: connection_count > CONNECTION_PREVIEW_LIMIT,
                    unverified_provider_count,
                }
            })
            .collect())
    }

    fn overview_recent_requests(
        &self,
        start: i64,
        end: i64,
    ) -> ApplicationResult<Vec<OverviewRequest>> {
        let conn = lock_conn!(self.state.db.conn);
        let effective = effective_usage_log_filter("l");
        let fresh_input = fresh_input_sql("l");
        // Unlike get_request_logs, this intentionally neither counts all rows
        // again nor calls maybe_backfill_log_costs (which writes to SQLite).
        let sql = format!(
            "SELECT l.request_id, l.app_type, l.provider_id,
                    COALESCE(p.name, gc.name, l.provider_id), l.model, l.created_at,
                    l.data_source, l.status_code,
                    {fresh_input} + l.output_tokens + l.cache_read_tokens + l.cache_creation_tokens,
                    l.total_cost_usd, l.latency_ms, l.first_token_ms
             FROM usage_logs l
             LEFT JOIN providers p ON l.provider_id = p.id AND l.app_type = p.app_type
             LEFT JOIN gateway_channels gc ON l.provider_id = gc.id
             WHERE l.created_at >= ?1 AND l.created_at <= ?2 AND {effective}
             ORDER BY l.created_at DESC, l.request_id DESC LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql).map_err(AppError::from)?;
        let rows = stmt
            .query_map(params![start, end, RECENT_REQUEST_LIMIT], |row| {
                let source = overview_source(row.get::<_, Option<String>>(6)?.as_deref());
                let measured = source == OverviewUsageSource::Captured;
                Ok(OverviewRequest {
                    request_id: row.get(0)?,
                    app: row.get(1)?,
                    provider_id: row.get(2)?,
                    provider_name: row.get(3)?,
                    model: row.get(4)?,
                    created_at: row.get(5)?,
                    source,
                    status_code: row.get(7)?,
                    total_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                    estimated_cost_usd: row.get(9)?,
                    latency_ms: measured.then_some(row.get::<_, i64>(10)?.max(0) as u64),
                    first_token_ms: row
                        .get::<_, Option<i64>>(11)?
                        .filter(|_| measured)
                        .map(|v| v.max(0) as u64),
                })
            })
            .map_err(AppError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(AppError::from)?;
        Ok(rows)
    }
}

fn overview_window(
    start: Option<i64>,
    end: Option<i64>,
    now: i64,
) -> ApplicationResult<OverviewWindow> {
    let end = end.unwrap_or(now);
    let start = start.unwrap_or_else(|| end.saturating_sub(DAY_SECONDS));
    let duration = end
        .checked_sub(start)
        .filter(|duration| *duration > 0 && *duration <= MAX_WINDOW_SECONDS);
    if duration.is_none()
        || Local.timestamp_opt(start, 0).single().is_none()
        || Local.timestamp_opt(end, 0).single().is_none()
    {
        return Err(ApplicationError::InvalidInput(
            "overview requires valid UTC-second bounds spanning more than zero and at most 366 days".to_string(),
        ));
    }
    Ok(OverviewWindow {
        start,
        end,
        trend_bucket: if duration.unwrap_or_default() <= DAY_SECONDS {
            OverviewTrendBucket::Hour
        } else {
            OverviewTrendBucket::Day
        },
        node_utc_offset_seconds: Local::now().offset().local_minus_utc(),
    })
}

fn overview_source(source: Option<&str>) -> OverviewUsageSource {
    match source {
        None | Some("gateway" | "proxy") => OverviewUsageSource::Captured,
        Some("session_log" | "codex_session" | "gemini_session" | "opencode_session") => {
            OverviewUsageSource::Session
        }
        _ => OverviewUsageSource::Unknown,
    }
}

fn overview_performance(row: ProviderStats) -> OverviewProviderPerformance {
    let source = match row.source_kind.as_str() {
        "station" => OverviewUsageSource::Captured,
        "session" => OverviewUsageSource::Session,
        _ => OverviewUsageSource::Unknown,
    };
    let measured = source == OverviewUsageSource::Captured;
    OverviewProviderPerformance {
        provider_name: row.provider_name,
        source,
        request_count: row.request_count,
        estimated_cost_usd: row.total_cost,
        avg_latency_ms: row.avg_latency_ms.filter(|_| measured),
        avg_first_token_ms: row.avg_first_token_ms.filter(|_| measured),
        avg_tokens_per_second: row.avg_tps.filter(|_| measured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayReasoningConfig;
    use crate::test_support::TestHome;
    use crate::{AppState, Database};
    use std::sync::Arc;

    fn application() -> Application {
        Application::from_state(Arc::new(AppState::new(Arc::new(
            Database::memory().unwrap(),
        ))))
    }

    fn channel(id: &str, enabled: bool) -> GatewayChannel {
        GatewayChannel {
            id: id.to_string(),
            endpoint_id: None,
            name: format!("Upstream {id}"),
            dialect: Dialect::Responses,
            base_url: "https://account:password@upstream.example:8443/path-secret?token=query-secret#fragment-secret".to_string(),
            api_key: "synthetic-api-secret".to_string(),
            path_override: Some("/path-override-secret".to_string()),
            models: Vec::new(),
            model_override: None,
            priority: 0,
            weight: 1,
            enabled,
            extra_headers: vec![("Authorization".to_string(), "header-secret".to_string())],
            imported_from: None,
        }
    }

    fn route(id: &str, channel_ids: &[&str]) -> GatewayRoute {
        GatewayRoute {
            id: id.to_string(),
            name: "Configured path".to_string(),
            website_url: None,
            app_type: None,
            channel_ids: channel_ids.iter().map(|id| id.to_string()).collect(),
            default_model: None,
            model_rules: Vec::new(),
            reasoning: GatewayReasoningConfig::default(),
            websocket_enabled: false,
            quota_api: None,
            enabled: true,
            created_at: 0,
        }
    }

    fn insert_log(db: &Database, id: &str, app: &str, source: &str, at: i64) {
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO usage_logs (
                request_id, provider_id, app_type, model, pricing_model,
                input_tokens, output_tokens, cache_read_tokens, total_cost_usd,
                latency_ms, first_token_ms, status_code, created_at, data_source, error_message
             ) VALUES (?1, ?2, ?3, 'known-model', 'known-model',
                       100, 20, 30, '0', 250, 40, 200, ?4, ?5, 'private-upstream-error-secret')",
                params![id, format!("provider-{id}"), app, at, source],
            )
            .unwrap();
    }

    #[test]
    fn overview_window_is_bounded_and_keeps_explicit_utc_bounds() {
        let now = 1_800_000_000;
        let default = overview_window(None, None, now).unwrap();
        assert_eq!((default.start, default.end), (now - DAY_SECONDS, now));
        assert_eq!(default.trend_bucket, OverviewTrendBucket::Hour);
        let week = overview_window(Some(now - 7 * DAY_SECONDS), Some(now), now + 10).unwrap();
        assert_eq!(week.end, now);
        assert_eq!(week.trend_bucket, OverviewTrendBucket::Day);
        for (start, end) in [
            (now, now),
            (now + 1, now),
            (now - MAX_WINDOW_SECONDS - 1, now),
            (i64::MIN, i64::MAX),
            (i64::MAX - 10, i64::MAX),
        ] {
            assert!(overview_window(Some(start), Some(end), now).is_err());
        }
    }

    #[test]
    fn overview_routing_shows_configuration_without_secrets_or_health_timestamps() {
        let routing = project_routing(
            vec![route("all", &[]), route("missing", &["enabled", "deleted"])],
            vec![channel("enabled", true), channel("disabled", false)],
            &HashMap::from([
                ("enabled".to_string(), ChannelHealth::Healthy),
                (
                    "disabled".to_string(),
                    ChannelHealth::Unhealthy("error-secret".to_string()),
                ),
            ]),
        );
        assert!(routing.routes[0].uses_all_channels);
        assert_eq!(routing.routes[0].enabled_channel_count, 1);
        assert_eq!(routing.routes[1].missing_channel_ids, vec!["deleted"]);
        assert_eq!(
            routing.channels[0].origin.as_deref(),
            Some("https://upstream.example:8443")
        );
        assert_eq!(
            routing.channels[0].reachability,
            OverviewReachability::Reachable
        );
        assert_eq!(
            routing.channels[1].reachability,
            OverviewReachability::Unreachable
        );
        assert!(
            routing
                .channels
                .iter()
                .all(|channel| channel.reachability_observed_at.is_none())
        );
        let serialized = serde_json::to_string(&routing).unwrap();
        for private in [
            "account",
            "password",
            "secret",
            "apiKey",
            "extraHeaders",
            "pathOverride",
        ] {
            assert!(!serialized.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn overview_snapshot_reads_stored_data_without_writes_or_secret_payloads() {
        let home = TestHome::new();
        let app = application();
        let db = &app.state.db;
        let now = 1_800_000_000;
        // A normal usage-log read would backfill this zero cost. The overview
        // must leave it and every other database record exactly as stored.
        db.conn.lock().unwrap().execute(
            "INSERT INTO model_pricing (model_id, display_name, input_cost_per_million, output_cost_per_million)
             VALUES ('known-model', 'Known', '10', '20')", [],
        ).unwrap();
        db.conn.lock().unwrap().execute(
            "INSERT INTO providers (id, app_type, name, settings_config, is_current, meta)
             VALUES ('current', 'claude', 'Primary', '{\"api_key\":\"provider-secret\"}', 1,
                     '{\"gatewayRouteId\":\"station-main\",\"usage_script\":{\"script\":\"script-secret\"}}')", [],
        ).unwrap();
        crate::settings::set_current_provider(&AppType::Claude, Some("stale-local")).unwrap();
        let settings_path = home.path().join(".ochub/settings.json");
        let settings_before = std::fs::read(&settings_path).unwrap();
        db.conn.lock().unwrap().execute(
            "INSERT INTO gateway_keys (id, name, key, route_id, model_policy, enabled, created_at) VALUES ('binding', 'Client policy', 'credential-must-stay-private', 'station-main', '{}', 1, 0)", [],
        ).unwrap();
        insert_log(db, "captured", "gateway", "gateway", now - 10);
        insert_log(db, "imported", "codex", "codex_session", now - 20);
        let changes_before = db.conn.lock().unwrap().total_changes();
        let snapshot =
            futures::executor::block_on(app.overview_snapshot(Some(now - 3600), Some(now)))
                .unwrap();
        assert_eq!(db.conn.lock().unwrap().total_changes(), changes_before);
        let binding = &snapshot.routing.data.as_ref().unwrap().bindings[0];
        assert_eq!(binding.id, "binding");
        assert!(binding.model_policy.is_some());
        assert_eq!(std::fs::read(settings_path).unwrap(), settings_before);
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("stale-local")
        );
        let claude = snapshot
            .apps
            .data
            .as_ref()
            .unwrap()
            .iter()
            .find(|app| app.app == "claude")
            .unwrap();
        assert_eq!(claude.current_provider_id.as_deref(), Some("current"));
        assert!(claude.stale_local_selection);
        assert_eq!(
            claude.connections[0].gateway_route_id.as_deref(),
            Some("station-main")
        );
        let usage = snapshot.usage.data.as_ref().unwrap();
        assert_eq!(usage.summary.total_requests, 2);
        assert_eq!(usage.recorded_success_rate, Some(100.0));
        assert!(usage.cost_is_estimate && usage.includes_imported_sessions);
        let requests = snapshot.recent_requests.data.as_ref().unwrap();
        assert_eq!(requests[0].latency_ms, Some(250));
        assert_eq!(requests[0].total_tokens, 150);
        assert_eq!(requests[1].source, OverviewUsageSource::Session);
        assert_eq!(requests[1].latency_ms, None);
        assert_eq!(requests[1].first_token_ms, None);
        assert_eq!(requests[1].total_tokens, 120); // Codex input already includes cache reads.
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for private in [
            "provider-secret",
            "credential-must-stay-private",
            "script-secret",
            "private-upstream-error-secret",
            "settingsConfig",
            "api_key",
            "errorMessage",
        ] {
            assert!(!serialized.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn overview_sections_fail_independently_and_empty_usage_has_no_success_rate() {
        let _home = TestHome::new();
        let app = application();
        let now = 1_800_000_000;
        let empty = futures::executor::block_on(app.overview_snapshot(Some(now - 3600), Some(now)))
            .unwrap();
        assert_eq!(empty.usage.data.unwrap().recorded_success_rate, None);
        assert!(empty.recent_requests.data.unwrap().is_empty());
        // Break only the draft-count source. Runtime, routing and historical
        // usage remain useful, while connections report a distinct failure.
        app.state
            .db
            .conn
            .lock()
            .unwrap()
            .execute("DROP TABLE editor_drafts", [])
            .unwrap();
        let snapshot =
            futures::executor::block_on(app.overview_snapshot(Some(now - 3600), Some(now)))
                .unwrap();
        assert!(snapshot.apps.data.is_none());
        assert_eq!(snapshot.apps.error.as_ref().unwrap().code, "INTERNAL");
        assert!(snapshot.gateway.data.is_some());
        assert!(snapshot.routing.data.is_some());
        assert!(snapshot.usage.data.is_some());
        assert!(snapshot.trend.data.is_some());
        assert!(snapshot.performance.data.is_some());
        assert!(snapshot.recent_requests.data.is_some());
        assert!(
            !serde_json::to_string(&snapshot.apps)
                .unwrap()
                .contains("no such table")
        );
    }

    #[test]
    fn overview_recent_requests_are_bounded_and_session_timing_stays_unknown() {
        let _home = TestHome::new();
        let app = application();
        let now = 1_800_000_000;
        for index in 0..20 {
            insert_log(
                &app.state.db,
                &format!("row-{index:02}"),
                "codex",
                "codex_session",
                now - index,
            );
        }
        insert_log(
            &app.state.db,
            "outside",
            "gateway",
            "gateway",
            now - DAY_SECONDS - 1,
        );
        let rows = app
            .overview_recent_requests(now - DAY_SECONDS, now)
            .unwrap();
        assert_eq!(rows.len(), RECENT_REQUEST_LIMIT as usize);
        assert_eq!(rows.first().unwrap().request_id, "row-00");
        assert_eq!(rows.last().unwrap().request_id, "row-09");
        assert!(
            rows.iter()
                .all(|row| row.latency_ms.is_none() && row.first_token_ms.is_none())
        );
    }

    #[test]
    fn overview_additive_summaries_bound_preview_and_distinguish_unverified_records() {
        let _home = TestHome::new();
        let app = application();
        {
            let conn = app.state.db.conn.lock().unwrap();
            for index in 0..20 {
                conn.execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, meta)
                     VALUES (?1, 'opencode', ?1, '{}', ?2)",
                    params![
                        format!("managed-{index:02}"),
                        if index < 15 {
                            "{\"liveConfigManaged\":true}"
                        } else {
                            "{}"
                        }
                    ],
                )
                .unwrap();
            }
        }
        let apps = app.overview_apps().unwrap();
        let additive = apps.iter().find(|app| app.app == "opencode").unwrap();
        assert_eq!(additive.current_provider_id, None);
        assert_eq!(additive.provider_count, 20);
        assert_eq!(additive.connection_count, 15);
        assert_eq!(additive.connections.len(), CONNECTION_PREVIEW_LIMIT);
        assert!(additive.connections_truncated);
        assert_eq!(additive.unverified_provider_count, 5);
        assert!(additive.connections.iter().all(|item| !item.current));
    }

    #[test]
    fn route_preview_uses_first_match_and_passthrough_suppresses_fallback() {
        use crate::gateway::GatewayModelRule;
        let mut route = route("r", &[]);
        route.default_model = Some("fallback".into());
        route.model_rules = vec![
            GatewayModelRule {
                model: "client-*".into(),
                upstream_model: "".into(),
                channel_id: None,
                dialect: None,
            },
            GatewayModelRule {
                model: "client-one".into(),
                upstream_model: "should-not-win".into(),
                channel_id: None,
                dialect: None,
            },
        ];
        let mut upstream = channel("upstream", true);
        upstream.model_override = Some("channel-target".into());
        let routing = project_routing(vec![route], vec![upstream], &HashMap::new());
        let matched = routing
            .explain("r", None, "client-one", Dialect::Messages)
            .unwrap();
        assert_eq!(matched.matched_rule.as_deref(), Some("client-*"));
        assert_eq!(matched.routing_model, "client-one");
        assert!(!matched.uses_fallback);
        assert_eq!(matched.candidates[0].upstream_model, "channel-target");
        let unmatched = routing
            .explain("r", None, "other", Dialect::Messages)
            .unwrap();
        assert_eq!(unmatched.routing_model, "fallback");
        assert_eq!(unmatched.candidates[0].upstream_model, "fallback");
        assert!(unmatched.uses_fallback);
    }

    #[test]
    fn route_preview_empty_caller_policy_overrides_route_and_rejects_stale_binding() {
        use crate::gateway::types::GatewayAppModelPolicy;
        let mut route = route("r", &[]);
        route.default_model = Some("route-fallback".into());
        let mut routing = project_routing(vec![route], vec![channel("a", true)], &HashMap::new());
        routing.bindings.push(OverviewRouteBinding {
            id: "caller".into(),
            name: "Client".into(),
            route_id: "r".into(),
            model_policy: Some(GatewayAppModelPolicy::default()),
            enabled: true,
        });
        let preview = routing
            .explain("r", Some("caller"), "client", Dialect::Messages)
            .unwrap();
        assert!(preview.uses_binding_policy);
        assert_eq!(preview.routing_model, "client");
        assert!(!preview.uses_fallback);
        assert!(
            routing
                .explain("r", Some("missing"), "client", Dialect::Messages)
                .is_none()
        );
        routing.bindings[0].route_id = "other-route".into();
        assert!(
            routing
                .explain("r", Some("caller"), "client", Dialect::Messages)
                .is_none()
        );
    }

    #[test]
    fn route_preview_explains_model_dialect_and_allowlist_exclusions() {
        use crate::gateway::GatewayModelRule;
        let mut route = route("r", &["ok", "model", "dialect", "disabled"]);
        route.model_rules.push(GatewayModelRule {
            model: "alias".into(),
            upstream_model: "actual".into(),
            channel_id: None,
            dialect: Some(Dialect::Responses),
        });
        let mut model = channel("model", true);
        model.models = vec!["alias".into()]; // Filters the mapped model, not the alias.
        let mut dialect = channel("dialect", true);
        dialect.dialect = Dialect::Messages;
        let routing = project_routing(
            vec![route],
            vec![
                channel("ok", true),
                model,
                dialect,
                channel("disabled", false),
                channel("outside", true),
            ],
            &HashMap::new(),
        );
        let preview = routing
            .explain("r", None, "alias", Dialect::Messages)
            .unwrap();
        let states: HashMap<_, _> = preview
            .candidates
            .iter()
            .map(|candidate| (candidate.channel_id.as_str(), candidate.state))
            .collect();
        assert_eq!(states["ok"], OverviewCandidateState::Eligible);
        assert_eq!(states["model"], OverviewCandidateState::ModelMismatch);
        assert_eq!(
            states["dialect"],
            OverviewCandidateState::RuleDialectMismatch
        );
        assert_eq!(states["disabled"], OverviewCandidateState::ChannelDisabled);
        assert_eq!(states["outside"], OverviewCandidateState::OutsideRoute);
        assert!(
            preview
                .candidates
                .iter()
                .all(|candidate| candidate.upstream_model == "actual")
        );
    }

    #[test]
    fn route_preview_orders_priority_before_native_dialect_and_keeps_health_separate() {
        let mut first = channel("first", true);
        first.priority = -1;
        let mut native = channel("native", true);
        native.dialect = Dialect::Messages;
        let other = channel("other", true);
        let routing = project_routing(
            vec![route("r", &[])],
            vec![other, native, first],
            &HashMap::from([("first".into(), ChannelHealth::Unhealthy("private".into()))]),
        );
        let preview = routing.explain("r", None, "m", Dialect::Messages).unwrap();
        assert_eq!(
            preview
                .candidates
                .iter()
                .map(|candidate| candidate.channel_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "native", "other"]
        );
        assert!(
            preview
                .candidates
                .iter()
                .all(|candidate| candidate.state == OverviewCandidateState::Eligible)
        );
        assert_eq!(
            routing.channels[2].reachability,
            OverviewReachability::Unreachable
        );
    }

    #[test]
    fn legacy_route_snapshot_cannot_silently_simulate_missing_policies() {
        let routing: OverviewRouting =
            serde_json::from_value(serde_json::json!({"routes": [], "channels": []})).unwrap();
        assert!(!routing.policy_details_available);
        assert!(
            routing
                .explain("r", None, "model", Dialect::Responses)
                .is_none()
        );
    }
}
