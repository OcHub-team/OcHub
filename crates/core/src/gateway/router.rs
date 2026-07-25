//! Channel selection: model name → ordered candidate channels.
//!
//! Selection mirrors relay-station routing:
//! 1. filter enabled channels whose model matchers cover the requested model
//!    (and that are not circuit-open / marked unhealthy),
//! 2. group by ascending `priority`,
//! 3. within a group, order by weighted random draw,
//! 4. concatenate groups → the failover candidate list. The forwarder tries
//!    candidates in order until one succeeds.

use super::types::GatewayChannel;

/// Deterministic-input weighted ordering seed. Callers pass entropy (e.g. from
/// `uuid`) so this module stays clock/RNG-free and unit-testable.
pub fn candidates_for_model(
    channels: &[GatewayChannel],
    model: &str,
    is_excluded: impl Fn(&GatewayChannel) -> bool,
    entropy: impl FnMut() -> u64,
) -> Vec<GatewayChannel> {
    candidates_for_model_ranked(channels, model, is_excluded, |_| 0, entropy)
}

/// Like [`candidates_for_model`], with a request-specific preference inside
/// each explicit priority group. This lets the gateway prefer a channel that
/// speaks the client's native dialect without overriding a user's configured
/// channel priority.
pub fn candidates_for_model_ranked(
    channels: &[GatewayChannel],
    model: &str,
    is_excluded: impl Fn(&GatewayChannel) -> bool,
    rank: impl Fn(&GatewayChannel) -> u8,
    mut entropy: impl FnMut() -> u64,
) -> Vec<GatewayChannel> {
    let mut eligible: Vec<&GatewayChannel> = channels
        .iter()
        .filter(|c| c.enabled && c.matches_model(model) && !is_excluded(c))
        .collect();
    eligible.sort_by_key(|c| (c.priority, rank(c)));

    let mut out: Vec<GatewayChannel> = Vec::with_capacity(eligible.len());
    let mut i = 0;
    while i < eligible.len() {
        // Collect one explicit-priority + request-preference group.
        let prio = eligible[i].priority;
        let preference = rank(eligible[i]);
        let mut group: Vec<&GatewayChannel> = Vec::new();
        while i < eligible.len() && eligible[i].priority == prio && rank(eligible[i]) == preference
        {
            group.push(eligible[i]);
            i += 1;
        }
        // Weighted draw without replacement.
        while !group.is_empty() {
            let total: u64 = group.iter().map(|c| c.weight.max(1) as u64).sum();
            let mut ticket = entropy() % total;
            let mut chosen = 0;
            for (idx, c) in group.iter().enumerate() {
                let w = c.weight.max(1) as u64;
                if ticket < w {
                    chosen = idx;
                    break;
                }
                ticket -= w;
            }
            out.push(group.remove(chosen).clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::Dialect;

    fn ch(id: &str, priority: i32, weight: u32, models: &[&str]) -> GatewayChannel {
        GatewayChannel {
            id: id.into(),
            name: id.into(),
            dialect: Dialect::Messages,
            base_url: "https://x".into(),
            api_key: String::new(),
            path_override: None,
            models: models.iter().map(|s| s.to_string()).collect(),
            model_override: None,
            priority,
            weight,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        }
    }

    #[test]
    fn filters_and_orders_by_priority() {
        let mut disabled = ch("off", 0, 1, &[]);
        disabled.enabled = false;
        let channels = vec![
            ch("backup", 10, 1, &[]),
            ch("primary", 0, 1, &["claude-*"]),
            ch("wrong-model", 0, 1, &["gpt-*"]),
            disabled,
        ];
        let got = candidates_for_model(&channels, "claude-x", |_| false, || 0);
        let ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["primary", "backup"]);
    }

    #[test]
    fn excluded_channels_are_skipped() {
        let channels = vec![ch("a", 0, 1, &[]), ch("b", 0, 1, &[])];
        let got = candidates_for_model(&channels, "m", |c| c.id == "a", || 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");
    }

    #[test]
    fn weighted_draw_respects_entropy() {
        let channels = vec![ch("light", 0, 1, &[]), ch("heavy", 0, 3, &[])];
        // entropy 1 → ticket 1: falls into "light"(w1)? ticket<1? no (ticket=1),
        // then heavy. So first draw picks heavy.
        let got = candidates_for_model(&channels, "m", |_| false, || 1);
        assert_eq!(got[0].id, "heavy");
        assert_eq!(got[1].id, "light");
        // entropy 0 → picks light first.
        let got = candidates_for_model(&channels, "m", |_| false, || 0);
        assert_eq!(got[0].id, "light");
    }

    #[test]
    fn all_candidates_survive_for_failover() {
        let channels = vec![ch("a", 0, 2, &[]), ch("b", 0, 1, &[]), ch("c", 5, 1, &[])];
        let got = candidates_for_model(&channels, "m", |_| false, || 0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].id, "c"); // lower priority always last
    }

    #[test]
    fn request_rank_orders_equal_priority_groups() {
        let mut messages = ch("messages", 0, 1, &[]);
        messages.dialect = Dialect::Messages;
        let mut responses = ch("responses", 0, 1, &[]);
        responses.dialect = Dialect::Responses;
        let got = candidates_for_model_ranked(
            &[responses, messages],
            "m",
            |_| false,
            |channel| u8::from(channel.dialect != Dialect::Messages),
            || 0,
        );
        assert_eq!(got[0].id, "messages");
    }
}
