//! Shared test-only helpers.
//!
//! A process-wide mutex used to serialize tests that mutate global environment
//! variables (`HOME`, `USERPROFILE`, `CC_SWITCH_TEST_HOME`). cc-switch used the
//! `serial_test` crate for this; that crate is not a workspace dependency in
//! routedeck-core, so this lock provides the same guarantee without a new dependency.
//!
//! All HOME-mutating tests across the crate should acquire this single lock so
//! they never run concurrently with one another.

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
