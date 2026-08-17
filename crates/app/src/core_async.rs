//! The bridge between gpui's executor and `ochub-core`'s tokio futures.
//!
//! `ochub-core` is async on tokio because the relay gateway is an HTTP server.
//! The UI runs on gpui's own executor, which is not tokio and has no reactor.
//! Awaiting a core future directly on a gpui task therefore does not merely run
//! slowly — anything touching the network or a timer panics with *"there is no
//! reactor running"*, and a panic unwinding across the objc stack aborts the
//! process rather than surfacing an error.
//!
//! Every crossing goes through [`run`], which drives the future on the shared
//! runtime and hands the result back over a channel that any executor can
//! await:
//!
//! ```ignore
//! cx.spawn(async move |this, cx| {
//!     let result = core_async::run(some_core_future).await;
//!     this.update(cx, |this, cx| { /* … */ }).ok();
//! })
//! .detach();
//! ```
//!
//! One runtime serves the whole process. Building a fresh
//! `new_current_thread` runtime per call — which four call sites used to do —
//! pays for reactor setup and teardown on every button press, and each site
//! grew its own "failed to build a runtime" error path for a failure that can
//! now only happen once, at startup.
//!
//! `cx.background_spawn` is not a substitute: that executor is also not Tokio.
//! A source-level regression test walks `crates/app/src` and fails if a
//! `cx.spawn` / `background_spawn` body awaits `WorkspaceBackend`, reqwest, or
//! `tokio::time` / `tokio::process` without going through [`run`].

use std::future::Future;
use std::io;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Build the process-wide runtime. Call once, early in `main`, before anything
/// can reach [`run`] or [`handle`].
///
/// Returns the error rather than panicking so startup can report it in the UI:
/// a process that cannot build a runtime still opens a window, it just cannot
/// reach the network.
pub fn init() -> io::Result<()> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("ochub-core")
        .build()?;
    // `set` only fails on a race, in which case another thread installed an
    // equivalent runtime and this one is simply dropped.
    let _ = RUNTIME.set(runtime);
    Ok(())
}

/// A handle to the shared runtime for background application services.
///
/// # Panics
/// If [`init`] has not run. That is a programming error in startup ordering,
/// not a runtime condition.
pub fn handle() -> &'static Handle {
    RUNTIME
        .get()
        .expect("core_async::init must run before the runtime is used")
        .handle()
}

/// Run a core future on the shared tokio runtime and await its result from
/// whatever executor the caller is on.
///
/// The future never runs on the calling thread, so this is safe to await
/// directly inside `cx.spawn` — there is no need to wrap it in
/// `cx.background_spawn` as well.
pub async fn run<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    handle().spawn(async move {
        // A receiver dropped before completion just means the caller went
        // away; the send failing is not an error.
        let _ = tx.send(future.await);
    });
    // The sender is only dropped without sending if the runtime is shutting
    // down, which happens at process exit.
    rx.await
        .expect("core_async runtime dropped a task before it completed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn init_is_idempotent_and_yields_a_usable_handle() {
        init().expect("build runtime");
        init().expect("second init is a no-op");
        assert!(handle().metrics().num_workers() > 0);
    }

    /// The point of the whole module: a future needing a tokio reactor
    /// completes when driven through `run`, on a plain non-tokio executor.
    /// Awaiting the same future directly here panics with "there is no reactor
    /// running", which is what used to reach the sync page.
    #[test]
    fn drives_futures_that_require_a_reactor() {
        init().expect("build runtime");
        let elapsed = futures::executor::block_on(run(async {
            let start = std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            start.elapsed()
        }));
        assert!(
            elapsed >= std::time::Duration::from_millis(15),
            "{elapsed:?}"
        );
    }

    #[test]
    fn propagates_values_out_of_the_runtime() {
        init().expect("build runtime");
        let sum = futures::executor::block_on(run(async { (1..=10).sum::<u32>() }));
        assert_eq!(sum, 55);
    }

    /// The Network Test crash: poll a reactor future on gpui / `futures`
    /// executor and Tokio panics instead of returning `Err`.
    #[test]
    #[should_panic(expected = "there is no reactor running")]
    fn awaiting_a_reactor_future_off_tokio_panics() {
        futures::executor::block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn proxy_probe_returns_through_run_without_a_caller_reactor() {
        init().expect("build runtime");
        let proxy = ochub_core::settings::ProxySettings::default();
        let result = futures::executor::block_on(run(async move {
            ochub_core::services::network_proxy::check_connection(&proxy).await
        }));
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn draft_model_fetch_returns_through_run_without_a_caller_reactor() {
        init().expect("build runtime");
        let result = futures::executor::block_on(run(async {
            ochub_core::services::model_fetch::fetch_models("", "", false, None, None).await
        }));
        assert_eq!(
            result.err().as_deref(),
            Some("API Key is required to fetch models")
        );
    }

    #[test]
    fn draft_speedtest_returns_through_run_without_a_caller_reactor() {
        init().expect("build runtime");
        let result = futures::executor::block_on(run(async {
            ochub_core::services::SpeedtestService::test_endpoints(Vec::new(), Some(8)).await
        }));
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn draft_balance_returns_through_run_without_a_caller_reactor() {
        init().expect("build runtime");
        let result = futures::executor::block_on(run(async {
            ochub_core::services::balance::get_balance("https://example.com", "").await
        }));
        let result = result.expect("empty key is a usage error, not a transport failure");
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("API key is empty"));
    }

    #[test]
    fn scanner_flags_an_unwrapped_backend_await() {
        let src = r#"
            cx.spawn(async move |this, cx| {
                let result = backend.settings().await;
            })
            .detach();
        "#;
        let hits = scan_source(Path::new("fake.rs"), src);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].contains("WorkspaceBackend"), "{hits:?}");
    }

    #[test]
    fn scanner_accepts_a_wrapped_backend_await() {
        let src = r#"
            cx.spawn(async move |this, cx| {
                let result = crate::core_async::run(async move { backend.settings().await }).await;
            })
            .detach();
        "#;
        assert_eq!(scan_source(Path::new("fake.rs"), src), Vec::<String>::new());
    }

    #[test]
    fn scanner_flags_an_unwrapped_core_http_probe() {
        let src = r#"
            cx.spawn(async move |this, cx| {
                let result = ochub_core::services::model_fetch::fetch_models(&u, &k, false, None, None).await;
            });
        "#;
        let hits = scan_source(Path::new("fake.rs"), src);
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    /// Every UI crossing into Tokio-backed I/O must go through [`run`].
    /// `cx.background_spawn` is another non-Tokio executor and does not count.
    #[test]
    fn ui_sources_cross_tokio_only_through_run() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        collect_rs_files(&root, &mut hits);
        hits.sort();
        assert!(
            !hits.is_empty(),
            "expected to find Rust sources under {}",
            root.display()
        );
        let mut violations = Vec::new();
        for path in hits {
            let file = fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!("read {}: {error}", path.display());
            });
            // Fixture snippets in unit tests would trip the scanner; production
            // call sites in this crate live above the `#[cfg(test)]` module.
            let source = file
                .split_once("#[cfg(test)]")
                .map(|(code, _)| code)
                .unwrap_or(&file);
            violations.extend(scan_source(&path, source));
        }
        assert!(
            violations.is_empty(),
            "UI task awaited Tokio-backed I/O without core_async::run:\n{}",
            violations.join("\n")
        );
    }

    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read app src") {
            let entry = entry.expect("read dir entry");
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    fn scan_source(path: &Path, source: &str) -> Vec<String> {
        let mut hits = Vec::new();
        for callee in ["cx.spawn", "background_spawn"] {
            for (line, args) in find_call_args(source, callee) {
                if let Some(reason) = unwrapped_tokio_crossing(args) {
                    hits.push(format!("{}:{line}: {reason}", path.display()));
                }
            }
        }
        hits
    }

    fn unwrapped_tokio_crossing(args: &str) -> Option<&'static str> {
        if args.contains("core_async::run") {
            return None;
        }
        if args.contains("tokio::time::") {
            return Some("tokio::time on the gpui executor");
        }
        if args.contains("tokio::process::") {
            return Some("tokio::process on the gpui executor");
        }
        if awaited_after(args, "reqwest::") {
            return Some("reqwest await on the gpui executor");
        }
        if awaited_after(args, "model_fetch::fetch_models") {
            return Some("model_fetch on the gpui executor");
        }
        if awaited_after(args, "SpeedtestService::test_endpoints")
            || awaited_after(args, "SpeedtestService::")
        {
            return Some("SpeedtestService on the gpui executor");
        }
        if awaited_after(args, "balance::get_balance") {
            return Some("balance::get_balance on the gpui executor");
        }
        if awaited_after(args, "network_proxy::") {
            return Some("network_proxy on the gpui executor");
        }
        if awaited_after(args, "update::install::prepare")
            || awaited_after(args, "update::check_for_updates")
            || awaited_after(args, "key_quota::")
            || awaited_after(args, "RemoteClient::")
        {
            return Some("tokio-backed core/remote await on the gpui executor");
        }
        if backend_method_awaited(args) {
            return Some("WorkspaceBackend await on the gpui executor");
        }
        None
    }

    fn collapse_ws(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut prev_space = false;
        for ch in source.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
        out
    }

    fn awaited_after(source: &str, needle: &str) -> bool {
        let compact = collapse_ws(source);
        let Some(at) = compact.find(needle) else {
            return false;
        };
        compact[at..]
            .chars()
            .take(400)
            .collect::<String>()
            .contains(".await")
    }

    fn backend_method_awaited(source: &str) -> bool {
        let compact = collapse_ws(source);
        let mut rest = compact.as_str();
        while let Some(at) = rest.find("backend.") {
            let after = &rest[at + "backend.".len()..];
            let method: String = after
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let call = after.get(method.len()..).unwrap_or("");
            if !matches!(method.as_str(), "clone" | "is_remote") && call.starts_with('(') {
                let window = after.chars().take(method.len() + 300).collect::<String>();
                if window.contains(".await") {
                    return true;
                }
            }
            rest = &rest[at + "backend.".len()..];
        }
        false
    }

    fn find_call_args<'a>(source: &'a str, callee: &str) -> Vec<(usize, &'a str)> {
        let mut out = Vec::new();
        let mut search_from = 0;
        while let Some(rel) = source[search_from..].find(callee) {
            let at = search_from + rel;
            let after_name = at + callee.len();
            if !is_isolated_ident(source, at, after_name) {
                search_from = after_name;
                continue;
            }
            let Some(open_at) = next_non_ws(source, after_name)
                .filter(|&idx| source.as_bytes().get(idx) == Some(&b'('))
            else {
                search_from = after_name;
                continue;
            };
            let Some(inner) = extract_balanced(&source[open_at..], b'(', b')') else {
                search_from = after_name;
                continue;
            };
            out.push((byte_line(source, at), inner));
            search_from = open_at + inner.len() + 2;
        }
        out
    }

    fn is_isolated_ident(source: &str, start: usize, end: usize) -> bool {
        let bytes = source.as_bytes();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        before_ok && after_ok
    }

    fn is_ident_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    fn next_non_ws(source: &str, from: usize) -> Option<usize> {
        source[from..]
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map(|(rel, _)| from + rel)
    }

    fn byte_line(source: &str, at: usize) -> usize {
        source[..at].bytes().filter(|byte| *byte == b'\n').count() + 1
    }

    fn extract_balanced(source: &str, open: u8, close: u8) -> Option<&str> {
        let bytes = source.as_bytes();
        if bytes.first() != Some(&open) {
            return None;
        }
        let mut depth = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = i.saturating_add(2);
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            i = i.saturating_add(2);
                            continue;
                        }
                        if bytes[i] == b'"' {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                b if b == open => {
                    depth += 1;
                    i += 1;
                }
                b if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&source[1..i]);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        None
    }
}
