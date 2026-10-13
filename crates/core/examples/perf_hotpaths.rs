//! Reproducible, isolated benchmarks for provider loading and session lookup.
//! Run with: cargo run --profile qa -p ochub-core --example perf_hotpaths
//! Fixture creation is excluded from timings. No production data is needed.

use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use ochub_core::application::Application;
use ochub_core::settings::CustomEndpoint;
use ochub_core::{AppId, AppState, Database, Provider, ProviderMeta};
use serde_json::{Value, json};

fn measure(name: &str, size: usize, mut operation: impl FnMut()) -> Value {
    for _ in 0..3 {
        operation();
    }
    let mut samples = Vec::new();
    for _ in 0..9 {
        let start = Instant::now();
        operation();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    json!({
        "name": name, "size": size, "samples": samples.len(),
        "medianMs": samples[samples.len() / 2],
        "minMs": samples[0], "maxMs": samples[samples.len() - 1],
    })
}

fn provider_fixture(count: usize) -> Arc<Database> {
    let db = Arc::new(Database::memory().unwrap());
    for index in 0..count {
        let mut provider = Provider::with_id(
            format!("provider-{index:05}"),
            format!("Provider {index:05}"),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "synthetic-benchmark-secret",
                    "ANTHROPIC_BASE_URL": "https://example.invalid/v1"
                },
                "extra": "x".repeat(1024),
            }),
            None,
        );
        provider.sort_index = Some(index);
        let mut meta = ProviderMeta::default();
        for endpoint in 0..3 {
            let url = format!("https://example.invalid/{index}/{endpoint}");
            meta.custom_endpoints.insert(
                url.clone(),
                CustomEndpoint {
                    url,
                    added_at: endpoint,
                    last_used: None,
                },
            );
        }
        provider.meta = Some(meta);
        db.save_provider("claude", &provider).unwrap();
    }
    db
}

fn seed_sessions(root: &Path, unrelated_count: usize) {
    let codex = root.join(".codex/sessions");
    let claude = root.join(".claude/projects/perf");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&claude).unwrap();
    for index in 0..unrelated_count {
        let id = uuid::Uuid::from_u128(index as u128 + 1).to_string();
        let lines = format!(
            "{}\n{}\n",
            json!({"type":"session_meta", "timestamp":"2026-10-13T00:00:00Z",
                "payload":{"id":id,"cwd":"/perf","source":"cli"}}),
            json!({"type":"response_item", "timestamp":"2026-10-13T00:00:01Z",
                "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Benchmark session"}]}}),
        );
        std::fs::write(codex.join(format!("rollout-{id}.jsonl")), lines).unwrap();
    }
    for index in 0..2 {
        let content = json!({"type":"user", "sessionId":format!("claude-{index}"),
            "cwd":"/perf", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"user","content":"Benchmark session"}});
        std::fs::write(
            claude.join(format!("claude-{index}.jsonl")),
            format!("{content}\n"),
        )
        .unwrap();
    }
}

fn main() {
    // A child gets a dedicated environment before it creates any threads.
    // This avoids changing process-global paths while workers are running.
    if std::env::var_os("OCHUB_PERF_WORKER").is_none() {
        let fixture = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .env("OCHUB_PERF_WORKER", "1")
            .env("OCHUB_TEST_HOME", fixture.path())
            .env("XDG_DATA_HOME", fixture.path().join(".local/share"))
            .env_remove("OCHUB_DATA_DIR")
            .env_remove("OCHUB_SOCKET")
            .status()
            .unwrap();
        assert!(status.success(), "benchmark worker failed");
        return;
    }

    let root = std::env::var_os("OCHUB_TEST_HOME").expect("isolated fixture home");
    let app_id = AppId::parse("claude").unwrap();
    let mut results = Vec::new();
    for count in [25, 250, 1000] {
        let db = provider_fixture(count);
        let application = Application::from_state(Arc::new(AppState::new(db.clone())));
        ochub_core::settings::set_current_provider(
            &ochub_core::AppType::Claude,
            Some("provider-00000"),
        )
        .unwrap();
        results.push(measure("effective_current_provider", count, || {
            let current = ochub_core::settings::get_effective_current_provider(
                &db,
                &ochub_core::AppType::Claude,
            )
            .unwrap();
            assert_eq!(current.as_deref(), Some("provider-00000"));
            black_box(current);
        }));
        results.push(measure("get_all_providers", count, || {
            let providers = db.get_all_providers("claude").unwrap();
            assert_eq!(providers.len(), count);
            black_box(providers);
        }));
        results.push(measure("provider_page_legacy", count, || {
            let listed = application.list_providers(&app_id).unwrap();
            let details = listed
                .iter()
                .map(|item| application.get_provider(&app_id, &item.id, false).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(details.len(), count);
            black_box((listed, details));
        }));
        results.push(measure("provider_page_snapshot", count, || {
            let snapshot = application.provider_snapshot(&app_id).unwrap();
            assert_eq!(snapshot.providers.len(), count);
            assert_eq!(snapshot.base_urls.len(), count);
            assert_eq!(snapshot.current.as_deref(), Some("provider-00000"));
            black_box(snapshot);
        }));
    }

    seed_sessions(Path::new(&root), 2000);
    let application = Application::from_state(Arc::new(AppState::new(Arc::new(
        Database::memory().unwrap(),
    ))));
    results.push(measure(
        "filtered_sessions_with_unrelated_history",
        2000,
        || {
            let sessions = application
                .list_sessions(std::slice::from_ref(&app_id), None)
                .unwrap();
            assert_eq!(sessions.len(), 2);
            black_box(sessions);
        },
    ));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "architecture": std::env::consts::ARCH,
            "debugAssertions": cfg!(debug_assertions),
            "warmupIterations": 3,
            "results": results,
        }))
        .unwrap()
    );
}
