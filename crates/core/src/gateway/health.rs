//! Periodic channel health probing.
//!
//! A lightweight reachability probe per enabled channel: any HTTP response
//! (even 401/404 — auth or path differences don't matter for liveness) counts
//! as healthy; connect/timeout errors and 5xx mark the channel unhealthy so the
//! router deprioritizes it until a later probe (or a successful live request)
//! recovers it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::db::Database;
use crate::gateway::types::{ChannelHealth, Dialect, GatewayChannel, GatewayEndpointTestResult};

const USER_TEST_TIMEOUT_SECS: u64 = 15;

fn models_request(client: &reqwest::Client, url: &str, api_key: &str) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .timeout(Duration::from_secs(USER_TEST_TIMEOUT_SECS));
    if !api_key.trim().is_empty() {
        request = request
            .bearer_auth(api_key)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    }
    request
}

fn truncate_body(body: String) -> String {
    let mut value: String = body.chars().take(300).collect();
    if body.chars().count() > 300 {
        value.push('…');
    }
    value
}

/// Fetch the OpenAI-compatible `/v1/models` list from one upstream endpoint.
/// Both common auth headers are sent so Messages-only relay stations work too.
pub async fn fetch_endpoint_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let candidates =
        crate::services::model_fetch::build_models_url_candidates(base_url, false, None)?;
    let mut last_error = None;

    for url in candidates {
        let response = models_request(client, &url, api_key)
            .send()
            .await
            .map_err(|error| format!("Request failed: {error}"))?;
        let status = response.status();
        if status.is_success() {
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|error| format!("Failed to parse model list: {error}"))?;
            let mut models: Vec<String> = body
                .get("data")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect();
            models.sort();
            models.dedup();
            return Ok(models);
        }

        let error = format!(
            "HTTP {}: {}",
            status.as_u16(),
            truncate_body(response.text().await.unwrap_or_default())
        );
        if matches!(status.as_u16(), 404 | 405) {
            last_error = Some(error);
            continue;
        }
        return Err(error);
    }

    Err(last_error.unwrap_or_else(|| "No models endpoint found".to_string()))
}

/// Measure an authenticated HTTP round trip to the endpoint's model-list URL.
pub async fn test_endpoint(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<GatewayEndpointTestResult, String> {
    let url = crate::services::model_fetch::build_models_url_candidates(base_url, false, None)?
        .into_iter()
        .next()
        .ok_or_else(|| "No models endpoint found".to_string())?;
    let started = Instant::now();
    let response = models_request(client, &url, api_key)
        .send()
        .await
        .map_err(|error| format!("Request failed: {error}"))?;
    let status = response.status().as_u16();
    Ok(GatewayEndpointTestResult {
        url,
        status,
        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        reachable: status < 500,
    })
}

/// Probe a single channel once.
pub async fn probe_channel(client: &reqwest::Client, channel: &GatewayChannel) -> ChannelHealth {
    let base = channel.base_url.trim_end_matches('/');
    let url = format!("{base}/v1/models");
    match client
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status >= 500 {
                ChannelHealth::Unhealthy(format!("HTTP {status}"))
            } else {
                ChannelHealth::Healthy
            }
        }
        Err(e) => ChannelHealth::Unhealthy(truncate(&e.to_string(), 200)),
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// What a single endpoint probe told us about the route.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointSignal {
    /// The endpoint parsed our (deliberately invalid) body — it exists.
    Strong,
    /// Auth/rate-limit/5xx response — the route may exist behind middleware.
    Weak,
    /// 404/405/connect failure — the endpoint is not served here.
    Absent,
}

async fn post_probe(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> EndpointSignal {
    let mut req = client
        .post(url)
        .timeout(Duration::from_secs(4))
        .header("content-type", "application/json")
        .body("{}");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    match req.send().await {
        Ok(resp) => match resp.status().as_u16() {
            404 | 405 | 501 => EndpointSignal::Absent,
            // An empty body fails validation only after routing succeeded.
            200 | 201 | 400 | 413 | 422 => EndpointSignal::Strong,
            _ => EndpointSignal::Weak,
        },
        Err(_) => EndpointSignal::Absent,
    }
}

fn dialects_from_signals(
    messages: EndpointSignal,
    responses: EndpointSignal,
    chat: EndpointSignal,
) -> Vec<Dialect> {
    let signals = [
        (Dialect::Messages, messages),
        (Dialect::Responses, responses),
        (Dialect::Chat, chat),
    ];
    let strong: Vec<Dialect> = signals
        .iter()
        .filter_map(|(dialect, signal)| (*signal == EndpointSignal::Strong).then_some(*dialect))
        .collect();
    if !strong.is_empty() {
        return strong;
    }
    signals
        .iter()
        .filter_map(|(dialect, signal)| (*signal == EndpointSignal::Weak).then_some(*dialect))
        .collect()
}

/// Detect every API dialect exposed by an upstream by probing the three
/// candidate endpoints with minimal invalid bodies. Strong validation signals
/// win; weak auth/middleware signals are returned only when none of the routes
/// could be confirmed strongly.
pub async fn detect_dialects(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Vec<Dialect> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() {
        return Vec::new();
    }
    let bearer = format!("Bearer {api_key}");
    let messages_url = format!("{base}/v1/messages");
    let responses_url = format!("{base}/v1/responses");
    let chat_url = format!("{base}/v1/chat/completions");
    let messages_headers = [("x-api-key", api_key), ("anthropic-version", "2023-06-01")];
    let bearer_headers = [("authorization", bearer.as_str())];
    let (messages, responses, chat) = tokio::join!(
        post_probe(client, &messages_url, &messages_headers),
        post_probe(client, &responses_url, &bearer_headers),
        post_probe(client, &chat_url, &bearer_headers),
    );
    dialects_from_signals(messages, responses, chat)
}

/// Backwards-compatible single-dialect detector for API callers that have not
/// adopted multi-interface stations yet. Preference remains Messages,
/// Responses, then Chat.
pub async fn detect_dialect(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Option<Dialect> {
    detect_dialects(client, base_url, api_key)
        .await
        .into_iter()
        .next()
}

/// Probe all enabled channels once, updating the shared health map.
pub async fn probe_all(
    db: &Database,
    client: &reqwest::Client,
    health: &RwLock<HashMap<String, ChannelHealth>>,
) {
    let channels = match db.get_gateway_channels() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[gateway] health probe: channel list failed: {e}");
            return;
        }
    };
    for channel in channels.iter().filter(|c| c.enabled) {
        let result = probe_channel(client, channel).await;
        if let ChannelHealth::Unhealthy(reason) = &result {
            log::info!("[gateway] channel '{}' unhealthy: {reason}", channel.name);
        }
        health.write().await.insert(channel.id.clone(), result);
    }
    // Drop stale entries for deleted channels.
    let ids: std::collections::HashSet<&str> = channels.iter().map(|c| c.id.as_str()).collect();
    health
        .write()
        .await
        .retain(|id, _| ids.contains(id.as_str()));
}

/// Spawn the periodic prober; returns its join handle (abort to stop).
pub fn spawn_prober(
    db: Arc<Database>,
    client: reqwest::Client,
    health: Arc<RwLock<HashMap<String, ChannelHealth>>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if interval_secs == 0 {
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(30)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            probe_all(&db, &client, &health).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_endpoint_signals_exclude_ambiguous_weak_ones() {
        assert_eq!(
            dialects_from_signals(
                EndpointSignal::Strong,
                EndpointSignal::Weak,
                EndpointSignal::Strong,
            ),
            vec![Dialect::Messages, Dialect::Chat]
        );
    }

    #[test]
    fn weak_signals_are_kept_when_nothing_is_confirmed() {
        assert_eq!(
            dialects_from_signals(
                EndpointSignal::Absent,
                EndpointSignal::Weak,
                EndpointSignal::Weak,
            ),
            vec![Dialect::Responses, Dialect::Chat]
        );
    }

    #[tokio::test]
    async fn model_fetch_and_latency_test_use_the_endpoint_models_route() {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                let authorized = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some("Bearer sk-test")
                    && headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        == Some("sk-test");
                if !authorized {
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({"error": "missing auth"})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "object": "list",
                        "data": [
                            {"id": "z-model"},
                            {"id": "a-model"},
                            {"id": "a-model"}
                        ]
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base_url = format!("http://{address}");
        let client = reqwest::Client::new();

        let models = fetch_endpoint_models(&client, &base_url, "sk-test")
            .await
            .unwrap();
        assert_eq!(models, vec!["a-model", "z-model"]);

        let result = test_endpoint(&client, &base_url, "sk-test").await.unwrap();
        assert_eq!(result.url, format!("{base_url}/v1/models"));
        assert_eq!(result.status, 200);
        assert!(result.reachable);
    }
}
