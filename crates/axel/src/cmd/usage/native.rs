//! Reconcile local harness calls off the UI thread. Missing files never erase data.
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use anyhow::{Context, Result};
use axel_store::{Harness, NativeUsage, Session, SessionFilter, Store, UsageDelta, pricing};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Scanner {
    // File metadata plus session ownership: unchanged conversations need no reads.
    seen: HashMap<String, Vec<(PathBuf, u64, SystemTime)>>,
}

fn files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() { files(&path, out); }
        else if path.extension().is_some_and(|e| e == "jsonl") { out.push(path); }
    }
}

impl Scanner {
    pub(super) fn scan(&mut self, store: &Store, claude: &Path, codex: &Path) -> Result<usize> {
        let mut groups: BTreeMap<String, Vec<Session>> = BTreeMap::new();
        for session in store.list_sessions(SessionFilter::default())? {
            let Some(id) = &session.harness_session_id else { continue };
            if matches!(session.harness, Harness::Claude | Harness::Codex) {
                groups.entry(format!("{}:{id}", session.harness)).or_default().push(session);
            }
        }
        let archived = codex.with_file_name("archived_sessions");
        let mut paths = Vec::new(); files(claude, &mut paths); files(codex, &mut paths); files(&archived, &mut paths);
        let mut changed = 0;
        for (group, mut sessions) in groups {
            sessions.sort_by_key(|s| (s.started_at, s.id.clone()));
            let native_id = sessions[0].harness_session_id.as_deref().unwrap();
            let is_claude = sessions[0].harness == Harness::Claude;
            let mut source: Vec<_> = paths.iter().filter(|p| {
                let filename = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if is_claude {
                    p.starts_with(claude) && (filename == format!("{native_id}.jsonl") || p.components().any(|c| c.as_os_str() == native_id))
                } else { (p.starts_with(codex) || p.starts_with(&archived)) && filename.ends_with(&format!("{native_id}.jsonl")) }
            }).cloned().collect();
            source.sort();
            if source.is_empty() { continue }
            let metadata = source.iter().map(|p| {
                let m = p.metadata()?; Ok((p.clone(), m.len(), m.modified()?))
            }).collect::<std::io::Result<Vec<_>>>();
            let Ok(mut fingerprint) = metadata else { continue };
            // A new Axel session changes which session owns subsequent calls.
            for s in &sessions { fingerprint.push((PathBuf::from(&s.id), s.started_at as u64, SystemTime::UNIX_EPOCH)); }
            if self.seen.get(&group) == Some(&fingerprint) { continue }
            let result = if is_claude { read_claude(&source) } else { read_codex(&source) };
            let Ok(records) = result else { continue };
            // Unknown or incomplete log formats must not zero out reported usage.
            if records.is_empty() { continue }
            let mut owned: HashMap<String, Vec<NativeUsage>> = sessions.iter().map(|s| (s.id.clone(), Vec::new())).collect();
            for record in records {
                if let Some(session) = sessions.iter().rev().find(|s| s.started_at <= record.at) {
                    owned.get_mut(&session.id).unwrap().push(record);
                }
            }
            for session in &sessions { store.replace_native_usage(&session.id, &owned[&session.id])?; }
            changed += sessions.len();
            self.seen.insert(group, fingerprint);
        }
        Ok(changed)
    }
}

fn stamp(v: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok().map(|d| d.timestamp())
}

fn read_lines(paths: &[PathBuf], mut line: impl FnMut(Value) -> Result<()>) -> Result<()> {
    for path in paths {
        let mut reader = BufReader::new(std::fs::File::open(path)?);
        let mut text = String::new();
        loop {
            text.clear();
            if reader.read_line(&mut text)? == 0 { break }
            // A writer's incomplete final line is retried on the next scan.
            if !text.ends_with('\n') { break }
            let value = serde_json::from_str(&text).with_context(|| format!("invalid native log {}", path.display()))?;
            line(value)?;
        }
    }
    Ok(())
}

fn read_claude(paths: &[PathBuf]) -> Result<Vec<NativeUsage>> {
    let mut calls: BTreeMap<String, NativeUsage> = BTreeMap::new();
    read_lines(paths, |v| {
        if v.get("type").and_then(Value::as_str) != Some("assistant") { return Ok(()) }
        let Some(at) = stamp(&v) else { anyhow::bail!("Claude usage without timestamp") };
        let Some(m) = v.get("message") else { return Ok(()) };
        let Some(u) = m.get("usage") else { return Ok(()) };
        let model = m.get("model").and_then(Value::as_str);
        if model == Some("<synthetic>") { return Ok(()) }
        let Some(id) = m.get("id").and_then(Value::as_str) else { anyhow::bail!("Claude usage without message id") };
        let id = format!("{}:{}", id, v.get("requestId").and_then(Value::as_str).unwrap_or(""));
        let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
        let hour = u.pointer("/cache_creation/ephemeral_1h_input_tokens").and_then(Value::as_i64).unwrap_or(0);
        let mut delta = UsageDelta {
            input_tokens:n("input_tokens"), output_tokens:n("output_tokens"), cache_read_tokens:n("cache_read_input_tokens"),
            cache_write_tokens:n("cache_creation_input_tokens"), model:model.map(String::from), ..Default::default()
        };
        delta.cost_usd_est = pricing::estimate_call(model, delta.input_tokens, delta.output_tokens, delta.cache_read_tokens, delta.cache_write_tokens, hour);
        // Claude repeats usage for each content block. The final block wins.
        calls.insert(id.clone(), NativeUsage { id, at, delta });
        Ok(())
    })?;
    Ok(calls.into_values().collect())
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Counts { input:i64, output:i64, cached:i64, written:i64, reasoning:i64 }
impl Counts {
    fn parse(v: &Value) -> Self {
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
        Self {input:n("input_tokens"),output:n("output_tokens"),cached:n("cached_input_tokens"),written:n("cache_write_input_tokens"),reasoning:n("reasoning_output_tokens")}
    }
    fn since(self, prev: Self) -> Option<Self> {
        Some(Self {input:self.input.checked_sub(prev.input).filter(|v| *v>=0)?,output:self.output.checked_sub(prev.output).filter(|v| *v>=0)?,cached:self.cached.checked_sub(prev.cached).filter(|v| *v>=0)?,written:self.written.checked_sub(prev.written).filter(|v| *v>=0)?,reasoning:self.reasoning.checked_sub(prev.reasoning).filter(|v| *v>=0)?})
    }
}

fn read_codex(paths: &[PathBuf]) -> Result<Vec<NativeUsage>> {
    // Each rollout is read in file order, with its own cumulative baseline.
    let mut calls = BTreeMap::new();
    for path in paths {
        let mut previous = None;
        let mut model = None;
        let mut fast = false;
        let mut ledger = false;
        let mut last_legacy: Option<(String,Counts)> = None;
        read_lines(std::slice::from_ref(path), |v| {
            let p = &v["payload"];
            if v["type"] == "turn_context" {
                if let Some(m) = p["model"].as_str() { model = Some(m.to_string()); }
                // ChatGPT/Codex fast quota multipliers are not API dollar charges.
                fast = p["service_tier"] == "priority" && p["auth_mode"] == "api_key";
            }
            let (delta, id) = if v["type"] == "token_usage_record" {
                let Some(response) = p["response_id"].as_str() else { anyhow::bail!("Codex ledger without response id") };
                let Some(u) = p.get("usage").filter(|u| u.is_object()) else { return Ok(()) };
                if !ledger {
                    // A ledger row can follow its legacy mirror at a format cutover.
                    if let Some((id, total)) = &last_legacy
                        && p.get("thread_token_usage").map(Counts::parse).is_some_and(|t| t == *total)
                    { calls.remove(id); }
                }
                ledger = true;
                (Counts::parse(u), response.to_string())
            } else {
                if p["type"] != "token_count" || ledger { return Ok(()) }
                let Some(total) = p.pointer("/info/total_token_usage").filter(|x| x.is_object()) else { return Ok(()) };
                let total = Counts::parse(total);
                let last = p.pointer("/info/last_token_usage").map(Counts::parse).unwrap_or_default();
                let delta = previous.map(|prev| total.since(prev).unwrap_or(last)).unwrap_or(last);
                previous = Some(total);
                let id = format!("{}:{}:{}:{}", path.file_name().unwrap().to_string_lossy(), v["timestamp"], total.input, total.output);
                last_legacy = Some((id.clone(),total));
                (delta,id)
            };
            if delta.input + delta.output == 0 { return Ok(()) }
            let Some(at) = stamp(&v) else { anyhow::bail!("Codex usage without timestamp") };
            let cached = delta.cached.min(delta.input);
            let written = delta.written.min(delta.input - cached);
            let mut usage = UsageDelta {input_tokens:delta.input-cached-written,output_tokens:delta.output,cache_read_tokens:cached,cache_write_tokens:written,reasoning_tokens:delta.reasoning.min(delta.output),model:model.clone(),..Default::default()};
            let model = model.as_deref().context("Codex usage without model")?;
            usage.cost_usd_est = pricing::estimate_call(Some(model),usage.input_tokens,usage.output_tokens,cached,written,0) * if fast {2.0} else {1.0};
            calls.insert(id.clone(), NativeUsage {id,at,delta:usage});
            Ok(())
        })?;
    }
    Ok(calls.into_values().collect())
}

pub(super) fn roots() -> Option<(PathBuf,PathBuf)> {
    let home = dirs::home_dir()?;
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
    let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
    Some((claude.join("projects"),codex.join("sessions")))
}

pub(super) fn start() {
    let database = Store::default_path();
    let Some((claude,codex)) = roots() else { return };
    let _ = std::thread::Builder::new().name("axel-native-usage".into()).spawn(move || {
        let Ok(store) = Store::open(&database) else { return };
        let mut scanner = Scanner::default();
        loop {
            if let Err(e) = scanner.scan(&store,&claude,&codex) { eprintln!("axel: usage reconciliation: {e}"); }
            std::thread::sleep(Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn claude_deduplicates_blocks_and_prices_hour_cache_writes() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("claude.jsonl");
        let call = |output| json!({"type":"assistant","timestamp":"2026-11-09T12:00:00Z","requestId":"r","message":{"id":"m","model":"claude-opus-5-5","usage":{"input_tokens":2,"output_tokens":output,"cache_read_input_tokens":1000,"cache_creation_input_tokens":1000,"cache_creation":{"ephemeral_1h_input_tokens":1000}}}});
        std::fs::write(&path,format!("{}\n{}\n{{\"partial\"",call(1),call(10))).unwrap();
        let rows = read_claude(&[path]).unwrap();
        assert_eq!(rows.len(),1); assert_eq!(rows[0].delta.output_tokens,10);
        assert!((rows[0].delta.cost_usd_est-0.008408).abs()<1e-12);
    }
    #[test]
    fn codex_differences_counters_and_does_not_repeat_duplicate_reports() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("codex.jsonl");
        let report = |i,o,last_i,last_o| json!({"timestamp":"2026-11-09T12:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":i,"output_tokens":o,"cached_input_tokens":i-100},"last_token_usage":{"input_tokens":last_i,"output_tokens":last_o,"cached_input_tokens":last_i-100}}}});
        let context = json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol"}});
        std::fs::write(&path,format!("{context}\n{}\n{}\n{}\n",report(300_000,100,300_000,100),report(300_000,100,300_000,100),report(600_000,200,300_000,100))).unwrap();
        let rows=read_codex(&[path]).unwrap(); assert_eq!(rows.len(),2);
        assert_eq!(rows.iter().map(|r|r.delta.input_tokens+r.delta.cache_read_tokens).sum::<i64>(),600_000);
        assert!((rows.iter().map(|r|r.delta.cost_usd_est).sum::<f64>()-0.12338).abs()<1e-12);
    }
    #[test]
    fn request_ledger_wins_over_mirrors_even_after_compaction_changes_totals() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("codex.jsonl");
        let context=serde_json::json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol"}});
        let record=serde_json::json!({"type":"token_usage_record","timestamp":"2026-11-09T12:00:00Z","payload":{"response_id":"resp_1","usage":{"input_tokens":1000,"output_tokens":100},"thread_token_usage":{"input_tokens":90000,"output_tokens":1000}}});
        let mirror=serde_json::json!({"timestamp":"2026-11-09T12:00:01Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":70000,"output_tokens":800},"last_token_usage":{"input_tokens":1000,"output_tokens":100}}}});
        std::fs::write(&path,format!("{context}\n{record}\n{mirror}\n{record}\n")).unwrap();
        let rows=read_codex(&[path]).unwrap();
        assert_eq!(rows.len(),1,"a request ID counts once; its mirrored notification adds nothing");
        assert_eq!(rows[0].delta.input_tokens,1000);
        assert!((rows[0].delta.cost_usd_est-0.003).abs()<1e-12);
    }

}
