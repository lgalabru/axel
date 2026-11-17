//! Token usage, model and stop reason read from a response as it streams
//! past, from bounded buffers: the bytes themselves are forwarded untouched
//! by the caller and never kept.

use serde_json::Value;

use crate::analyze::clean_model;
use crate::route::Side;

/// An SSE line kept whole for parsing up to this size.  Usage events are a
/// few hundred bytes; OpenAI's `response.completed` echoes the instructions
/// and the output, so it can be far larger, and past this the line's tail
/// window is used instead.
const LINE_BYTES_MAX: usize = 256 * 1024;
/// The end of an over-long line kept for its `usage` object, which OpenAI
/// writes after the output.
const TAIL_BYTES_MAX: usize = 16 * 1024;
/// The start of an over-long line kept for its event type.
const HEAD_BYTES_MAX: usize = 512;
/// A non-streamed JSON body kept for parsing up to this size; past it, no
/// usage is read (the body still passes through whole).
const JSON_BODY_BYTES_MAX: usize = 1024 * 1024;
/// Bytes held to decide a body's shape when no content type says: leading
/// whitespace and `event:` fit many times over.
const SNIFF_BYTES_MAX: usize = 64;
/// Longest stop reason or error type kept: provider enums are short.
const CODE_BYTES_MAX: usize = 64;

/// What the response said about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) model: Option<String>,
    /// Uncached input tokens.
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_write_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) stop_reason: Option<String>,
    /// The provider's error type (`overloaded_error`), when it sent one.
    pub(crate) error_type: Option<String>,
}

/// How the body is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// `text/event-stream`, line by line.
    Stream,
    /// One JSON document.
    Json,
    /// No content type said which: the first bytes decide (ChatGPT's Codex
    /// backend streams with none).
    Sniff,
    /// Nothing worth reading (compressed, or not a model response).
    Opaque,
}

pub(crate) struct Observer {
    side: Side,
    shape: Shape,
    line: Vec<u8>,
    /// The current line outgrew [`LINE_BYTES_MAX`]: `line` holds its head,
    /// `tail` a window of its end.
    is_line_long: bool,
    tail: Vec<u8>,
    body: Vec<u8>,
    is_body_long: bool,
    /// The provider's last event arrived (`message_stop`,
    /// `response.completed`): a harness that hangs up after it has had the
    /// whole answer.
    has_final_event: bool,
    pub(crate) usage: Usage,
}

impl Observer {
    pub(crate) fn new(side: Side, shape: Shape) -> Observer {
        Observer { side, shape, line: Vec::new(), is_line_long: false, tail: Vec::new(), body: Vec::new(), is_body_long: false, has_final_event: false, usage: Usage::default() }
    }

    pub(crate) fn shape(&self) -> Shape {
        self.shape
    }

    pub(crate) fn has_final_event(&self) -> bool {
        self.has_final_event
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.shape == Shape::Sniff {
            self.feed_sniff(chunk);
            return;
        }
        match self.shape {
            Shape::Stream => self.feed_stream(chunk),
            Shape::Json => self.feed_json(chunk),
            Shape::Sniff | Shape::Opaque => {}
        }
        debug_assert!(self.line.len() <= LINE_BYTES_MAX && self.tail.len() <= 2 * TAIL_BYTES_MAX);
        debug_assert!(self.body.len() <= JSON_BODY_BYTES_MAX);
    }

    /// The first bytes, held until they say what the body is, then read as that.
    fn feed_sniff(&mut self, chunk: &[u8]) {
        let room = SNIFF_BYTES_MAX - self.body.len();
        let (head, rest) = chunk.split_at(chunk.len().min(room));
        self.body.extend_from_slice(head);
        let shape = sniff(&self.body);
        let shape = if shape == Shape::Sniff && self.body.len() == SNIFF_BYTES_MAX { Shape::Opaque } else { shape };
        if shape == Shape::Sniff {
            debug_assert!(rest.is_empty());
            return;
        }
        self.shape = shape;
        let held = std::mem::take(&mut self.body);
        self.feed(&held);
        self.feed(rest);
    }

    /// The body ended: read what is left.
    pub(crate) fn finish(&mut self) {
        match self.shape {
            Shape::Stream => self.end_line(),
            Shape::Json if !self.is_body_long => {
                if let Ok(json) = serde_json::from_slice::<Value>(&self.body) {
                    self.apply_document(&json);
                }
            }
            Shape::Json | Shape::Sniff | Shape::Opaque => {}
        }
        self.body = Vec::new();
    }

    fn feed_json(&mut self, chunk: &[u8]) {
        if self.is_body_long {
            return;
        }
        if self.body.len() + chunk.len() > JSON_BODY_BYTES_MAX {
            self.is_body_long = true;
            self.body = Vec::new();
            return;
        }
        self.body.extend_from_slice(chunk);
    }

    fn feed_stream(&mut self, mut chunk: &[u8]) {
        while let Some(newline) = chunk.iter().position(|b| *b == b'\n') {
            self.push_line_bytes(&chunk[..newline]);
            self.end_line();
            chunk = &chunk[newline + 1..];
        }
        self.push_line_bytes(chunk);
    }

    fn push_line_bytes(&mut self, bytes: &[u8]) {
        if !self.is_line_long && self.line.len() + bytes.len() <= LINE_BYTES_MAX {
            self.line.extend_from_slice(bytes);
            return;
        }
        if !self.is_line_long {
            self.is_line_long = true;
            self.tail.extend_from_slice(&self.line[self.line.len().saturating_sub(TAIL_BYTES_MAX)..]);
            self.line.truncate(HEAD_BYTES_MAX);
        }
        let keep = &bytes[bytes.len().saturating_sub(TAIL_BYTES_MAX)..];
        self.tail.extend_from_slice(keep);
        if self.tail.len() > 2 * TAIL_BYTES_MAX {
            self.tail.drain(..self.tail.len() - TAIL_BYTES_MAX);
        }
    }

    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        if self.is_line_long {
            let tail = std::mem::take(&mut self.tail);
            self.is_line_long = false;
            self.apply_long_line(&line, &tail);
            return;
        }
        let Some(payload) = line.strip_prefix(b"data:") else { return };
        let payload = payload.trim_ascii();
        if payload.is_empty() || payload == b"[DONE]" {
            return;
        }
        if let Ok(event) = serde_json::from_slice::<Value>(payload) {
            self.apply_event(&event);
        }
    }

    /// An event too long to parse whole: its type from the head, its usage
    /// object from the tail.
    fn apply_long_line(&mut self, head: &[u8], tail: &[u8]) {
        let is_final = [b"response.completed".as_slice(), b"response.incomplete", b"response.failed"]
            .iter()
            .any(|name| find(head, name).is_some());
        if !is_final || self.side != Side::OpenAi {
            return;
        }
        self.has_final_event = true;
        if let Some(usage) = last_object_after(tail, b"\"usage\":") {
            apply_openai_usage(&mut self.usage, &usage);
        }
    }

    fn apply_event(&mut self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match (self.side, kind) {
            (Side::Anthropic, "message_start") => {
                if let Some(message) = event.get("message") {
                    self.apply_anthropic_message(message);
                }
            }
            (Side::Anthropic, "message_delta") => {
                if let Some(usage) = event.get("usage") {
                    apply_anthropic_usage(&mut self.usage, usage);
                }
                set_code(&mut self.usage.stop_reason, event.pointer("/delta/stop_reason"));
            }
            (Side::Anthropic, "message_stop") => self.has_final_event = true,
            (Side::Anthropic, "error") => set_code(&mut self.usage.error_type, event.pointer("/error/type")),
            (Side::OpenAi, "response.created" | "response.in_progress") => self.set_model(event.pointer("/response/model")),
            (Side::OpenAi, "response.completed" | "response.incomplete" | "response.failed") => {
                self.has_final_event = true;
                if let Some(response) = event.get("response") {
                    self.apply_openai_response(response);
                }
            }
            (Side::OpenAi, "error") => set_code(&mut self.usage.error_type, event.get("code").or_else(|| event.pointer("/error/code"))),
            _ => {}
        }
    }

    fn apply_document(&mut self, json: &Value) {
        if let Some(error) = json.get("error") {
            set_code(&mut self.usage.error_type, error.get("type").or_else(|| error.get("code")));
        }
        match self.side {
            Side::Anthropic => self.apply_anthropic_message(json),
            Side::OpenAi => self.apply_openai_response(json),
        }
    }

    fn apply_anthropic_message(&mut self, message: &Value) {
        self.set_model(message.get("model"));
        if let Some(usage) = message.get("usage") {
            apply_anthropic_usage(&mut self.usage, usage);
        }
        set_code(&mut self.usage.stop_reason, message.get("stop_reason"));
    }

    fn apply_openai_response(&mut self, response: &Value) {
        self.set_model(response.get("model"));
        if let Some(usage) = response.get("usage") {
            apply_openai_usage(&mut self.usage, usage);
        }
        let reason = response.pointer("/incomplete_details/reason").filter(|v| !v.is_null()).or_else(|| response.get("status"));
        set_code(&mut self.usage.stop_reason, reason);
        set_code(&mut self.usage.error_type, response.pointer("/error/code"));
    }

    fn set_model(&mut self, model: Option<&Value>) {
        if let Some(model) = model.and_then(Value::as_str).and_then(clean_model) {
            self.usage.model = Some(model);
        }
    }
}

/// Anthropic counts: `input_tokens` is already the uncached part.  A
/// `message_delta` repeats the counts so far, so later values replace.
fn apply_anthropic_usage(usage: &mut Usage, json: &Value) {
    let count = |key: &str| json.get(key).and_then(Value::as_u64);
    for (slot, value) in [
        (&mut usage.input_tokens, count("input_tokens")),
        (&mut usage.output_tokens, count("output_tokens")),
        (&mut usage.cache_read_tokens, count("cache_read_input_tokens")),
        (&mut usage.cache_write_tokens, count("cache_creation_input_tokens")),
    ] {
        if value.is_some() {
            *slot = value;
        }
    }
}

/// OpenAI counts: `input_tokens` includes the cached ones, which come out.
fn apply_openai_usage(usage: &mut Usage, json: &Value) {
    let input = json.get("input_tokens").and_then(Value::as_u64);
    let cached = json.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64);
    usage.input_tokens = input.map(|i| i.saturating_sub(cached.unwrap_or(0)));
    usage.cache_read_tokens = cached;
    usage.output_tokens = json.get("output_tokens").and_then(Value::as_u64);
    usage.reasoning_tokens = json.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64);
}

/// A provider's code (`end_turn`, `max_output_tokens`), kept only if it looks
/// like one, so no free text can pass for it.
fn set_code(slot: &mut Option<String>, value: Option<&Value>) {
    let Some(code) = value.and_then(Value::as_str) else { return };
    let is_code = !code.is_empty() && code.len() <= CODE_BYTES_MAX && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    if is_code {
        *slot = Some(code.to_string());
    }
}

/// A body's shape from its first bytes: a JSON document opens with `{`, a
/// stream with an SSE field (`event:`, `data:`, or a `:` comment).
/// `Sniff` while the bytes so far could still start either.
fn sniff(first: &[u8]) -> Shape {
    let start = first.trim_ascii_start();
    let fields: [&[u8]; 3] = [b"event:", b"data:", b":"];
    if start.starts_with(b"{") {
        Shape::Json
    } else if fields.iter().any(|f| start.starts_with(f)) {
        Shape::Stream
    } else if fields.iter().any(|f| f.starts_with(start)) {
        // Empty, or a field name cut short: wait for more.
        Shape::Sniff
    } else {
        Shape::Opaque
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The JSON object right after the last `key` in `bytes`, if it parses.
fn last_object_after(bytes: &[u8], key: &[u8]) -> Option<Value> {
    let at = bytes.windows(key.len()).rposition(|w| w == key)?;
    let rest = &bytes[at + key.len()..];
    let mut values = serde_json::Deserializer::from_slice(rest).into_iter::<Value>();
    values.next()?.ok().filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_in_pieces(o: &mut Observer, bytes: &[u8], piece: usize) {
        for chunk in bytes.chunks(piece) {
            o.feed(chunk);
        }
        o.finish();
    }

    const ANTHROPIC_SSE: &str = "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":3,\"cache_creation_input_tokens\":120,\"cache_read_input_tokens\":26000,\"output_tokens\":1}}}\r\n\r\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n";

    #[test]
    fn anthropic_usage_comes_from_message_start_and_delta_whatever_the_chunking() {
        for piece in [1, 7, 4096] {
            let mut o = Observer::new(Side::Anthropic, Shape::Stream);
            feed_in_pieces(&mut o, ANTHROPIC_SSE.as_bytes(), piece);
            let u = &o.usage;
            assert_eq!(u.model.as_deref(), Some("claude-opus-5-5"));
            assert_eq!((u.input_tokens, u.cache_write_tokens, u.cache_read_tokens, u.output_tokens), (Some(3), Some(120), Some(26_000), Some(5)));
            assert_eq!(u.stop_reason.as_deref(), Some("end_turn"));
        }
    }

    #[test]
    fn openai_usage_comes_from_response_completed_even_when_the_line_is_huge() {
        let output = "x".repeat(LINE_BYTES_MAX * 2);
        let completed = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"model\":\"gpt-5.5\",\"instructions\":\"{output}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":1000,\"input_tokens_details\":{{\"cached_tokens\":800}},\"output_tokens\":50,\"output_tokens_details\":{{\"reasoning_tokens\":20}},\"total_tokens\":1050}}}}}}\n\n"
        );
        for body in [completed.replace(&output, "short"), completed] {
            let mut o = Observer::new(Side::OpenAi, Shape::Stream);
            feed_in_pieces(&mut o, format!("data: {{\"type\":\"response.created\",\"response\":{{\"model\":\"gpt-5.5\"}}}}\n\n{body}").as_bytes(), 8192);
            let u = &o.usage;
            assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens, u.reasoning_tokens), (Some(200), Some(800), Some(50), Some(20)));
            assert_eq!(u.model.as_deref(), Some("gpt-5.5"));
            assert!(o.line.len() <= LINE_BYTES_MAX && o.tail.is_empty());
        }
    }

    #[test]
    fn a_json_body_is_read_and_free_text_never_passes_for_a_code() {
        let mut o = Observer::new(Side::Anthropic, Shape::Json);
        feed_in_pieces(&mut o, br#"{"model":"claude-haiku-5","stop_reason":"max_tokens","usage":{"input_tokens":9,"output_tokens":2}}"#, 5);
        assert_eq!((o.usage.input_tokens, o.usage.output_tokens, o.usage.stop_reason.as_deref()), (Some(9), Some(2), Some("max_tokens")));
        let mut o = Observer::new(Side::Anthropic, Shape::Json);
        feed_in_pieces(&mut o, br#"{"type":"error","error":{"type":"the prompt said: secret stuff","message":"m"}}"#, 64);
        assert_eq!(o.usage.error_type, None);
        let mut o = Observer::new(Side::Anthropic, Shape::Json);
        feed_in_pieces(&mut o, br#"{"type":"error","error":{"type":"overloaded_error","message":"m"}}"#, 64);
        assert_eq!(o.usage.error_type.as_deref(), Some("overloaded_error"));
    }

    #[test]
    fn a_body_without_a_content_type_is_sniffed() {
        let mut o = Observer::new(Side::Anthropic, Shape::Sniff);
        o.feed(b"");
        assert_eq!(o.shape(), Shape::Sniff);
        feed_in_pieces(&mut o, ANTHROPIC_SSE.as_bytes(), 3);
        assert_eq!((o.shape(), o.usage.output_tokens, o.has_final_event()), (Shape::Stream, Some(5), false));
        o.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n");
        assert!(o.has_final_event());
        let mut o = Observer::new(Side::OpenAi, Shape::Sniff);
        feed_in_pieces(&mut o, br#" {"model":"gpt-5.5","usage":{"input_tokens":3,"output_tokens":1}}"#, 4);
        assert_eq!((o.shape(), o.usage.input_tokens), (Shape::Json, Some(3)));
        let mut o = Observer::new(Side::OpenAi, Shape::Sniff);
        o.feed(b"\x28\xb5\x2f\xfd");
        assert_eq!(o.shape(), Shape::Opaque);
    }

    #[test]
    fn a_json_body_past_its_limit_is_not_kept() {
        let mut o = Observer::new(Side::OpenAi, Shape::Json);
        o.feed(&vec![b' '; JSON_BODY_BYTES_MAX]);
        o.feed(b"{}");
        assert!(o.is_body_long && o.body.is_empty());
        o.finish();
        assert_eq!(o.usage, Usage::default());
    }
}
