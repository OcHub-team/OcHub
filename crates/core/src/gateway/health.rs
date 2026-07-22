//! Periodic channel health probing.
//!
//! A lightweight reachability probe per enabled channel: any HTTP response
//! (even 401/404 — auth or path differences don't matter for liveness) counts
//! as healthy; connect/timeout errors and 5xx mark the channel unhealthy so the
//! router deprioritizes it until a later probe (or a successful live request)
//! recovers it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::db::Database;
use crate::gateway::types::{ChannelHealth, GatewayChannel};

/// Probe a single channel once.
pub async fn probe_channel(client: &reqwest::Client, channel: &GatewayChannel) -> ChannelHealth {
    let base = channel.base_url.trim_end_matches('/');
    let url = format!("{base}/v1/models");
    match client
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status >= 500 {
                ChannelHealth::Unhealthy(format!("HTTP {status}"))
            } else {
                ChannelHealth::Healthy
            }
        }
        Err(e) => ChannelHealth::Unhealthy(truncate(&e.to_string(), 200)),
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Probe all enabled channels once, updating the shared health map.
pub async fn probe_all(
    db: &Database,
    client: &reqwest::Client,
    health: &RwLock<HashMap<String, ChannelHealth>>,
) {
    let channels = match db.get_gateway_channels() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[gateway] health probe: channel list failed: {e}");
            return;
        }
    };
    for channel in channels.iter().filter(|c| c.enabled) {
        let result = probe_channel(client, channel).await;
        if let ChannelHealth::Unhealthy(reason) = &result {
            log::info!("[gateway] channel '{}' unhealthy: {reason}", channel.name);
        }
        health.write().await.insert(channel.id.clone(), result);
    }
    // Drop stale entries for deleted channels.
    let ids: std::collections::HashSet<&str> = channels.iter().map(|c| c.id.as_str()).collect();
    health
        .write()
        .await
        .retain(|id, _| ids.contains(id.as_str()));
}

/// Spawn the periodic prober; returns its join handle (abort to stop).
pub fn spawn_prober(
    db: Arc<Database>,
    client: reqwest::Client,
    health: Arc<RwLock<HashMap<String, ChannelHealth>>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if interval_secs == 0 {
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(30)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            probe_all(&db, &client, &health).await;
        }
    })
}
