//! Request-side helpers shared by the chat→messages and responses→messages
//! converters: content/block mapping, tool & tool_choice conversion, prompt-cache
//! breakpoints, thinking-budget resolution, and final messages-request assembly.

use serde_json::{Map, Value, json};

/// Options for building a messages-dialect request.
#[derive(Debug, Clone)]
pub struct MessagesRequestOptions {
    /// The messages dialect requires `max_tokens`; chat/responses clients may omit
    /// it. Default cap applied when absent (upstream still clamps to the model).
    pub default_max_tokens: i64,
    /// Inject ephemeral prompt-cache breakpoints on the stable prefix (last
    /// system block + first / last-two user turns). Chat/responses clients never
    /// send cache markers themselves, so without this no prompt caching happens.
    pub inject_cache_breakpoints: bool,
    /// Thinking budget (tokens) used when a client enables thinking without
    /// specifying a budget.
    pub default_thinking_budget: i64,
}

impl Default for MessagesRequestOptions {
    fn default() -> Self {
        Self {
            default_max_tokens: 32000,
            inject_cache_breakpoints: true,
            default_thinking_budget: 10000,
        }
    }
}

/// Append `blocks` to the trailing message when it shares `role`, otherwise start
/// a new message. This merges consecutive tool results (and same-role turns) into
/// a single messages-dialect message, which that dialect expects for
/// `tool_result` blocks.
pub(crate) fn push_message(messages: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        arr.append(&mut blocks);
        return;
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// Flatten a `content` value (string | array of parts) to a single plain string.
/// Used for tool-result bodies, which the messages dialect carries as plain text.
pub(crate) fn content_to_plain_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                } else if let Some(s) = p.as_str() {
                    out.push_str(s);
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// Build a base64 image block from a data-URI image URL. Non-data URLs return
/// `None` and are skipped (remote-URL sources are not modeled downstream).
pub(crate) fn image_block_from_url(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    // data:<media_type>;base64,<data>
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.split(';').next().unwrap_or("image/png");
    Some(json!({
        "type": "image",
        "source": { "type": "base64", "media_type": media_type, "data": data }
    }))
}

/// Convert chat `content` (string | array of `{type: text|image_url}`) to
/// messages content blocks.
pub(crate) fn chat_content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("image_url") => {
                        let url = match p.get("image_url") {
                            Some(Value::Object(m)) => m.get("url").and_then(Value::as_str),
                            Some(Value::String(s)) => Some(s.as_str()),
                            _ => None,
                        };
                        if let Some(block) = url.and_then(image_block_from_url) {
                            blocks.push(block);
                        }
                    }
                    _ => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        } else if let Some(t) = p.get("refusal").and_then(Value::as_str) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        }
                    }
                }
            }
            blocks
        }
        _ => vec![],
    }
}

/// Collect text parts (string | array) into messages `system` text blocks.
pub(crate) fn content_to_system_blocks(content: Option<&Value>) -> Vec<Value> {
    let text = content_to_plain_text(content);
    if text.is_empty() {
        vec![]
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

/// chat tool `{type:function, function:{name, description, parameters}}` →
/// messages tool `{name, description, input_schema}`.
pub(crate) fn convert_chat_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            tool_from_parts(
                f.get("name").and_then(Value::as_str),
                f.get("description").and_then(Value::as_str),
                f.get("parameters"),
            )
        })
        .collect()
}

/// responses tool `{type:function, name, description, parameters}` (name at top
/// level) → messages tool. Only plain function tools translate; built-in
/// server-side tools have no messages equivalent and are dropped.
pub(crate) fn convert_responses_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            if t.get("type").and_then(Value::as_str) != Some("function") {
                return None;
            }
            tool_from_parts(
                t.get("name").and_then(Value::as_str),
                t.get("description").and_then(Value::as_str),
                t.get("parameters"),
            )
        })
        .collect()
}

fn tool_from_parts(
    name: Option<&str>,
    description: Option<&str>,
    parameters: Option<&Value>,
) -> Option<Value> {
    let name = name.filter(|n| !n.trim().is_empty())?;
    let mut tool = Map::new();
    tool.insert("name".into(), json!(name));
    if let Some(d) = description {
        tool.insert("description".into(), json!(d));
    }
    let schema = parameters
        .cloned()
        .filter(|p| p.as_object().map(|o| !o.is_empty()).unwrap_or(false))
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    tool.insert("input_schema".into(), schema);
    Some(Value::Object(tool))
}

/// Map a chat/responses `tool_choice` to the messages form.
pub(crate) fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            // `none` must be explicit: omitting tool_choice while tools are
            // present defaults to `auto` upstream, which would let the model call
            // tools the client forbade.
            "none" => Some(json!({ "type": "none" })),
            _ => None,
        },
        Value::Object(_) => {
            // chat: {type:function, function:{name}}; responses: {type:function, name}.
            let name = tc
                .pointer("/function/name")
                .or_else(|| tc.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({ "type": "tool", "name": name }))
        }
        _ => None,
    }
}

/// Tag a content block with an ephemeral cache breakpoint.
fn set_cache_control(block: &mut Value) {
    if let Some(obj) = block.as_object_mut() {
        obj.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }
}

/// Inject prompt-cache breakpoints on the stable prefix:
/// - the last system block (caches tools + system), and
/// - the first user turn (a fixed deep anchor whose marker never moves), and
/// - the last block of the **last two** user turns.
///
/// Marking the last *two* user turns (not just the newest) is what makes
/// incremental multi-turn caching actually read instead of re-create: the
/// upstream only reads a previously cached prefix whose breakpoint marker is
/// still present, so each request must keep the prior turn's breakpoint in
/// addition to adding its own. Consecutive requests then overlap on a shared
/// breakpoint and read each other's cache. Total breakpoints stay within the
/// upstream limit of 4 (≤1 system + ≤3 user). Blocks below the minimum cacheable
/// size are ignored upstream, so this is always safe.
fn inject_cache_control(system: &mut [Value], messages: &mut [Value]) {
    if let Some(last_system) = system.last_mut() {
        set_cache_control(last_system);
    }
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect();
    let mut targets: Vec<usize> = Vec::new();
    if let Some(&first) = user_indices.first() {
        targets.push(first);
    }
    for &idx in user_indices.iter().rev().take(2) {
        if !targets.contains(&idx) {
            targets.push(idx);
        }
    }
    for idx in targets {
        if let Some(last_block) = messages[idx]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|c| c.last_mut())
        {
            set_cache_control(last_block);
        }
    }
}

/// Map a reasoning-effort string to a thinking budget (tokens), or `None` when
/// thinking should stay off (`minimal`/`none`/absent).
fn effort_to_thinking_budget(effort: Option<&str>) -> Option<i64> {
    match effort {
        Some("low") => Some(4096),
        Some("medium") => Some(10000),
        Some("high") | Some("xhigh") => Some(16000),
        _ => None,
    }
}

/// Resolve the extended-thinking budget from a request body. Honors both a
/// reasoning-effort string and a raw messages-style `thinking` object passed
/// through the chat/responses endpoint (some clients do this). An explicit
/// `{"type":"disabled"}` turns it off.
pub(crate) fn resolve_thinking_budget(
    obj: &Map<String, Value>,
    effort: Option<&str>,
    opts: &MessagesRequestOptions,
) -> Option<i64> {
    if let Some(thinking) = obj.get("thinking") {
        let ty = thinking.get("type").and_then(Value::as_str);
        if ty == Some("disabled") {
            return None;
        }
        if ty == Some("enabled") || thinking.as_bool() == Some(true) {
            let budget = thinking
                .get("budget_tokens")
                .and_then(Value::as_i64)
                .filter(|b| *b >= 1024)
                .unwrap_or(opts.default_thinking_budget);
            return Some(budget);
        }
    }
    effort_to_thinking_budget(effort)
}

/// Assemble the final messages-dialect request from converted parts.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble(
    model: &str,
    system: Vec<Value>,
    messages: Vec<Value>,
    max_tokens: i64,
    temperature: Option<f64>,
    stream: Option<bool>,
    mut tools: Vec<Value>,
    mut tool_choice: Option<Value>,
    thinking_budget: Option<i64>,
    opts: &MessagesRequestOptions,
) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    // Clamp into i32 range so downstream typed parsers never fail on an
    // out-of-range client value. With thinking enabled the upstream also
    // requires max_tokens > budget_tokens.
    let mut max_tokens = max_tokens.clamp(1, i32::MAX as i64);
    if let Some(budget) = thinking_budget
        && max_tokens <= budget
    {
        max_tokens = (budget + 4096).min(i32::MAX as i64);
    }
    out.insert("max_tokens".into(), json!(max_tokens));
    // The messages dialect requires the first message to use the `user` role.
    // chat/responses accept inputs that begin with an assistant / tool turn
    // (few-shot priming, replaying a tool-call tail); prepend a minimal user
    // turn so those don't 400.
    let mut messages = messages;
    if messages
        .first()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        != Some("user")
    {
        messages.insert(
            0,
            json!({ "role": "user", "content": [{ "type": "text", "text": "(continue)" }] }),
        );
    }
    let mut system = system;
    if opts.inject_cache_breakpoints {
        inject_cache_control(&mut system, &mut messages);
    }
    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    out.insert("messages".into(), Value::Array(messages));
    // With thinking enabled the upstream requires temperature unset (defaults to
    // 1), so only forward the client's temperature when thinking is off.
    if thinking_budget.is_none()
        && let Some(t) = temperature
    {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(s) = stream {
        out.insert("stream".into(), json!(s));
    }
    // Every custom tool requires a non-empty name; nameless entries (e.g.
    // untranslatable server-side tools) must not leak through.
    tools.retain(|tool| {
        tool.get("name")
            .and_then(Value::as_str)
            .map(|name| !name.trim().is_empty())
            .unwrap_or(false)
    });
    if tools.is_empty() {
        tool_choice = None;
    } else if let Some(choice) = tool_choice.as_mut()
        && choice.get("type").and_then(Value::as_str) == Some("tool")
    {
        let selected_name = choice.get("name").and_then(Value::as_str);
        let selected_exists = selected_name
            .map(|selected| {
                tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(selected))
            })
            .unwrap_or(false);
        if !selected_exists {
            *choice = json!({ "type": "auto" });
        }
    }
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tc) = tool_choice {
        // Forced tool use (`any`/`tool`) is incompatible with extended thinking
        // upstream; relax to auto.
        let tc = if thinking_budget.is_some()
            && matches!(
                tc.get("type").and_then(Value::as_str),
                Some("any") | Some("tool")
            ) {
            json!({ "type": "auto" })
        } else {
            tc
        };
        out.insert("tool_choice".into(), tc);
    }
    if let Some(budget) = thinking_budget {
        out.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": budget }),
        );
    }
    Value::Object(out)
}
