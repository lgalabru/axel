//! What a request's context is made of: the body parsed in memory, measured
//! by kind, and dropped.  Only sizes, counts and tool names come out.

use std::collections::HashMap;

use axel_store::{CONTEXT_PARTS_MAX, ContextBreakdown, ContextKind, ContextPart};
use serde_json::Value;

use crate::route::{Endpoint, Side};

/// Longest tool name kept as a label; the store refuses longer.
const LABEL_BYTES_MAX: usize = 120;
/// Longest model name kept.
pub(crate) const MODEL_BYTES_MAX: usize = 120;
/// Tool call ids remembered to name the tool a result answers: far more
/// calls than one context holds.
const CALL_IDS_MAX: usize = 16_384;

/// What the request body says.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Analysis {
    pub(crate) model: Option<String>,
    pub(crate) breakdown: Option<ContextBreakdown>,
}

/// Break `body` down, if it is JSON this endpoint's provider speaks.  A body
/// that isn't (compressed, or not JSON) gives nothing, never an error: the
/// request is forwarded either way.
pub(crate) fn analyze(endpoint: Endpoint, body: &[u8]) -> Analysis {
    if !endpoint.has_context() || body.is_empty() {
        return Analysis::default();
    }
    let Ok(json) = serde_json::from_slice::<Value>(body) else { return Analysis::default() };
    let Some(object) = json.as_object() else { return Analysis::default() };
    let model = object.get("model").and_then(Value::as_str).and_then(clean_model);
    let mut parts = Parts::default();
    let (messages, tools) = match endpoint.side() {
        Side::Anthropic => anthropic(object, &mut parts),
        Side::OpenAi => openai(object, &mut parts),
    };
    let breakdown = ContextBreakdown { parts: parts.finish(), messages, tools };
    debug_assert!(breakdown.parts.len() <= CONTEXT_PARTS_MAX);
    Analysis { model, breakdown: Some(breakdown) }
}

/// A model name as a harness or provider gives it, if it looks like one.
pub(crate) fn clean_model(model: &str) -> Option<String> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '[' | ']');
    let fits = !model.is_empty() && model.len() <= MODEL_BYTES_MAX;
    (fits && model.chars().all(is_name)).then(|| model.to_string())
}

/// Messages API: `system`, `tools`, `messages`.
fn anthropic(object: &serde_json::Map<String, Value>, parts: &mut Parts) -> (u32, u32) {
    if let Some(system) = object.get("system") {
        let count = system.as_array().map_or(1, Vec::len);
        parts.add_many(ContextKind::System, "", json_bytes(system), count);
    }
    let tools = tool_definitions(object.get("tools"), parts);
    let messages = object.get("messages").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut calls = CallNames::default();
    for message in messages {
        let role_kind = match message.get("role").and_then(Value::as_str) {
            Some("assistant") => ContextKind::AssistantText,
            _ => ContextKind::UserText,
        };
        match message.get("content") {
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    anthropic_block(block, role_kind, &mut calls, parts);
                }
            }
            Some(other) => parts.add(role_kind, "", json_bytes(other)),
            None => {}
        }
    }
    (saturating_u32(messages.len()), tools)
}

fn anthropic_block(block: &Value, role_kind: ContextKind, calls: &mut CallNames, parts: &mut Parts) {
    let bytes = json_bytes(block);
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => parts.add(role_kind, "", bytes),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            calls.remember(block.get("id"), name);
            parts.add(ContextKind::ToolCall, name, bytes);
        }
        "image" => parts.add(ContextKind::Image, "", bytes),
        "thinking" | "redacted_thinking" => parts.add(ContextKind::Reasoning, "", bytes),
        kind if kind == "tool_result" || kind.ends_with("_tool_result") => {
            parts.add(ContextKind::ToolResult, calls.name(block.get("tool_use_id")), bytes);
        }
        _ => parts.add(ContextKind::Other, "", bytes),
    }
}

/// Responses API: `instructions`, `tools`, `input`.
fn openai(object: &serde_json::Map<String, Value>, parts: &mut Parts) -> (u32, u32) {
    if let Some(instructions) = object.get("instructions").filter(|v| !v.is_null()) {
        parts.add(ContextKind::System, "", json_bytes(instructions));
    }
    let tools = tool_definitions(object.get("tools"), parts);
    let items = match object.get("input") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(text @ Value::String(_)) => {
            parts.add(ContextKind::UserText, "", json_bytes(text));
            return (1, tools);
        }
        _ => &[][..],
    };
    let mut calls = CallNames::default();
    let mut tools = tools;
    for item in items {
        // Codex's ChatGPT backend sends its tools as an input item.
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            tools = tools.saturating_add(tool_definitions(item.get("tools"), parts));
            continue;
        }
        openai_item(item, &mut calls, parts);
    }
    (saturating_u32(items.len()), tools)
}

fn openai_item(item: &Value, calls: &mut CallNames, parts: &mut Parts) {
    let bytes = json_bytes(item);
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    // An item with a role and no type is a message.
    let kind = item.get("type").and_then(Value::as_str).unwrap_or(if item.get("role").is_some() { "message" } else { "" });
    match kind {
        "message" => openai_message(item, parts),
        "function_call" | "custom_tool_call" => {
            calls.remember(item.get("call_id"), name);
            parts.add(ContextKind::ToolCall, name, bytes);
        }
        "function_call_output" | "custom_tool_call_output" => parts.add(ContextKind::ToolResult, calls.name(item.get("call_id")), bytes),
        "local_shell_call" => {
            calls.remember(item.get("call_id"), "local_shell");
            parts.add(ContextKind::ToolCall, "local_shell", bytes);
        }
        "local_shell_call_output" => parts.add(ContextKind::ToolResult, "local_shell", bytes),
        "web_search_call" => parts.add(ContextKind::ToolCall, "web_search", bytes),
        "reasoning" => parts.add(ContextKind::Reasoning, "", bytes),
        _ => parts.add(ContextKind::Other, "", bytes),
    }
}

/// A message's content by role; images inside it count as images.
fn openai_message(item: &Value, parts: &mut Parts) {
    let role_kind = match item.get("role").and_then(Value::as_str) {
        Some("assistant") => ContextKind::AssistantText,
        Some("system" | "developer") => ContextKind::System,
        _ => ContextKind::UserText,
    };
    let Some(Value::Array(content)) = item.get("content") else {
        parts.add(role_kind, "", json_bytes(item));
        return;
    };
    for block in content {
        let kind = match block.get("type").and_then(Value::as_str) {
            Some("input_image") => ContextKind::Image,
            _ => role_kind,
        };
        parts.add(kind, "", json_bytes(block));
    }
}

/// One part per tool definition, by name; how many there were.  A
/// namespace (Codex groups its tools as `{"type": "namespace", "name":
/// "functions", "tools": […]}`) counts each tool in it, one level deep; its
/// own description counts under the namespace's name.
fn tool_definitions(tools: Option<&Value>, parts: &mut Parts) -> u32 {
    let tools = tools.and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut count = 0u32;
    for tool in tools {
        // A built-in tool may have only a type (`{"type": "web_search"}`).
        let name = tool.get("name").or_else(|| tool.get("type")).and_then(Value::as_str).unwrap_or("");
        let inner = tool.get("tools").and_then(Value::as_array).filter(|_| tool.get("type").and_then(Value::as_str) == Some("namespace"));
        let Some(inner) = inner else {
            parts.add(ContextKind::ToolDefinition, name, json_bytes(tool));
            count = count.saturating_add(1);
            continue;
        };
        for leaf in inner {
            let leaf_name = leaf.get("name").or_else(|| leaf.get("type")).and_then(Value::as_str).unwrap_or("");
            // Codex's own tools are in `functions`; others (MCP servers) keep
            // their namespace, so two servers' `search` stay apart.
            let label = if name == "functions" { leaf_name.to_string() } else { format!("{name}.{leaf_name}") };
            parts.add(ContextKind::ToolDefinition, &label, json_bytes(leaf));
        }
        if let Some(description) = tool.get("description") {
            parts.add(ContextKind::ToolDefinition, name, json_bytes(description));
        }
        count = count.saturating_add(saturating_u32(inner.len()));
    }
    count
}

/// Tool call ids to tool names, so a result is counted under its tool.
#[derive(Default)]
struct CallNames {
    names: HashMap<String, String>,
}

impl CallNames {
    fn remember(&mut self, id: Option<&Value>, name: &str) {
        let Some(id) = id.and_then(Value::as_str) else { return };
        if self.names.len() < CALL_IDS_MAX {
            self.names.insert(id.to_string(), label(name));
        }
    }

    fn name(&self, id: Option<&Value>) -> &str {
        id.and_then(Value::as_str).and_then(|id| self.names.get(id)).map_or("", String::as_str)
    }
}

/// Parts added up by kind and label.
#[derive(Default)]
struct Parts {
    by_key: HashMap<(ContextKind, String), (u64, u32)>,
}

impl Parts {
    fn add(&mut self, kind: ContextKind, name: &str, bytes: u64) {
        self.add_many(kind, name, bytes, 1);
    }

    fn add_many(&mut self, kind: ContextKind, name: &str, bytes: u64, count: usize) {
        let entry = self.by_key.entry((kind, label(name))).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(bytes);
        entry.1 = entry.1.saturating_add(saturating_u32(count));
    }

    /// Largest first; past [`CONTEXT_PARTS_MAX`], the smallest fold into one
    /// `Other` part, so the total is kept.
    fn finish(self) -> Vec<ContextPart> {
        let total_bytes: u64 = self.by_key.values().map(|(b, _)| *b).fold(0, u64::saturating_add);
        let mut parts: Vec<ContextPart> =
            self.by_key.into_iter().map(|((kind, label), (bytes, count))| ContextPart { kind, label, bytes, count }).collect();
        sort(&mut parts);
        if parts.len() > CONTEXT_PARTS_MAX {
            let mut other = ContextPart { kind: ContextKind::Other, label: String::new(), bytes: 0, count: 0 };
            let kept: Vec<ContextPart> = parts.drain(..).enumerate().filter_map(|(i, p)| {
                let is_plain_other = p.kind == ContextKind::Other && p.label.is_empty();
                if i < CONTEXT_PARTS_MAX - 1 && !is_plain_other {
                    return Some(p);
                }
                other.bytes = other.bytes.saturating_add(p.bytes);
                other.count = other.count.saturating_add(p.count);
                None
            }).collect();
            parts = kept;
            parts.push(other);
            sort(&mut parts);
        }
        assert!(parts.len() <= CONTEXT_PARTS_MAX);
        debug_assert_eq!(parts.iter().map(|p| p.bytes).fold(0, u64::saturating_add), total_bytes);
        parts
    }
}

/// Largest first; ties in a fixed order, so a breakdown reads the same twice.
fn sort(parts: &mut [ContextPart]) {
    parts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)).then_with(|| (a.kind as u8).cmp(&(b.kind as u8))));
}

/// A tool's name, cut to what the store keeps (on a character boundary).
fn label(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    if clean.len() <= LABEL_BYTES_MAX {
        return clean;
    }
    let mut end = LABEL_BYTES_MAX;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].to_string()
}

/// How many bytes `value` takes as JSON, counted without building the text.
fn json_bytes(value: &Value) -> u64 {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len() as u64);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("a JSON value always serializes, and counting never fails");
    count.0
}

fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn part(b: &ContextBreakdown, kind: ContextKind, label: &str) -> Option<(u64, u32)> {
        b.parts.iter().find(|p| p.kind == kind && p.label == label).map(|p| (p.bytes, p.count))
    }

    #[test]
    fn an_anthropic_context_breaks_down_by_kind_and_tool() {
        let body = json!({
            "model": "claude-opus-5-5",
            "system": [{"type": "text", "text": "You are Claude Code."}, {"type": "text", "text": "More."}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}, {"name": "Read", "input_schema": {}}],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "s"},
                    {"type": "text", "text": "Let me look."},
                    {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "a\nb"},
                    {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                    {"type": "text", "text": "and?"}
                ]}
            ]
        });
        let a = analyze(Endpoint::AnthropicMessages, &serde_json::to_vec(&body).unwrap());
        assert_eq!(a.model.as_deref(), Some("claude-opus-5-5"));
        let b = a.breakdown.unwrap();
        assert_eq!((b.messages, b.tools), (3, 2));
        assert_eq!(part(&b, ContextKind::System, ""), Some((json_bytes(&body["system"]), 2)));
        assert_eq!(part(&b, ContextKind::ToolDefinition, "Bash"), Some((json_bytes(&body["tools"][0]), 1)));
        assert_eq!(part(&b, ContextKind::ToolCall, "Bash").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::ToolResult, "Bash").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::UserText, "").map(|p| p.1), Some(2));
        assert_eq!(part(&b, ContextKind::AssistantText, "").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::Image, "").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::Reasoning, "").map(|p| p.1), Some(1));
        assert!(b.parts.windows(2).all(|w| w[0].bytes >= w[1].bytes), "largest first");
    }

    #[test]
    fn an_openai_context_breaks_down_by_item() {
        let body = json!({
            "model": "gpt-5.5-codex",
            "instructions": "You are Codex.",
            "tools": [{"type": "function", "name": "shell", "parameters": {}}, {"type": "web_search"}],
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}, {"type": "input_image", "image_url": "data:"}]},
                {"type": "reasoning", "summary": [], "encrypted_content": "xyz"},
                {"type": "function_call", "name": "shell", "call_id": "c1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}
            ]
        });
        let b = analyze(Endpoint::OpenAiResponses, &serde_json::to_vec(&body).unwrap()).breakdown.unwrap();
        assert_eq!((b.messages, b.tools), (6, 2));
        assert_eq!(part(&b, ContextKind::System, "").map(|p| p.1), Some(2));
        assert!(part(&b, ContextKind::ToolDefinition, "web_search").is_some());
        assert_eq!(part(&b, ContextKind::ToolResult, "shell").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::ToolCall, "shell").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::Image, "").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::Reasoning, "").map(|p| p.1), Some(1));
        assert_eq!(part(&b, ContextKind::AssistantText, "").map(|p| p.1), Some(1));
    }

    #[test]
    fn codex_tools_sent_as_an_input_item_in_namespaces_count_as_tools() {
        let body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "additional_tools", "role": "developer", "id": "x", "tools": [
                    {"type": "namespace", "name": "functions", "description": "Codex's tools", "tools": [
                        {"type": "custom", "name": "exec", "description": "Run"}, {"type": "function", "name": "wait"}
                    ]},
                    {"type": "namespace", "name": "mcp__axel", "description": "", "tools": [{"type": "function", "name": "send"}]}
                ]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "say ok"}]}
            ]
        });
        let b = analyze(Endpoint::OpenAiResponses, &serde_json::to_vec(&body).unwrap()).breakdown.unwrap();
        assert_eq!((b.messages, b.tools), (2, 3));
        assert!(part(&b, ContextKind::ToolDefinition, "exec").is_some());
        assert!(part(&b, ContextKind::ToolDefinition, "mcp__axel.send").is_some());
        assert_eq!(part(&b, ContextKind::ToolDefinition, "functions"), Some((json_bytes(&json!("Codex's tools")), 1)));
        assert!(b.parts.iter().all(|p| p.kind != ContextKind::Other));
    }

    #[test]
    fn past_the_part_limit_the_smallest_fold_into_other() {
        let tools: Vec<Value> = (0..200).map(|i| json!({"name": format!("tool{i:03}"), "description": "x".repeat(i)})).collect();
        let body = json!({"tools": tools, "messages": [{"role": "user", "content": [{"type": "mystery"}]}]});
        let b = analyze(Endpoint::AnthropicMessages, &serde_json::to_vec(&body).unwrap()).breakdown.unwrap();
        assert_eq!(b.parts.len(), CONTEXT_PARTS_MAX);
        let others: Vec<_> = b.parts.iter().filter(|p| p.kind == ContextKind::Other).collect();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].count, 200 - (CONTEXT_PARTS_MAX as u32 - 1) + 1);
        assert!(part(&b, ContextKind::ToolDefinition, "tool199").is_some());
        assert!(part(&b, ContextKind::ToolDefinition, "tool000").is_none());
        let total: u64 = b.parts.iter().map(|p| p.bytes).sum();
        let expected: u64 = body["tools"].as_array().unwrap().iter().map(json_bytes).sum::<u64>() + json_bytes(&json!({"type": "mystery"}));
        assert_eq!(total, expected);
    }

    #[test]
    fn labels_are_bounded_and_bodies_that_are_not_json_give_nothing() {
        let long = "é".repeat(100);
        let body = json!({"tools": [{"name": long}]});
        let b = analyze(Endpoint::AnthropicMessages, &serde_json::to_vec(&body).unwrap()).breakdown.unwrap();
        assert!(b.parts[0].label.len() <= LABEL_BYTES_MAX);
        assert_eq!(analyze(Endpoint::AnthropicMessages, b"\x28\xb5\x2f\xfd compressed"), Analysis::default());
        assert_eq!(analyze(Endpoint::AnthropicModels, b"{}"), Analysis::default());
        assert_eq!(clean_model("gpt 5\n"), None);
        assert_eq!(clean_model(&"m".repeat(MODEL_BYTES_MAX + 1)), None);
    }
}
