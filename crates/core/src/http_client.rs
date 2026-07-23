//! Shared outbound HTTP client.
//!
//! OAuth, model fetching, usage scripts, and health checks share one pool with
//! no mutable application-level networking state.

use once_cell::sync::OnceCell;
use reqwest::Client;
use std::time::Duration;

static GLOBAL_CLIENT: OnceCell<Client> = OnceCell::new();

/// Eagerly initialize the shared client. Calling this more than once is safe.
pub fn init() -> Result<(), String> {
    if GLOBAL_CLIENT.get().is_some() {
        return Ok(());
    }
    let client = build_client()?;
    let _ = GLOBAL_CLIENT.set(client);
    Ok(())
}

/// Return the shared client, initializing it lazily on first use.
pub fn get() -> Client {
    GLOBAL_CLIENT
        .get_or_init(|| {
            build_client().unwrap_or_else(|error| {
                log::error!("failed to build shared HTTP client: {error}");
                Client::new()
            })
        })
        .clone()
}

fn build_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // Preserve response encodings for callers that process them explicitly.
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_client_initializes_idempotently() {
        super::init().unwrap();
        super::init().unwrap();
        let _ = super::get();
    }
}
