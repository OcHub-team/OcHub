//! Minimal incremental SSE parsing / encoding.
//!
//! [`SseParser`] accepts arbitrary byte chunks (network reads may split events —
//! and even UTF-8 sequences — anywhere) and yields complete [`WireEvent`]s. The
//! same `WireEvent` type doubles as the unit exchanged with WebSocket transports,
//! where each frame carries one event's `data` payload.

/// One wire event: optional event name + data payload (usually a JSON document).
#[derive(Debug, Clone, PartialEq)]
pub struct WireEvent {
    pub event: Option<String>,
    pub data: String,
}

impl WireEvent {
    pub fn new(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    /// Event with no name (plain `data:` SSE lines, e.g. the chat dialect).
    pub fn data_only(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// Render as an SSE block (`event:` line when named, `data:` line, blank line).
    pub fn to_sse(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + 32);
        if let Some(name) = &self.event {
            if !name.is_empty() {
                out.push_str("event: ");
                out.push_str(name);
                out.push('\n');
            }
        }
        out.push_str("data: ");
        out.push_str(&self.data);
        out.push_str("\n\n");
        out
    }
}

/// Incremental SSE parser. Feed raw bytes; complete events come out.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a chunk of bytes and return every event completed by it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<WireEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((block_end, sep_len)) = find_block_end(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..block_end + sep_len).collect();
            let text = String::from_utf8_lossy(&block[..block_end]);
            if let Some(ev) = parse_block(&text) {
                events.push(ev);
            }
        }
        events
    }

    /// Flush a trailing unterminated block (call once at end of stream).
    pub fn finish(&mut self) -> Vec<WireEvent> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&block);
        parse_block(&text).into_iter().collect()
    }
}

/// Locate the first blank-line separator (`\n\n` or `\r\n\r\n`); returns
/// (block length, separator length).
fn find_block_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

/// Parse one SSE block into an event. Multiple `data:` lines are joined with
/// `\n` per the SSE spec; comment lines (`:`) and unknown fields are ignored.
fn parse_block(text: &str) -> Option<WireEvent> {
    let mut event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    Some(WireEvent {
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut p = SseParser::new();
        let none = p.feed(b"event: message_start\ndata: {\"a\"");
        assert!(none.is_empty());
        let events = p.feed(b":1}\n\nevent: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].event.as_deref(), Some("ping"));
    }

    #[test]
    fn handles_crlf_and_data_only() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: hello\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn finish_flushes_trailing_block() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail").is_empty());
        let events = p.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    fn round_trips_to_sse() {
        let ev = WireEvent::new("message_stop", "{\"type\":\"message_stop\"}");
        assert_eq!(
            ev.to_sse(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let plain = WireEvent::data_only("{}");
        assert_eq!(plain.to_sse(), "data: {}\n\n");
    }
}
