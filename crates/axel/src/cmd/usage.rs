//! Token accounting from harness transcripts (no telemetry server).

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use axel_identity::Keypair;
use axel_store::{Store, UsageDelta};
use serde_json::Value;

use axel_store::pricing;

/// Claude Code: tail the transcript JSONL from the last seen offset and add the
/// `usage` of every new assistant message.
pub fn record_claude(store: &Store, signer: &Keypair, session_id: &str, transcript_path: &str, model_hint: Option<&str>) -> Result<()> {
    let path = Path::new(transcript_path);
    if !path.is_file() {
        return Ok(());
    }
    let offset = store.get_session(session_id)?.map(|s| s.transcript_offset.max(0) as u64).unwrap_or(0);
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if offset > len { 0 } else { offset }; // file was rotated/rewritten
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut consumed = start;
    let mut lines = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            break; // end, or a partial line still being written: re-read next time
        }
        consumed += n as u64;
        lines.push(line.clone());
    }
    let (mut delta, model) = claude_usage(lines.iter().map(String::as_str), model_hint);
    if delta.turns == 0 && consumed == offset {
        return Ok(());
    }
    delta.cost_usd_est = pricing::estimate(model.as_deref(), delta.input_tokens, delta.output_tokens, delta.cache_read_tokens, delta.cache_write_tokens);
    delta.model = model;
    store.add_usage(signer, session_id, &delta, Some(consumed))?;
    Ok(())
}

/// The usage of the assistant messages in transcript lines, and the last
/// model named. Claude Code writes one line per content block, each
/// repeating its message's `usage`, so a message counts once: by
/// `message.id` and `requestId`, its last line winning.
fn claude_usage<'a>(lines: impl Iterator<Item = &'a str>, model_hint: Option<&str>) -> (UsageDelta, Option<String>) {
    let mut model = model_hint.map(String::from);
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut usages: Vec<Value> = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let m = msg.get("model").and_then(|m| m.as_str());
        if m == Some("<synthetic>") {
            continue;
        }
        if let Some(m) = m {
            model = Some(m.to_string());
        }
        let Some(u) = msg.get("usage") else { continue };
        let id = msg.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let req = v.get("requestId").and_then(|i| i.as_str()).unwrap_or("");
        if id.is_empty() && req.is_empty() {
            usages.push(u.clone());
            continue;
        }
        match seen.get(&format!("{id}:{req}")) {
            Some(&at) => usages[at] = u.clone(),
            None => {
                seen.insert(format!("{id}:{req}"), usages.len());
                usages.push(u.clone());
            }
        }
    }
    let mut delta = UsageDelta::default();
    for u in &usages {
        let g = |k: &str| u.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        delta.input_tokens += g("input_tokens");
        delta.output_tokens += g("output_tokens");
        delta.cache_read_tokens += g("cache_read_input_tokens");
        delta.cache_write_tokens += g("cache_creation_input_tokens");
        delta.turns += 1;
    }
    (delta, model)
}

/// Codex: find the rollout for a thread id and record the latest totals.
/// Rollouts report cumulative totals, so we store the difference.
pub fn record_codex(store: &Store, signer: &Keypair, session_id: &str, codex_thread_id: &str, model_hint: Option<&str>) -> Result<()> {
    let Some(rollout) = find_codex_rollout(codex_thread_id) else { return Ok(()) };
    let mut text = String::new();
    std::fs::File::open(&rollout)?.read_to_string(&mut text)?;
    let mut totals: Option<(i64, i64, i64, i64)> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let info = v.pointer("/payload/info/total_token_usage").or_else(|| v.pointer("/info/total_token_usage"));
        if let Some(t) = info {
            let g = |k: &str| t.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
            // Codex's input includes what was cached; ours doesn't.
            let (cached, written) = (g("cached_input_tokens"), g("cache_write_input_tokens"));
            totals = Some(((g("input_tokens") - cached - written).max(0), g("output_tokens"), cached, written));
        }
    }
    let Some((i, o, cr, cw)) = totals else { return Ok(()) };
    let prev = store.usage(session_id)?.unwrap_or_default();
    let delta = UsageDelta {
        input_tokens: (i - prev.input_tokens).max(0),
        output_tokens: (o - prev.output_tokens).max(0),
        cache_read_tokens: (cr - prev.cache_read_tokens).max(0),
        cache_write_tokens: (cw - prev.cache_write_tokens).max(0),
        turns: 1,
        cost_usd_est: 0.0,
        ..Default::default()
    };
    let mut delta = delta;
    delta.cost_usd_est = pricing::estimate(model_hint.or(Some("codex")), delta.input_tokens, delta.output_tokens, delta.cache_read_tokens, delta.cache_write_tokens);
    store.add_usage(signer, session_id, &delta, None)?;
    Ok(())
}

fn find_codex_rollout(codex_thread_id: &str) -> Option<std::path::PathBuf> {
    let root = dirs::home_dir()?.join(".codex").join("sessions");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(codex_thread_id) && n.ends_with(".jsonl")) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_message_split_over_lines_counts_once() {
        let block = |id: &str, out: i64| {
            format!(r#"{{"type":"assistant","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"claude-opus-5-5","usage":{{"input_tokens":2,"cache_read_input_tokens":100,"cache_creation_input_tokens":10,"output_tokens":{out}}}}}}}"#)
        };
        // msg_a has three content blocks (thinking, text, tool use), msg_b one.
        let lines = [block("a", 5), block("a", 5), block("a", 9), r#"{"type":"user"}"#.to_string(), block("b", 3)];
        let (d, model) = claude_usage(lines.iter().map(String::as_str), None);
        assert_eq!((d.input_tokens, d.cache_read_tokens, d.cache_write_tokens), (4, 200, 20), "two messages, not four");
        assert_eq!(d.output_tokens, 9 + 3, "a message's last line wins");
        assert_eq!(d.turns, 2);
        assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
    }
}
