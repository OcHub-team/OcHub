//! Lightweight per-line syntax highlighting for the config code editor.
//!
//! Hand-rolled tokenizers (no tree-sitter): config files are small and the
//! grammar subset we need — keys, strings, numbers, keywords, comments,
//! punctuation — is tiny. Each tokenizer maps one line to contiguous spans
//! covering every byte, which `code_editor` turns into shaped `TextRun`s.
//! Tokenizers are stateless across lines; multi-line strings degrade to
//! plain text, which is acceptable for provider config files.

use crate::theme;

/// Highlighting language, mirroring `provider_config::Language`.
/// `Plain` is the no-highlight fallback for hosts editing arbitrary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Lang {
    Json,
    Toml,
    Yaml,
    Env,
    Plain,
}

impl Lang {
    pub fn from_core(lang: ochub_core::provider_config::Language) -> Self {
        use ochub_core::provider_config::Language as L;
        match lang {
            L::Json => Lang::Json,
            L::Toml => Lang::Toml,
            L::Yaml => Lang::Yaml,
            L::Env => Lang::Env,
        }
    }
}

/// Token classes; each maps to one theme color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Plain,
    Key,
    Str,
    Num,
    Keyword,
    Comment,
    Punct,
    Section,
}

impl Token {
    pub fn color(self) -> gpui::Rgba {
        match self {
            Token::Plain => theme::text(),
            Token::Key => theme::accent(),
            Token::Str => theme::green(),
            Token::Num => theme::peach(),
            Token::Keyword => theme::mauve(),
            Token::Comment => theme::muted(),
            Token::Punct => theme::subtext(),
            Token::Section => theme::mauve(),
        }
    }
}

/// A run of `len` bytes with one token class. Spans are contiguous and cover
/// the entire line.
pub type Span = (usize, Token);

/// Tokenize one line. The returned spans always sum to `line.len()`.
pub fn line_spans(lang: Lang, line: &str) -> Vec<Span> {
    let spans = match lang {
        Lang::Json => json_line(line),
        Lang::Toml => toml_line(line),
        Lang::Yaml => yaml_line(line),
        Lang::Env => env_line(line),
        Lang::Plain => vec![(line.len(), Token::Plain)],
    };
    debug_assert_eq!(spans.iter().map(|s| s.0).sum::<usize>(), line.len());
    spans
}

/// Span accumulator that merges adjacent same-token runs and guarantees full
/// coverage of the line.
struct Acc {
    spans: Vec<Span>,
}

impl Acc {
    fn new() -> Self {
        Self { spans: Vec::new() }
    }

    fn push(&mut self, len: usize, token: Token) {
        if len == 0 {
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            if last.1 == token {
                last.0 += len;
                return;
            }
        }
        self.spans.push((len, token));
    }
}

/// Consume a quoted string starting at `bytes[i]` (which is the quote char);
/// returns the byte length including both quotes (or to end of line).
fn quoted_len(bytes: &[u8], i: usize, quote: u8) -> usize {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b if b == quote => return j + 1 - i,
            _ => j += 1,
        }
    }
    bytes.len() - i
}

/// Length of a number-ish token ([0-9eE+-._] after a digit/minus start).
fn number_len(bytes: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < bytes.len() {
        match bytes[j] {
            b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' | b'_' | b':' | b'T' | b'Z' => j += 1,
            _ => break,
        }
    }
    j - i
}

/// Length of a bare identifier/keyword run.
fn word_len(bytes: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < bytes.len() {
        match bytes[j] {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => j += 1,
            _ => break,
        }
    }
    (j - i).max(1)
}

/// Whether the next non-whitespace byte after `i` is `needle`.
fn next_nonspace_is(bytes: &[u8], mut i: usize, needle: u8) -> bool {
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' => i += 1,
            b => return b == needle,
        }
    }
    false
}

fn json_line(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let mut acc = Acc::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let len = quoted_len(bytes, i, b'"');
                // A string followed by `:` is an object key.
                let token = if next_nonspace_is(bytes, i + len, b':') {
                    Token::Key
                } else {
                    Token::Str
                };
                acc.push(len, token);
                i += len;
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => {
                acc.push(1, Token::Punct);
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let len = number_len(bytes, i).max(1);
                acc.push(len, Token::Num);
                i += len;
            }
            b'a'..=b'z' => {
                let len = word_len(bytes, i);
                let word = &line[i..i + len];
                let token = if matches!(word, "true" | "false" | "null") {
                    Token::Keyword
                } else {
                    Token::Plain
                };
                acc.push(len, token);
                i += len;
            }
            _ => {
                acc.push(1, Token::Plain);
                i += 1;
            }
        }
    }
    if acc.spans.is_empty() {
        acc.push(bytes.len(), Token::Plain);
    }
    acc.spans
}

/// Tokenize the value portion shared by TOML / YAML / env lines.
fn value_tokens(acc: &mut Acc, line: &str, start: usize, allow_comment: bool) {
    let bytes = line.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'#' if allow_comment => {
                acc.push(bytes.len() - i, Token::Comment);
                return;
            }
            b'"' => {
                let len = quoted_len(bytes, i, b'"');
                acc.push(len, Token::Str);
                i += len;
            }
            b'\'' => {
                let len = quoted_len(bytes, i, b'\'');
                acc.push(len, Token::Str);
                i += len;
            }
            b'{' | b'}' | b'[' | b']' | b',' | b'=' | b':' => {
                acc.push(1, Token::Punct);
                i += 1;
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let len = number_len(bytes, i).max(1);
                acc.push(len, Token::Num);
                i += len;
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                let len = word_len(bytes, i);
                let word = &line[i..i + len];
                let token = if matches!(
                    word,
                    "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "inf" | "nan"
                ) {
                    Token::Keyword
                } else {
                    Token::Plain
                };
                acc.push(len, token);
                i += len;
            }
            _ => {
                acc.push(1, Token::Plain);
                i += 1;
            }
        }
    }
}

fn toml_line(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let mut acc = Acc::new();
    acc.push(indent, Token::Plain);

    if trimmed.starts_with('#') {
        acc.push(trimmed.len(), Token::Comment);
        return acc.spans;
    }
    if trimmed.starts_with('[') {
        // [table] / [[array-of-tables]] header; trailing comment allowed.
        let end = line.rfind(']').map(|p| p + 1).unwrap_or(line.len());
        acc.push(end - indent, Token::Section);
        value_tokens(&mut acc, line, end, true);
        return acc.spans;
    }
    // key = value — the key may be dotted or quoted; color up to the first
    // unquoted `=`.
    let mut i = indent;
    let mut eq = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i += quoted_len(bytes, i, b'"'),
            b'\'' => i += quoted_len(bytes, i, b'\''),
            b'=' => {
                eq = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    match eq {
        Some(eq) => {
            acc.push(eq - indent, Token::Key);
            acc.push(1, Token::Punct);
            value_tokens(&mut acc, line, eq + 1, true);
        }
        None => value_tokens(&mut acc, line, indent, true),
    }
    acc.spans
}

fn yaml_line(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let mut acc = Acc::new();
    acc.push(indent, Token::Plain);

    if trimmed.starts_with('#') {
        acc.push(trimmed.len(), Token::Comment);
        return acc.spans;
    }
    let mut i = indent;
    // Leading list dash(es).
    while i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b' ' {
        acc.push(2, Token::Punct);
        i += 2;
    }
    // `key:` (followed by space or EOL) → Key. Scan for the first `:` outside
    // quotes that terminates a key.
    let mut j = i;
    let mut colon = None;
    while j < bytes.len() {
        match bytes[j] {
            b'"' => j += quoted_len(bytes, j, b'"'),
            b'\'' => j += quoted_len(bytes, j, b'\''),
            b':' if j + 1 >= bytes.len() || bytes[j + 1] == b' ' => {
                colon = Some(j);
                break;
            }
            b'#' => break,
            _ => j += 1,
        }
    }
    match colon {
        Some(colon) => {
            acc.push(colon - i, Token::Key);
            acc.push(1, Token::Punct);
            value_tokens(&mut acc, line, colon + 1, true);
        }
        None => value_tokens(&mut acc, line, i, true),
    }
    acc.spans
}

fn env_line(line: &str) -> Vec<Span> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let mut acc = Acc::new();
    acc.push(indent, Token::Plain);

    if trimmed.starts_with('#') {
        acc.push(trimmed.len(), Token::Comment);
        return acc.spans;
    }
    match line[indent..].find('=') {
        Some(rel) => {
            acc.push(rel, Token::Key);
            acc.push(1, Token::Punct);
            // Everything after `=` is the raw value.
            let rest = line.len() - indent - rel - 1;
            let value = &line[indent + rel + 1..];
            let token = if value.starts_with('"') || value.starts_with('\'') {
                Token::Str
            } else {
                Token::Str
            };
            acc.push(rest, token);
        }
        None => acc.push(line.len() - indent, Token::Plain),
    }
    acc.spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(spans: &[Span]) -> usize {
        spans.iter().map(|s| s.0).sum()
    }

    #[test]
    fn spans_cover_every_byte() {
        let samples: &[(Lang, &str)] = &[
            (Lang::Json, r#"  "env": { "KEY": "value", "n": 42 },"#),
            (Lang::Json, r#"{"a":true,"b":null,"c":[1,2.5,-3e4]}"#),
            (Lang::Json, ""),
            (Lang::Toml, r#"model = "gpt-5"  # comment"#),
            (Lang::Toml, "[model_providers.custom]"),
            (Lang::Toml, "# full comment"),
            (Lang::Yaml, "  - name: hello  # tail"),
            (Lang::Yaml, "custom_providers:"),
            (Lang::Env, "GEMINI_API_KEY=abc123"),
            (Lang::Env, "# comment"),
            (Lang::Plain, "anything at all"),
        ];
        for (lang, line) in samples {
            let spans = line_spans(*lang, line);
            assert_eq!(total(&spans), line.len(), "coverage for {lang:?}: {line}");
        }
    }

    #[test]
    fn json_keys_vs_strings() {
        let spans = line_spans(Lang::Json, r#""key": "value""#);
        assert_eq!(spans[0].1, Token::Key);
        assert!(spans.iter().any(|s| s.1 == Token::Str));
    }

    #[test]
    fn toml_sections_and_comments() {
        let spans = line_spans(Lang::Toml, "[table.sub]");
        assert_eq!(spans[0].1, Token::Section);
        let spans = line_spans(Lang::Toml, "# note");
        assert_eq!(spans[0].1, Token::Comment);
    }

    #[test]
    fn multibyte_utf8_stays_covered() {
        let line = r#"  "名称": "供应商 🦀""#;
        let spans = line_spans(Lang::Json, line);
        assert_eq!(total(&spans), line.len());
    }
}
