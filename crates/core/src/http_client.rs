//! Shared outbound HTTP client.
//!
//! OAuth, model fetching, usage scripts, and health checks share one pool.
//! The only mutable application-level networking state is the user's
//! configured proxy, which [`reload`] applies by rebuilding the pool — the
//! client is otherwise handed out as an immutable clone.

use once_cell::sync::OnceCell;
use reqwest::Client;
use std::sync::RwLock;
use std::time::Duration;

static GLOBAL_CLIENT: OnceCell<RwLock<Client>> = OnceCell::new();

/// Eagerly initialize the shared client. Calling this more than once is safe.
pub fn init() -> Result<(), String> {
    if GLOBAL_CLIENT.get().is_some() {
        return Ok(());
    }
    let client = build_client()?;
    let _ = GLOBAL_CLIENT.set(RwLock::new(client));
    Ok(())
}

/// Return the shared client, initializing it lazily on first use.
pub fn get() -> Client {
    GLOBAL_CLIENT
        .get_or_init(|| {
            let client = build_client().unwrap_or_else(|error| {
                log::error!("failed to build shared HTTP client: {error}");
                Client::new()
            });
            RwLock::new(client)
        })
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Rebuild the shared client from the current proxy settings. Call this after
/// the user changes the app-wide proxy so already-running call sites (update
/// checks, model fetching, balance/subscription lookups, ...) pick it up
/// without an app restart.
pub fn reload() {
    let Some(lock) = GLOBAL_CLIENT.get() else {
        return;
    };
    match build_client() {
        Ok(client) => {
            *lock.write().unwrap_or_else(|error| error.into_inner()) = client;
        }
        Err(error) => log::error!("failed to rebuild shared HTTP client: {error}"),
    }
}

fn build_client() -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // Preserve response encodings for callers that process them explicitly.
        .no_gzip()
        .no_brotli()
        .no_deflate();
    if let Some(proxy_url) = crate::settings::get_settings()
        .proxy
        .as_ref()
        .and_then(|proxy| proxy.url())
    {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|error| format!("invalid proxy configuration: {error}"))?;
        builder = builder.proxy(proxy);
    }
    builder
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
