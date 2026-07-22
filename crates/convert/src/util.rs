//! Small shared helpers.

/// Unix-seconds timestamp for `created` fields (cosmetic; 0 on clock error).
pub(crate) fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Short random suffix for generated ids.
pub(crate) fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
