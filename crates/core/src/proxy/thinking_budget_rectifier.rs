//! Reactive Claude thinking budget rectifier.

use crate::db::proxy_types::RectifierConfig;
use serde_json::Value;

const MAX_THINKING_BUDGET: u64 = 32_000;
const MAX_TOKENS_VALUE: u64 = 64_000;
const MIN_MAX_TOKENS_FOR_BUDGET: u64 = MAX_THINKING_BUDGET + 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetRectifySnapshot {
    pub max_tokens: Option<u64>,
    pub thinking_type: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetRectifyResult {
    pub applied: bool,
    pub before: BudgetRectifySnapshot,
    pub after: BudgetRectifySnapshot,
}

pub fn should_rectify_thinking_budget(
    error_message: Option<&str>,
    config: &RectifierConfig,
) -> bool {
    if !config.enabled || !config.request_thinking_budget {
        return false;
    }
    let Some(message) = error_message else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    let has_budget = lower.contains("budget_tokens") || lower.contains("budget tokens");
    let has_thinking = lower.contains("thinking");
    let has_1024 = lower.contains("greater than or equal to 1024")
        || lower.contains(">= 1024")
        || (lower.contains("1024") && lower.contains("input should be"));
    has_budget && has_thinking && has_1024
}

pub fn rectify_thinking_budget(body: &mut Value) -> BudgetRectifyResult {
    let before = snapshot_budget(body);
    if before.thinking_type.as_deref() == Some("adaptive") {
        return BudgetRectifyResult {
            applied: false,
            before: before.clone(),
            after: before,
        };
    }

    if !body.get("thinking").is_some_and(Value::is_object) {
        body["thinking"] = Value::Object(Default::default());
    }
    let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) else {
        return BudgetRectifyResult {
            applied: false,
            before: before.clone(),
            after: before,
        };
    };

    thinking.insert("type".to_string(), Value::String("enabled".to_string()));
    thinking.insert(
        "budget_tokens".to_string(),
        Value::Number(MAX_THINKING_BUDGET.into()),
    );
    if before.max_tokens.is_none() || before.max_tokens < Some(MIN_MAX_TOKENS_FOR_BUDGET) {
        body["max_tokens"] = Value::Number(MAX_TOKENS_VALUE.into());
    }

    let after = snapshot_budget(body);
    BudgetRectifyResult {
        applied: before != after,
        before,
        after,
    }
}

fn snapshot_budget(body: &Value) -> BudgetRectifySnapshot {
    let thinking = body.get("thinking").and_then(Value::as_object);
    BudgetRectifySnapshot {
        max_tokens: body.get("max_tokens").and_then(Value::as_u64),
        thinking_type: thinking
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        thinking_budget_tokens: thinking
            .and_then(|value| value.get("budget_tokens"))
            .and_then(Value::as_u64),
    }
}
