//! Pure configured routing semantics shared by the forwarder and inspector.
use super::types::{Dialect, GatewayAppModelPolicy, GatewayModelRule, GatewayRoute};

/// Can requests of `inlet` dialect be served by a channel of `channel` dialect?
/// All nine pairs convert (chat-upstream reverse conversion is lossy: thinking
/// signatures and cache markers have no chat representation). Kept as the single
/// source of truth so UI/apply guards lift automatically with the matrix.
pub fn conversion_supported(_inlet: Dialect, _channel: Dialect) -> bool {
    true
}

pub(crate) fn channel_supports_request(
    inlet: Dialect,
    channel: Dialect,
    remote_compaction: bool,
) -> bool {
    conversion_supported(inlet, channel) && (!remote_compaction || channel == Dialect::Responses)
}

/// Prefer the client's native wire format, then the richer Messages ↔
/// Responses bridge, and keep conversions involving Chat as the last automatic
/// choice because Chat cannot preserve every advanced reasoning capability.
pub(crate) fn dialect_conversion_rank(inlet: Dialect, upstream: Dialect) -> u8 {
    if inlet == upstream {
        0
    } else if !matches!(inlet, Dialect::Chat) && !matches!(upstream, Dialect::Chat) {
        1
    } else {
        2
    }
}

pub(crate) fn request_model_rule<'a>(
    policy: Option<&'a GatewayAppModelPolicy>,
    route: Option<&'a GatewayRoute>,
    model: &str,
) -> Option<&'a GatewayModelRule> {
    match policy {
        Some(policy) => policy.rule_for_model(model),
        None => route.and_then(|route| route.rule_for_model(model)),
    }
}

pub(crate) fn request_model_override<'a>(
    rule: Option<&'a GatewayModelRule>,
    policy: Option<&'a GatewayAppModelPolicy>,
    route: Option<&'a GatewayRoute>,
) -> Option<&'a str> {
    model_override(
        rule,
        match policy {
            Some(policy) => policy.fallback_model.as_deref(),
            None => route.and_then(|route| route.default_model.as_deref()),
        },
    )
}

pub(crate) fn model_override<'a>(
    rule: Option<&'a GatewayModelRule>,
    fallback: Option<&'a str>,
) -> Option<&'a str> {
    match rule {
        // A matching pass-through suppresses an otherwise configured fallback.
        Some(rule) => rule.upstream_model_override(),
        None => fallback,
    }
}
