//! Bidirectional conversion between three LLM wire dialects:
//!
//! - **`messages`** — the block-structured dialect (`/v1/messages`): request with
//!   `system` + `messages[].content[]` blocks (`text` / `thinking` / `tool_use` /
//!   `tool_result` / `image`), SSE events `message_start` … `message_stop`.
//! - **`chat`** — the chat-completions dialect (`/v1/chat/completions`): request with
//!   flat `messages[]`, response `chat.completion`, stream `chat.completion.chunk`.
//! - **`responses`** — the item-structured event dialect (`/v1/responses`): request
//!   with `input[]` items, response `response` object, stream of typed
//!   `response.*` events. Used over both SSE and WebSocket transports.
//!
//! Supported conversion pairs (each covers request + non-stream body + event stream):
//!
//! | client speaks | upstream speaks | module |
//! |---------------|-----------------|--------|
//! | chat          | messages        | [`chat`] |
//! | responses     | messages        | [`responses`] |
//! | messages      | responses       | [`messages`] |
//!
//! Everything is *sans-io*: request converters are pure `Value -> Value` functions,
//! and stream converters are push-based state machines ([`WireEvent`] in,
//! [`Output`] out). Feed them from an SSE byte stream (via [`sse::SseParser`]) or
//! from WebSocket frames — the conversion logic is identical for both transports.

pub mod aggregate;
pub mod chat;
mod common;
pub mod messages;
pub mod responses;
pub mod signature;
pub mod sse;
#[cfg(test)]
pub(crate) mod test_fixtures;
pub mod usage;
mod util;

pub use common::MessagesRequestOptions;
pub use messages::ResponsesRequestOptions;
pub use signature::{MemorySignatureStore, SignatureStore};
pub use sse::{SseParser, WireEvent};

use serde_json::Value;

/// Conversion error.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

/// Signed `thinking` blocks captured from a finished assistant turn, keyed by the
/// turn's `tool_use` ids. Persist via [`signature::store_capture`] so a follow-up
/// request can replay them (see [`signature`] for why this round-trip exists).
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureCapture {
    /// Complete `{type:"thinking", thinking, signature}` blocks (signature non-empty).
    pub thinking_blocks: Vec<Value>,
    /// The `tool_use` ids emitted in the same turn.
    pub tool_use_ids: Vec<String>,
}

/// One item produced by a stream converter for each pushed upstream event.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A converted protocol event to deliver to the client. For the chat dialect
    /// the event name is `None` (plain `data:` lines); for the messages/responses
    /// dialects it carries the event name. Over WebSocket, send `data` as the
    /// frame payload (the event name is duplicated in the JSON `type` field).
    Event(WireEvent),
    /// Merged usage snapshot in *messages*-dialect shape (`input_tokens`,
    /// `cache_read_input_tokens`, `cache_creation_input_tokens`,
    /// `output_tokens`). Emitted whenever upstream reports usage, independent of
    /// whether the client asked for usage — use it for local accounting.
    Usage(Value),
    /// Signed thinking blocks + tool ids captured at end of turn (see
    /// [`SignatureCapture`]). At most one per stream.
    Capture(SignatureCapture),
    /// Upstream reported a terminal error; `Event` items for it (if any) have
    /// already been emitted. The payload is the upstream error value.
    Error(Value),
    /// End of stream. For a chat SSE client, emit `data: [DONE]` on this.
    Done,
}
