//! A tiny ACP agent for tests: NDJSON JSON-RPC over stdio, no SDK.
//!
//! Per prompt it streams two message chunks, one `tool_call`, asks for
//! permission, sends a `tool_call_update`, and ends the turn.  Special prompts:
//! `readfile:<path>` asks the client to `fs/read_text_file` and echoes the
//! content; `writefile:<path>:<text>` asks the client to write; `sleep` waits
//! until `session/cancel` and returns `cancelled`; `thought` also streams a
//! thought chunk.  Set `FAKE_AGENT_PERMISSION=0` to skip the permission request.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let mut prompts = 0u32;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => respond(
                id,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": false, "promptCapabilities": { "image": true, "embeddedContext": false } },
                    "agentInfo": { "name": "fake-acp-agent", "version": "0.0.1" }
                }),
            ),
            "session/new" => respond(id, json!({ "sessionId": "fake-session-1" })),
            "session/prompt" => {
                prompts += 1;
                prompt(&mut lines, id, &params, &format!("call-{prompts}"));
            }
            "session/cancel" => {}
            _ => {
                if id.is_some() {
                    send(
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } }),
                    );
                }
            }
        }
    }
}

fn prompt(
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
    id: Option<Value>,
    params: &Value,
    call_id: &str,
) {
    let sid = params
        .get("sessionId")
        .cloned()
        .unwrap_or(json!("fake-session-1"));
    let text = params
        .pointer("/prompt/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let update = |u: Value| {
        send(
            json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": sid, "update": u } }),
        )
    };

    if let Some(path) = text.strip_prefix("readfile:") {
        let resp = request(
            lines,
            50,
            "fs/read_text_file",
            json!({ "sessionId": sid, "path": path }),
        );
        let content = resp
            .as_ref()
            .and_then(|r| r.pointer("/result/content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!("error: {}", resp.map(|r| r.to_string()).unwrap_or_default())
            });
        update(
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("file says: {content}") } }),
        );
        respond(id, json!({ "stopReason": "end_turn" }));
        return;
    }
    if let Some(rest) = text.strip_prefix("writefile:") {
        let (path, body) = rest.split_once(':').unwrap_or((rest, ""));
        let resp = request(
            lines,
            51,
            "fs/write_text_file",
            json!({ "sessionId": sid, "path": path, "content": body }),
        );
        let ok = resp.as_ref().is_some_and(|r| r.get("error").is_none());
        update(
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": if ok { "written" } else { "write denied" } } }),
        );
        respond(id, json!({ "stopReason": "end_turn" }));
        return;
    }
    // A turn that spends: context fill, then the usage in the reply. `spend`
    // marks it as Axel's adapters do; `spend-unmarked` doesn't.
    if text == "spend" || text == "spend-unmarked" {
        update(json!({ "sessionUpdate": "usage_update", "used": 1234, "size": 200000 }));
        send(json!({ "jsonrpc": "2.0", "method": "_axel/limits", "params": { "windows": [{ "window": "five_hour", "usedPercent": 30.0, "resetsAt": 1000 }] } }));
        let mut reply = json!({ "stopReason": "end_turn",
            "usage": { "totalTokens": 160, "inputTokens": 10, "outputTokens": 50, "cachedReadTokens": 100, "cachedWriteTokens": 0, "thoughtTokens": 5 } });
        if text == "spend" {
            reply["_meta"] = json!({ "axel": { "costUsd": 0.5, "model": "fake-model" } });
        }
        respond(id, reply);
        return;
    }
    if text == "sleep" {
        // Wait for session/cancel.
        for l in lines.by_ref() {
            let Ok(l) = l else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&l)
                && v.get("method").and_then(Value::as_str) == Some("session/cancel")
            {
                break;
            }
        }
        respond(id, json!({ "stopReason": "cancelled" }));
        return;
    }

    if text == "ask" {
        // A question for the owner, as the Claude adapter asks AskUserQuestion.
        let resp = request(
            lines,
            60,
            "session/request_permission",
            json!({
                "sessionId": sid,
                "toolCall": { "toolCallId": "toolu_q#0", "title": "Database: Which one?", "kind": "think" },
                "options": [
                    { "optionId": "answer:0", "name": "Postgres", "kind": "allow_once" },
                    { "optionId": "answer:1", "name": "SQLite", "kind": "allow_once" },
                    { "optionId": "answer:none", "name": "None of these", "kind": "reject_once" }
                ]
            }),
        );
        let picked = resp.as_ref().and_then(|r| r.pointer("/result/outcome/optionId")).and_then(Value::as_str).unwrap_or("none").to_string();
        update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("picked {picked}") } }));
        respond(id, json!({ "stopReason": "end_turn" }));
        return;
    }
    if text.contains("thought") {
        update(
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "Let me think." } }),
        );
    }
    update(
        json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Hello, " } }),
    );
    update(
        json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "world." } }),
    );
    update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": "Bash",
        "kind": "execute",
        "status": "pending",
        "rawInput": { "command": "cargo test" }
    }));
    let mut allowed = true;
    if std::env::var("FAKE_AGENT_PERMISSION").as_deref() != Ok("0") {
        let resp = request(
            lines,
            100,
            "session/request_permission",
            json!({
                "sessionId": sid,
                "toolCall": { "toolCallId": call_id, "title": "Bash", "kind": "execute", "rawInput": { "command": "cargo test" } },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
                ]
            }),
        );
        let outcome = resp
            .as_ref()
            .and_then(|r| r.pointer("/result/outcome").cloned())
            .unwrap_or(Value::Null);
        allowed = outcome.get("outcome").and_then(Value::as_str) == Some("selected")
            && outcome
                .get("optionId")
                .and_then(Value::as_str)
                .is_some_and(|o| o.starts_with("allow"));
    }
    update(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "status": if allowed { "completed" } else { "failed" },
        "rawOutput": if allowed { "test result: ok" } else { "permission denied" },
        "locations": [{ "path": "Cargo.toml" }]
    }));
    update(
        json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": if allowed { "Tests pass." } else { "I was not allowed to run tests." } } }),
    );
    respond(id, json!({ "stopReason": "end_turn" }));
}

/// Send a request and block until its response arrives (other traffic is dropped).
fn request(
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
    id: u64,
    method: &str,
    params: Value,
) -> Option<Value> {
    send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    for l in lines.by_ref() {
        let Ok(l) = l else { return None };
        let Ok(v) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) == Some(id) && v.get("method").is_none() {
            return Some(v);
        }
    }
    None
}

fn respond(id: Option<Value>, result: Value) {
    send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn send(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}
