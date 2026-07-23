//! Control-API routes for the OcHub local gateway: config, upstreams, route
//! profiles, keys, lifecycle, health, import, and one-click app configuration.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use ochub_core::gateway::apply;
use ochub_core::gateway::types::{GatewayChannel, GatewayConfig, GatewayKey, GatewayRoute};
use ochub_core::{AppError, AppType};

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

// -- lifecycle --------------------------------------------------------------

async fn gateway_status(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.gateway.status().await)
}

async fn gateway_start(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.gateway.start().await?)
}

async fn gateway_stop(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    s.app.gateway.stop().await?;
    Ok(Json(json!({ "ok": true })))
}

// -- config -----------------------------------------------------------------

async fn config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_gateway_config()?)
}

async fn config_set(
    State(s): State<ServerState>,
    Json(config): Json<GatewayConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_gateway_config(&config)?;
    // Push live-applicable parts into a running gateway.
    s.app.gateway.reload_config().await?;
    Ok(Json(json!({ "ok": true })))
}

// -- channels ---------------------------------------------------------------

async fn channels_list(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let channels = s.app.db.get_gateway_channels()?;
    let health = s.app.gateway.health_snapshot().await;
    let items: Vec<Value> = channels
        .into_iter()
        .map(|c| {
            let h = health.get(&c.id).cloned();
            let mut v = serde_json::to_value(&c).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "health".into(),
                    serde_json::to_value(h).unwrap_or(Value::Null),
                );
            }
            v
        })
        .collect();
    Ok(Json(json!({ "channels": items })))
}

async fn channel_upsert(
    State(s): State<ServerState>,
    Json(mut channel): Json<GatewayChannel>,
) -> ApiResult<Json<Value>> {
    if channel.id.trim().is_empty() {
        channel.id = uuid::Uuid::new_v4().to_string();
    }
    if channel.name.trim().is_empty() {
        return Err(ApiError(AppError::InvalidInput(
            "channel name must not be empty".into(),
        )));
    }
    if channel.base_url.trim().is_empty() {
        return Err(ApiError(AppError::InvalidInput(
            "channel base_url must not be empty".into(),
        )));
    }
    s.app.db.upsert_gateway_channel(&channel)?;
    to_value(channel)
}

async fn channel_delete(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let deleted = s.app.db.delete_gateway_channel(&id)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn channels_probe(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    s.app.gateway.probe_now().await?;
    to_value(s.app.gateway.health_snapshot().await)
}

async fn channel_import(
    State(s): State<ServerState>,
    Path((app, provider_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type: AppType = app
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("unknown app type: {app}")))?;
    to_value(apply::import_provider_as_channel(
        &s.app,
        app_type,
        &provider_id,
    )?)
}

// -- routes -----------------------------------------------------------------

async fn routes_list(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_gateway_routes()?)
}

async fn route_upsert(
    State(s): State<ServerState>,
    Json(mut route): Json<GatewayRoute>,
) -> ApiResult<Json<Value>> {
    if route.id.trim().is_empty() {
        route.id = uuid::Uuid::new_v4().to_string();
    }
    if route.name.trim().is_empty() {
        return Err(ApiError(AppError::InvalidInput(
            "route name must not be empty".into(),
        )));
    }
    s.app.db.upsert_gateway_route(&route)?;
    to_value(route)
}

async fn route_delete(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let deleted = s.app.db.delete_gateway_route(&id)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn route_activate(
    State(s): State<ServerState>,
    Path((id, app)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type: AppType = app
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("unknown app type: {app}")))?;
    to_value(apply::activate_route_for_app(&s.app, app_type, &id)?)
}

// -- keys -------------------------------------------------------------------

async fn keys_list(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_gateway_keys()?)
}

#[derive(Deserialize)]
struct NewKeyBody {
    name: String,
}

async fn key_create(
    State(s): State<ServerState>,
    Json(body): Json<NewKeyBody>,
) -> ApiResult<Json<Value>> {
    if body.name.trim().is_empty() {
        return Err(ApiError(AppError::InvalidInput(
            "key name must not be empty".into(),
        )));
    }
    let key = GatewayKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        key: ochub_core::gateway::generate_key_secret(),
        route_id: None,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    s.app.db.upsert_gateway_key(&key)?;
    to_value(key)
}

async fn key_update(
    State(s): State<ServerState>,
    Json(key): Json<GatewayKey>,
) -> ApiResult<Json<Value>> {
    s.app.db.upsert_gateway_key(&key)?;
    Ok(Json(json!({ "ok": true })))
}

async fn key_delete(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let deleted = s.app.db.delete_gateway_key(&id)?;
    Ok(Json(json!({ "deleted": deleted })))
}

// -- one-click app configuration -------------------------------------------

async fn ensure_running_base_url(s: &ServerState) -> Result<String, AppError> {
    let mut status = s.app.gateway.status().await;
    if !status.running {
        let mut config = s.app.db.get_gateway_config()?;
        if !config.enabled {
            config.enabled = true;
            s.app.db.set_gateway_config(&config)?;
        }
        status = s.app.gateway.start().await?;
    }
    Ok(status.base_url)
}

async fn apply_app(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type: AppType = app
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("unknown app type: {app}")))?;
    let base_url = ensure_running_base_url(&s).await?;
    let result = tokio::task::spawn_blocking({
        let state = s.app.clone();
        move || apply::apply_to_app(&state, app_type, &base_url)
    })
    .await
    .map_err(|e| AppError::Message(format!("apply task failed: {e}")))??;
    to_value(result)
}

async fn apply_station_app(
    State(s): State<ServerState>,
    Path((station_id, app)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type: AppType = app
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("unknown app type: {app}")))?;
    let base_url = ensure_running_base_url(&s).await?;
    let route_id = if station_id.starts_with(apply::STATION_ROUTE_PREFIX) {
        station_id
    } else {
        apply::station_route_id(&station_id)
    };
    let result = tokio::task::spawn_blocking({
        let state = s.app.clone();
        move || apply::apply_station_to_app(&state, app_type, &base_url, &route_id)
    })
    .await
    .map_err(|e| AppError::Message(format!("apply station task failed: {e}")))??;
    to_value(result)
}

async fn generic_info(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let base_url = ensure_running_base_url(&s).await?;
    let result = apply::generic_client_info(&s.app, &base_url)?;
    to_value(result)
}

async fn supported_apps(State(_s): State<ServerState>) -> Json<Value> {
    Json(json!({
        "apps": apply::supported_apps()
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
    }))
}

pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/gateway/status", get(gateway_status))
        .route("/api/gateway/start", post(gateway_start))
        .route("/api/gateway/stop", post(gateway_stop))
        .route("/api/gateway/config", get(config_get).post(config_set))
        .route(
            "/api/gateway/channels",
            get(channels_list).post(channel_upsert),
        )
        .route(
            "/api/gateway/channels/{id}",
            axum::routing::delete(channel_delete),
        )
        .route(
            "/api/gateway/channels/import/{app}/{provider_id}",
            post(channel_import),
        )
        .route("/api/gateway/channels/probe", post(channels_probe))
        .route("/api/gateway/routes", get(routes_list).post(route_upsert))
        .route(
            "/api/gateway/routes/{id}",
            axum::routing::delete(route_delete),
        )
        .route(
            "/api/gateway/routes/{id}/activate/{app}",
            post(route_activate),
        )
        .route("/api/gateway/keys", get(keys_list).post(key_create))
        .route("/api/gateway/keys/update", post(key_update))
        .route("/api/gateway/keys/{id}", axum::routing::delete(key_delete))
        .route("/api/gateway/apply/{app}", post(apply_app))
        .route(
            "/api/gateway/stations/{station_id}/apply/{app}",
            post(apply_station_app),
        )
        .route("/api/gateway/generic-info", get(generic_info))
        .route("/api/gateway/supported-apps", get(supported_apps))
}
