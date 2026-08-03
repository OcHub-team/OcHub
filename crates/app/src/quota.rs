//! The quota line: one truncated row per card, with the overflow in a dialog.
//!
//! Provider cards (official subscription quota) and station cards (relay quota)
//! both read the same `UsageResult`. They used to carry a full-size button
//! sitting beside *Edit* and *Delete* and a private formatter each — a read-only
//! refresh dressed as a command, formatted two ways that drifted apart. Quota is
//! a *state* a card reports, so here it is one line of status text whose leading
//! `⟳` is the refresh affordance.
//!
//! The line never wraps. An official subscription returns up to four tiers
//! (5-hour, weekly, weekly Opus, weekly Sonnet) and those never fit a card, so
//! the row truncates and [`detail_body`] shows the whole set. That keeps all
//! four states — never queried, in flight, ready, failed — exactly one line
//! tall, so a card does not change height when someone checks its quota and the
//! list underneath does not jump.
//!
//! Percentages are **remaining**, not used, and the wording says so: a bare
//! `82%` leaves the reader guessing which way it runs.

use chrono::{DateTime, Utc};
use gpui::{App, ClickEvent, FontWeight, SharedString, Window, div, prelude::*, px};
use ochub_core::UsageResult;
use ochub_core::model::UsageData;

use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
use crate::tf;
use crate::theme;

/// Remaining share at or below which a tier reads as running out and the line
/// turns red. Only known when the source reports a total to divide by.
const LOW_REMAINING: f64 = 0.1;

/// One quota tier, already localized and formatted.
pub struct QuotaEntry {
    label: String,
    value: String,
    reset: Option<String>,
    /// Why the upstream marked this tier unusable, if it did.
    invalid: Option<String>,
    low: bool,
}

impl QuotaEntry {
    /// `5 小时 剩 82%（3 小时后重置）` — the inline form, one tier's worth.
    fn inline(&self) -> String {
        match &self.reset {
            Some(reset) => tf!(
                k::QUOTA_ENTRY_WITH_RESET,
                label = self.label,
                value = self.value,
                reset = reset,
            ),
            None => tf!(k::QUOTA_ENTRY, label = self.label, value = self.value),
        }
    }
}

/// What the card knows about its quota right now.
pub enum QuotaState<'a> {
    /// Never queried in this session.
    Idle,
    /// A query is in flight.
    Loading,
    /// The last query succeeded and returned these tiers.
    Ready(&'a [QuotaEntry]),
    /// The last query failed; the reason went to a notification, because it is
    /// far longer than the line can hold.
    Failed,
}

/// Localized tiers from a query result, or the reason there are none.
///
/// A result that succeeded but carries nothing usable is an `Err` too: "no
/// quota returned" is what the reader needs, and an empty `Ok` would render as
/// a blank line that looks like a layout bug.
pub fn parse(result: &UsageResult) -> Result<Vec<QuotaEntry>, String> {
    if !result.success {
        return Err(result
            .error
            .clone()
            .unwrap_or_else(|| raw(k::QUOTA_NO_DATA).to_string()));
    }
    let entries: Vec<QuotaEntry> = result
        .data
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(entry)
        .collect();
    if entries.is_empty() {
        Err(raw(k::QUOTA_NO_DATA).to_string())
    } else {
        Ok(entries)
    }
}

/// The card row: `⟳` then a single truncated line of tiers.
///
/// The icon always refreshes. The text opens the detail dialog once there is
/// something to detail, and otherwise refreshes too, so that a card showing
/// "click to retry" retries wherever it is clicked.
pub fn line(
    id_prefix: &str,
    name: &str,
    state: QuotaState<'_>,
    refresh: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    detail: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let (text, color, ready) = match state {
        QuotaState::Idle => (t(k::QUOTA_ACTION_QUERY), theme::muted(), false),
        QuotaState::Loading => (t(k::QUOTA_ACTION_QUERYING), theme::muted(), false),
        QuotaState::Failed => (t(k::QUOTA_FAILED), theme::red(), false),
        QuotaState::Ready(entries) => (
            SharedString::from(summary(entries)),
            if entries
                .iter()
                .any(|entry| entry.low || entry.invalid.is_some())
            {
                theme::red()
            } else {
                theme::accent()
            },
            true,
        ),
    };

    // Both hit targets refresh until there is something to detail, so the
    // handler is shared rather than duplicated at every call site.
    let refresh = std::rc::Rc::new(refresh);
    let refresh_icon = refresh.clone();

    div()
        .flex()
        .flex_row()
        .items_center()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!("{id_prefix}-refresh")))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(tf!(k::QUOTA_REFRESH_ARIA, name = name)))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(18.))
                .h(px(18.))
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .child(icon(IconName::Refresh, color, 12.))
                .on_click(move |event, window, cx| refresh_icon(event, window, cx)),
        )
        .child({
            // The text is a control in both cases, so it is announced as one in
            // both cases — only the sentence differs.
            let summary = div()
                .id(SharedString::from(format!("{id_prefix}-summary")))
                .role(gpui::Role::Button)
                .min_w_0()
                .text_color(color)
                .text_xs()
                .truncate()
                .cursor_pointer()
                .hover(|style| style.text_color(theme::text()))
                .child(text);
            if ready {
                summary
                    .aria_label(SharedString::from(tf!(k::QUOTA_DETAIL_ARIA, name = name)))
                    .on_click(detail)
            } else {
                summary
                    .aria_label(SharedString::from(tf!(k::QUOTA_REFRESH_ARIA, name = name)))
                    .on_click(move |event, window, cx| refresh(event, window, cx))
            }
        })
}

/// The dialog body: every tier on its own row, nothing truncated.
pub fn detail_body(entries: &[QuotaEntry]) -> gpui::Div {
    if entries.is_empty() {
        return div().flex().flex_col().child(
            div()
                .text_color(theme::subtext())
                .text_sm()
                .child(t(k::QUOTA_DETAIL_EMPTY)),
        );
    }
    entries.iter().fold(
        div().flex().flex_col().gap_3(),
        |body,
         QuotaEntry {
             label,
             value,
             reset,
             invalid,
             low,
         }| {
            body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(label.clone())),
                            )
                            .child(
                                div()
                                    .text_color(if *low { theme::red() } else { theme::text() })
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(value.clone())),
                            ),
                    )
                    .when_some(reset.clone(), |row, reset| {
                        row.child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(SharedString::from(reset)),
                        )
                    })
                    .when_some(invalid.clone(), |row, message| {
                        row.child(
                            div()
                                .text_color(theme::red())
                                .text_xs()
                                .child(SharedString::from(tf!(
                                    k::QUOTA_INVALID,
                                    message = message
                                ))),
                        )
                    }),
            )
        },
    )
}

fn summary(entries: &[QuotaEntry]) -> String {
    entries
        .iter()
        .map(QuotaEntry::inline)
        .collect::<Vec<_>>()
        .join(raw(k::QUOTA_SEPARATOR))
}

fn entry(item: &UsageData) -> Option<QuotaEntry> {
    let extra = Extra::parse(item.extra.as_deref());
    let (value, low) = value(item, &extra)?;
    Some(QuotaEntry {
        label: label(item.plan_name.as_deref().unwrap_or_default(), &extra),
        value,
        reset: extra.resets_at.as_deref().and_then(reset),
        invalid: (item.is_valid == Some(false))
            .then(|| item.invalid_message.clone())
            .flatten(),
        low,
    })
}

/// A tier's name. The identifiers are what the upstream APIs send; anything
/// unrecognized is shown as-is rather than dropped, since a relay is free to
/// invent its own plan names.
fn label(plan_name: &str, extra: &Extra) -> String {
    match plan_name {
        "five_hour" => raw(k::QUOTA_TIER_FIVE_HOUR).to_string(),
        "seven_day" | "weekly_limit" => raw(k::QUOTA_TIER_WEEKLY).to_string(),
        "seven_day_opus" => raw(k::QUOTA_TIER_WEEKLY_OPUS).to_string(),
        "seven_day_sonnet" => raw(k::QUOTA_TIER_WEEKLY_SONNET).to_string(),
        "monthly_limit" => raw(k::QUOTA_TIER_MONTHLY).to_string(),
        "" => extra
            .plan_label
            .clone()
            .unwrap_or_else(|| raw(k::QUOTA_TIER_BALANCE).to_string()),
        other => other.replace('_', " "),
    }
}

/// The formatted remaining amount, and whether it is running out.
///
/// `None` means the source reported no number at all for this tier, which is
/// not worth a row.
fn value(item: &UsageData, extra: &Extra) -> Option<(String, bool)> {
    if extra.unlimited {
        return Some((raw(k::QUOTA_VALUE_UNLIMITED).to_string(), false));
    }
    let remaining = item
        .remaining
        .or_else(|| Some(item.total? - item.used?))
        .filter(|value| value.is_finite())?;

    // A percent tier already *is* the fraction, so it needs no total beside it:
    // "剩 82% / 100%" says nothing "剩 82%" does not.
    if item.unit.as_deref() == Some("%") {
        let percent = remaining.clamp(0.0, 100.0);
        return Some((
            tf!(
                k::QUOTA_VALUE_REMAINING_PERCENT,
                percent = format!("{percent:.0}")
            ),
            percent <= LOW_REMAINING * 100.0,
        ));
    }

    let amount = money(remaining, item.unit.as_deref());
    match item.total.filter(|total| *total > 0.0) {
        Some(total) => Some((
            tf!(
                k::QUOTA_VALUE_REMAINING_OF,
                value = amount,
                total = money(total, item.unit.as_deref()),
            ),
            remaining / total <= LOW_REMAINING,
        )),
        // Without a total there is no scale, so no threshold to be under.
        None => Some((tf!(k::QUOTA_VALUE_REMAINING, value = amount), false)),
    }
}

fn money(value: f64, unit: Option<&str>) -> String {
    match unit {
        Some("USD") => format!("${value:.2}"),
        Some(unit) if !unit.is_empty() => format!("{value:.2} {unit}"),
        _ => format!("{value:.2}"),
    }
}

/// A reset instant as time remaining. Anything unparseable is passed through
/// untouched: it came from the upstream and is at least true.
fn reset(at: &str) -> Option<String> {
    let Ok(at) = DateTime::parse_from_rfc3339(at) else {
        return Some(at.to_string());
    };
    let left = at.with_timezone(&Utc) - Utc::now();
    let minutes = left.num_minutes();
    Some(match () {
        () if left.num_seconds() <= 0 => raw(k::QUOTA_RESET_DONE).to_string(),
        () if minutes < 1 => raw(k::QUOTA_RESET_SOON).to_string(),
        () if minutes < 60 => tf!(k::QUOTA_RESET_MINUTES, minutes = minutes),
        () if left.num_hours() < 24 => tf!(k::QUOTA_RESET_HOURS, hours = left.num_hours()),
        () => tf!(k::QUOTA_RESET_DAYS, days = left.num_days()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assertions are on the numbers and the flags, never on the prose around
    /// them: the catalog decides the wording and the tests run under whichever
    /// locale happens to be installed.
    fn tier(plan: &str, remaining: Option<f64>, unit: &str) -> UsageData {
        UsageData {
            plan_name: Some(plan.to_string()),
            extra: None,
            is_valid: Some(true),
            invalid_message: None,
            total: None,
            used: None,
            remaining,
            unit: Some(unit.to_string()),
        }
    }

    fn ok(data: Vec<UsageData>) -> UsageResult {
        UsageResult {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    #[test]
    fn a_percent_tier_reports_what_is_left() {
        let entries = parse(&ok(vec![tier("five_hour", Some(82.0), "%")])).expect("entries");
        assert!(entries[0].value.contains("82"), "{}", entries[0].value);
        assert!(!entries[0].low);
    }

    #[test]
    fn a_percent_tier_near_empty_is_flagged_low() {
        let entries = parse(&ok(vec![tier("five_hour", Some(6.0), "%")])).expect("entries");
        assert!(entries[0].low);
    }

    /// `remaining` is what upstreams report, but not all of them: a tier with
    /// only a total and a used has to be subtracted into one.
    #[test]
    fn a_tier_without_remaining_is_derived_from_the_total() {
        let mut item = tier("weekly_limit", None, "%");
        item.total = Some(100.0);
        item.used = Some(30.0);
        let entries = parse(&ok(vec![item])).expect("entries");
        assert!(entries[0].value.contains("70"), "{}", entries[0].value);
    }

    #[test]
    fn a_balance_with_a_ceiling_is_shown_against_it() {
        let mut item = tier("", Some(12.34), "USD");
        item.total = Some(50.0);
        let entries = parse(&ok(vec![item])).expect("entries");
        assert!(entries[0].value.contains("$12.34"), "{}", entries[0].value);
        assert!(entries[0].value.contains("$50.00"), "{}", entries[0].value);
        assert!(!entries[0].low);
    }

    #[test]
    fn a_balance_without_a_ceiling_is_never_low() {
        // Nothing to be a tenth *of*, so the threshold cannot apply.
        let entries = parse(&ok(vec![tier("", Some(0.01), "USD")])).expect("entries");
        assert!(!entries[0].low);
    }

    #[test]
    fn an_unlimited_tier_needs_no_number() {
        let mut item = tier("", None, "");
        item.extra = Some(r#"{"unlimited":true}"#.to_string());
        let entries = parse(&ok(vec![item])).expect("entries");
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].low);
    }

    /// A tier the upstream sent no figure for would render as a label followed
    /// by nothing, which reads as a bug rather than as missing data.
    #[test]
    fn a_tier_with_no_figure_at_all_is_dropped() {
        assert!(parse(&ok(vec![tier("five_hour", None, "%")])).is_err());
    }

    #[test]
    fn a_result_that_returned_nothing_is_an_error_not_an_empty_line() {
        assert!(parse(&ok(vec![])).is_err());
        assert!(
            parse(&UsageResult {
                success: false,
                data: None,
                error: Some("upstream said no".to_string()),
            })
            .is_err()
        );
    }

    /// The three shapes `extra` arrives in, none of which is a contract.
    #[test]
    fn a_reset_time_is_found_in_every_extra_shape() {
        assert_eq!(
            Extra::parse(Some(r#"{"resetsAt":"2030-01-01T00:00:00Z"}"#)).resets_at,
            Some("2030-01-01T00:00:00Z".to_string())
        );
        assert_eq!(
            Extra::parse(Some("2030-01-01T00:00:00Z")).resets_at,
            Some("2030-01-01T00:00:00Z".to_string())
        );
        assert_eq!(
            Extra::parse(Some("Reset: 2030-01-01")).resets_at,
            Some("2030-01-01".to_string())
        );
        assert_eq!(Extra::parse(None).resets_at, None);
        assert_eq!(Extra::parse(Some("  ")).resets_at, None);
    }

    #[test]
    fn a_reset_time_that_is_not_a_timestamp_is_passed_through() {
        assert_eq!(reset("next monday"), Some("next monday".to_string()));
    }

    #[test]
    fn an_elapsed_reset_time_does_not_render_as_negative() {
        let text = reset("2000-01-01T00:00:00Z").expect("text");
        assert!(!text.contains('-'), "{text}");
    }
}

/// Everything the `extra` blob may carry.
///
/// It is not one shape: ZenMux sends a JSON object, an official subscription
/// sends a bare ISO timestamp, Copilot sends `Reset: <date>`, and a
/// user-written usage script sends whatever it likes. Each is read for what it
/// has and nothing is required.
#[derive(Default)]
struct Extra {
    unlimited: bool,
    resets_at: Option<String>,
    plan_label: Option<String>,
}

impl Extra {
    fn parse(raw: Option<&str>) -> Self {
        let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
            return Self::default();
        };
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text)
        {
            let string = |key: &str| {
                map.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            return Self {
                unlimited: map
                    .get("unlimited")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                resets_at: string("resetsAt").or_else(|| string("resets_at")),
                plan_label: string("planLabel"),
            };
        }
        Self {
            resets_at: Some(
                text.strip_prefix("Reset:")
                    .map(str::trim)
                    .unwrap_or(text)
                    .to_string(),
            ),
            ..Self::default()
        }
    }
}
