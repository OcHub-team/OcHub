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

/// A handle to the shared runtime, for callers that need to own the blocking
/// (the control API server thread parks on it for the life of the process).
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
        assert!(elapsed >= std::time::Duration::from_millis(15), "{elapsed:?}");
    }

    #[test]
    fn propagates_values_out_of_the_runtime() {
        init().expect("build runtime");
        let sum = futures::executor::block_on(run(async { (1..=10).sum::<u32>() }));
        assert_eq!(sum, 55);
    }
}
