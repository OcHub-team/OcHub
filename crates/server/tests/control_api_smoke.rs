use std::net::SocketAddr;

use serde_json::json;

async fn spawn_temp_server() -> (String, tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let home = tempfile::tempdir().expect("temp home");
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));

    let state = routedeck_server::ServerState::init().expect("server state");
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let app = routedeck_server::build_router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve test router");
    });

    (format!("http://{addr}"), home, handle)
}

#[tokio::test]
async fn control_api_smoke_covers_core_feature_groups() {
    let (base, _home, handle) = spawn_temp_server().await;
    let client = reqwest::Client::new();

    let get_paths = [
        "/api/health",
        "/api/settings",
        "/api/providers/claude",
        "/api/providers/codex",
        "/api/providers/gemini",
        "/api/universal-providers",
        "/api/proxy/status",
        "/api/proxy/config",
        "/api/proxy/takeover",
        "/api/proxy/circuit-breaker/config",
        "/api/proxy/failover/claude/available",
        "/api/upstream-proxy/status",
        "/api/proxy/stream-check/config",
        "/api/mcp",
        "/api/mcp/config/claude",
        "/api/prompts/claude",
        "/api/skills",
        "/api/usage/summary",
        "/api/usage/by-app",
        "/api/usage/trends",
        "/api/usage/provider-limits?providerId=official&appType=claude",
        "/api/usage/data-sources",
        "/api/sessions",
        "/api/env/conflicts/claude",
        "/api/backups/db",
        "/api/config/status/claude",
        "/api/config/claude-code-path",
        "/api/config/dir/claude",
        "/api/config/app-path",
        "/api/portable",
        "/api/lightweight",
        "/api/codex/history/unify-backup",
        "/api/workspace/allowed-files",
        "/api/workspace/memory",
        "/api/claude-mcp/status",
        "/api/copilot/status",
        "/api/copilot/accounts",
        "/api/opencode/live-provider-ids",
        "/api/openclaw/health",
        "/api/openclaw/default-model",
        "/api/openclaw/env",
        "/api/hermes/live-provider-ids",
        "/api/hermes/model-config",
        "/api/hermes/memory-limits",
        "/api/claude-plugin/status",
        "/api/sync/webdav/status",
        "/api/sync/s3/status",
    ];

    for path in get_paths {
        let response = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap_or_else(|err| panic!("GET {path} failed: {err}"));
        assert!(
            response.status().is_success(),
            "GET {path} returned {}",
            response.status()
        );
    }

    let parse_response = client
        .post(format!("{base}/api/deeplink/parse"))
        .json(&json!({
            "url": "ccswitch://v1/import?resource=provider&app=claude&name=Test&endpoint=https%3A%2F%2Fapi.example.com&apiKey=test"
        }))
        .send()
        .await
        .expect("deeplink parse");
    assert!(parse_response.status().is_success());
    let parsed: serde_json::Value = parse_response.json().await.expect("deeplink json");
    assert_eq!(parsed["resource"], "provider");
    assert_eq!(parsed["app"], "claude");

    let validate_response = client
        .post(format!("{base}/api/claude-mcp/validate"))
        .json(&json!({ "command": "sh" }))
        .send()
        .await
        .expect("mcp validate");
    assert!(validate_response.status().is_success());

    handle.abort();
}
