//! Shared test-only helpers.
//!
//! A process-wide mutex used to serialize tests that mutate global environment
//! variables (`HOME`, `USERPROFILE`, `OCHUB_TEST_HOME`). cc-switch used the
//! `serial_test` crate for this; that crate is not a workspace dependency in
//! ochub-core, so this lock provides the same guarantee without a new dependency.
//!
//! All HOME-mutating tests across the crate should acquire this single lock so
//! they never run concurrently with one another.

use std::ffi::OsStr;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Acquire the global environment-mutation test lock.
///
/// Returns a guard that must be held for the duration of any test that reads or
/// writes process-global env vars affecting path resolution. Poisoning is
/// ignored so a panicking test does not deadlock the rest of the suite.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// Set a process-global env var from a test.
///
/// Rust 2024 made [`std::env::set_var`] unsafe: a concurrent reader in another
/// thread is undefined behavior. Callers hold [`env_lock`] (or their module's
/// own equivalent), which serializes the tests that mutate the environment, and
/// the test binary starts no background thread that reads it — so the write is
/// confined to the one thread doing it. Wrapping the two calls here keeps that
/// argument in one place instead of at all ~40 call sites.
pub(crate) fn set_var(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    unsafe { std::env::set_var(key, value) };
}

/// Remove a process-global env var from a test. Same reasoning as [`set_var`].
pub(crate) fn remove_var(key: impl AsRef<OsStr>) {
    unsafe { std::env::remove_var(key) };
}
