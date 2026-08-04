//! API-key quota readout for relay stations.
//!
//! New API and Sub2API both expose a read-only endpoint authenticated by the
//! same bearer key used for inference. Their paths and response shapes differ,
//! and nothing about an inference endpoint reveals which console (if any) sits
//! behind it — so the station states which one it speaks and this module talks
//! to exactly that one, normalizing into the existing [`UsageResult`] model.
//! Probing both was worse than useless: on a provider with neither, it spent
//! two requests to produce an error the user could do nothing about.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use url::Url;

use crate::gateway::StationQuotaApi;
use crate::model::{UsageData, UsageResult};

const DEFAULT_NEW_API_QUOTA_PER_UNIT: f64 = 500_000.0;

pub async fn query_key_quota(
    base_url: &str,
    api_key: &str,
    api: StationQuotaApi,
) -> Result<UsageResult, String> {
    if api_key.trim().is_empty() {
        return Ok(failure("API key is empty"));
    }
    let root = service_root(base_url)?;
    let client = crate::http_client::get();

    match api {
        StationQuotaApi::Sub2Api => {
            let response = request_json(
                &client,
                root.join("v1/usage").map_err(|error| error.to_string())?,
                api_key,
            )
            .await;
            if let Ok((status, body)) = &response
                && status.is_success()
                && let Some(result) = parse_sub2api(body)
            {
                return Ok(result);
            }
            Ok(failure(&format!(
                "Sub2API quota endpoint did not answer ({})",
                request_error_summary(&response)
            )))
        }
        StationQuotaApi::NewApi => {
            let response = request_json(
                &client,
                root.join("api/usage/token/")
                    .map_err(|error| error.to_string())?,
                api_key,
            )
            .await;
            if let Ok((status, body)) = &response
                && status.is_success()
                && let Some(raw) = parse_new_api_raw(body)
            {
                let quota_per_unit = query_new_api_quota_per_unit(&client, &root)
                    .await
                    .unwrap_or(DEFAULT_NEW_API_QUOTA_PER_UNIT);
                return Ok(format_new_api(raw, quota_per_unit));
            }
            Ok(failure(&format!(
                "New API quota endpoint did not answer ({})",
                request_error_summary(&response)
            )))
        }
    }
}

fn service_root(base_url: &str) -> Result<Url, String> {
    let mut url =
        Url::parse(base_url.trim()).map_err(|error| format!("Invalid provider URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Provider URL must use HTTP or HTTPS".to_string());
    }
    url.set_query(None);
    url.set_fragment(None);

    let mut segments = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if segments
        .last()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "v1" | "v1beta"))
    {
        segments.pop();
        if segments.last().is_some_and(|value| value == "api") {
            segments.pop();
        }
    }
    let path = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", segments.join("/"))
    };
    url.set_path(&path);
    Ok(url)
}

async fn request_json(
    client: &Client,
    url: Url,
    api_key: &str,
) -> Result<(StatusCode, Value), String> {
    let response = client
        .get(url)
        .bearer_auth(api_key.trim())
        .header("Accept", "application/json")
        .header("User-Agent", "OcHub/key-quota")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| format!("network error: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read response: {error}"))?;
    let body = serde_json::from_slice(&bytes)
        .map_err(|error| format!("HTTP {status}, invalid JSON: {error}"))?;
    Ok((status, body))
}

fn request_error_summary(result: &Result<(StatusCode, Value), String>) -> String {
    match result {
        Err(error) => error.clone(),
        Ok((status, body)) if !status.is_success() => {
            let message = body
                .pointer("/error/message")
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            format!("HTTP {status}: {message}")
        }
        Ok((_, _)) => "unexpected response".to_string(),
    }
}

#[derive(Debug, PartialEq)]
struct NewApiQuota {
    name: Option<String>,
    total: f64,
    used: f64,
    remaining: f64,
    unlimited: bool,
    expires_at: Option<i64>,
}

fn parse_new_api_raw(body: &Value) -> Option<NewApiQuota> {
    let data = body.get("data")?;
    if body.get("code").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    Some(NewApiQuota {
        name: data.get("name").and_then(Value::as_str).map(str::to_string),
        total: number(data, "total_granted")?,
        used: number(data, "total_used")?,
        remaining: number(data, "total_available")?,
        unlimited: data
            .get("unlimited_quota")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        expires_at: data
            .get("expires_at")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0),
    })
}

fn format_new_api(raw: NewApiQuota, quota_per_unit: f64) -> UsageResult {
    let divisor = if quota_per_unit.is_finite() && quota_per_unit > 0.0 {
        quota_per_unit
    } else {
        DEFAULT_NEW_API_QUOTA_PER_UNIT
    };
    UsageResult {
        success: true,
        data: Some(vec![UsageData {
            plan_name: raw.name.or_else(|| Some("New API".to_string())),
            extra: Some(
                json!({
                    "provider": "new-api",
                    "unlimited": raw.unlimited,
                    "expiresAt": raw.expires_at,
                    "quotaPerUnit": divisor,
                })
                .to_string(),
            ),
            is_valid: Some(true),
            invalid_message: None,
            total: (!raw.unlimited).then_some(raw.total / divisor),
            used: Some(raw.used / divisor),
            remaining: (!raw.unlimited).then_some(raw.remaining / divisor),
            unit: Some("USD".to_string()),
        }]),
        error: None,
    }
}

async fn query_new_api_quota_per_unit(client: &Client, root: &Url) -> Option<f64> {
    let response = client
        .get(root.join("api/status").ok()?)
        .header("Accept", "application/json")
        .header("User-Agent", "OcHub/key-quota")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    number(body.get("data").unwrap_or(&body), "quota_per_unit")
}

fn parse_sub2api(body: &Value) -> Option<UsageResult> {
    let mode = body.get("mode").and_then(Value::as_str)?;
    let valid = body.get("isValid").and_then(Value::as_bool).unwrap_or(true);
    let invalid_message = body
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| *status != "active")
        .map(str::to_string);
    let mut data = Vec::new();

    if mode == "quota_limited" {
        if let Some(quota) = body.get("quota") {
            data.push(usage_item(
                Some("Key quota"),
                number(quota, "remaining"),
                number(quota, "limit"),
                number(quota, "used"),
                quota.get("unit").and_then(Value::as_str).or(Some("USD")),
                valid,
                invalid_message.clone(),
                Some(json!({ "provider": "sub2api", "kind": "key" })),
            ));
        }
        if let Some(rate_limits) = body.get("rate_limits").and_then(Value::as_array) {
            for limit in rate_limits {
                let window = limit
                    .get("window")
                    .and_then(Value::as_str)
                    .unwrap_or("Rate limit");
                data.push(usage_item(
                    Some(window),
                    number(limit, "remaining"),
                    number(limit, "limit"),
                    number(limit, "used"),
                    Some("USD"),
                    valid,
                    invalid_message.clone(),
                    Some(json!({
                        "provider": "sub2api",
                        "kind": "rate_limit",
                        "resetAt": limit.get("reset_at"),
                    })),
                ));
            }
        }
    } else if mode == "unrestricted" {
        let plan_name = body
            .get("planName")
            .and_then(Value::as_str)
            .unwrap_or("Sub2API");
        if let Some(subscription) = body.get("subscription") {
            for (label, usage_key, limit_key) in [
                ("Daily", "daily_usage_usd", "daily_limit_usd"),
                ("Weekly", "weekly_usage_usd", "weekly_limit_usd"),
                ("Monthly", "monthly_usage_usd", "monthly_limit_usd"),
            ] {
                let total = number(subscription, limit_key);
                if total.is_some_and(|value| value > 0.0) {
                    let used = number(subscription, usage_key).unwrap_or(0.0);
                    data.push(usage_item(
                        Some(&format!("{plan_name} · {label}")),
                        total.map(|total| (total - used).max(0.0)),
                        total,
                        Some(used),
                        Some("USD"),
                        valid,
                        invalid_message.clone(),
                        Some(json!({ "provider": "sub2api", "kind": "subscription" })),
                    ));
                }
            }
        }
        if data.is_empty() {
            data.push(usage_item(
                Some(plan_name),
                number(body, "remaining").or_else(|| number(body, "balance")),
                None,
                None,
                body.get("unit").and_then(Value::as_str).or(Some("USD")),
                valid,
                invalid_message.clone(),
                Some(json!({ "provider": "sub2api", "kind": "balance" })),
            ));
        }
    } else {
        return None;
    }

    Some(UsageResult {
        success: true,
        data: (!data.is_empty()).then_some(data),
        error: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn usage_item(
    plan_name: Option<&str>,
    remaining: Option<f64>,
    total: Option<f64>,
    used: Option<f64>,
    unit: Option<&str>,
    is_valid: bool,
    invalid_message: Option<String>,
    extra: Option<Value>,
) -> UsageData {
    UsageData {
        plan_name: plan_name.map(str::to_string),
        extra: extra.map(|value| value.to_string()),
        is_valid: Some(is_valid),
        invalid_message,
        total,
        used,
        remaining,
        unit: unit.map(str::to_string),
    }
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn failure(message: &str) -> UsageResult {
    UsageResult {
        success: false,
        data: None,
        error: Some(message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde_json::json;

    use super::{
        StationQuotaApi, format_new_api, parse_new_api_raw, parse_sub2api, query_key_quota,
        service_root,
    };

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    #[test]
    fn strips_known_inference_suffix_without_losing_subpath() {
        assert_eq!(
            service_root("https://example.com/relay/api/v1/")
                .unwrap()
                .as_str(),
            "https://example.com/relay/"
        );
        assert_eq!(
            service_root("https://example.com/").unwrap().as_str(),
            "https://example.com/"
        );
    }

    #[test]
    fn parses_new_api_key_quota_with_deployment_conversion() {
        let raw = parse_new_api_raw(&json!({
            "code": true,
            "data": {
                "name": "oc-key",
                "total_granted": 3000,
                "total_used": 1000,
                "total_available": 2000,
                "unlimited_quota": false,
                "expires_at": 1234
            }
        }))
        .unwrap();
        let result = format_new_api(raw, 1000.0);
        let quota = &result.data.unwrap()[0];
        assert_eq!(quota.total, Some(3.0));
        assert_eq!(quota.used, Some(1.0));
        assert_eq!(quota.remaining, Some(2.0));
    }

    #[test]
    fn parses_sub2api_key_and_window_quotas() {
        let result = parse_sub2api(&json!({
            "mode": "quota_limited",
            "isValid": true,
            "status": "active",
            "quota": { "limit": 10, "used": 2.5, "remaining": 7.5, "unit": "USD" },
            "rate_limits": [
                { "window": "5h", "limit": 3, "used": 1, "remaining": 2 }
            ]
        }))
        .unwrap();
        let data = result.data.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].remaining, Some(7.5));
        assert_eq!(data[1].plan_name.as_deref(), Some("5h"));
        assert_eq!(data[1].remaining, Some(2.0));
    }

    #[test]
    fn parses_sub2api_subscription_windows() {
        let result = parse_sub2api(&json!({
            "mode": "unrestricted",
            "isValid": true,
            "planName": "Pro",
            "subscription": {
                "daily_usage_usd": 2,
                "daily_limit_usd": 10,
                "weekly_usage_usd": 0,
                "weekly_limit_usd": 0,
                "monthly_usage_usd": 25,
                "monthly_limit_usd": 100
            }
        }))
        .unwrap();
        let data = result.data.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].remaining, Some(8.0));
        assert_eq!(data[1].remaining, Some(75.0));
    }

    #[tokio::test]
    async fn probes_sub2api_with_the_inference_key() {
        let router = Router::new().route(
            "/v1/usage",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-sub2api");
                Json(json!({
                    "mode": "quota_limited",
                    "isValid": true,
                    "status": "active",
                    "quota": { "limit": 8, "used": 3, "remaining": 5, "unit": "USD" }
                }))
            }),
        );
        let base = serve(router).await;
        let result = query_key_quota(
            &format!("{base}/v1"),
            "sk-sub2api",
            StationQuotaApi::Sub2Api,
        )
        .await
        .unwrap();
        assert_eq!(result.data.unwrap()[0].remaining, Some(5.0));
    }

    #[tokio::test]
    async fn falls_back_to_new_api_and_reads_instance_conversion() {
        let router = Router::new()
            .route(
                "/v1/usage",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "not found" })),
                    )
                }),
            )
            .route(
                "/api/usage/token/",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-new-api");
                    Json(json!({
                        "code": true,
                        "message": "ok",
                        "data": {
                            "name": "desktop",
                            "total_granted": 6000,
                            "total_used": 1000,
                            "total_available": 5000,
                            "unlimited_quota": false,
                            "expires_at": 0
                        }
                    }))
                }),
            )
            .route(
                "/api/status",
                get(|| async {
                    Json(json!({ "success": true, "data": { "quota_per_unit": 1000 } }))
                }),
            );
        let base = serve(router).await;
        let result = query_key_quota(&base, "sk-new-api", StationQuotaApi::NewApi)
            .await
            .unwrap();
        let quota = &result.data.unwrap()[0];
        assert_eq!(quota.total, Some(6.0));
        assert_eq!(quota.used, Some(1.0));
        assert_eq!(quota.remaining, Some(5.0));
    }
}
