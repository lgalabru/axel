//! Ingest harness hook events. Reads one JSON object from stdin, then:
//!
//! 1. mirrors the state to Rex through OSC 7501 (tab indicators),
//! 2. records a status row and the thread state,
//! 3. posts a `question` / `permission_request` message when a human is needed,
//! 4. adds token usage from the harness transcript,
//! 5. for `PreToolUse`, acts as the autopilot **guard**: denies a short list of
//!    irreversible actions and lets everything else through,
//! 6. for Claude, checkpoints the workspace at each turn's start and end
//!    (`UserPromptSubmit`, `Stop`) and shares the turn with what it changed.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use axel_acp::checkpoint::{self, Which};
use axel_inbox::{EnvelopeBuilder, kind};
use axel_rex::osc;
use axel_store::{SessionState, SessionUpdate, StatusReport, TurnMark, TurnSummary};
use serde_json::Value;

use super::{Ctx, convert, launch, usage};
use crate::cli::HarnessArg;

pub fn run(harness: HarnessArg, guard_only: bool) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);
    let (Ok(thread_id), Ok(session_id)) = (std::env::var("AXEL_THREAD"), std::env::var("AXEL_SESSION")) else {
        // Not launched by Axel: stay silent so hooks are harmless elsewhere.
        return Ok(());
    };
    let agent_pk = std::env::var("AXEL_AGENT").unwrap_or_default();
    let model = std::env::var("AXEL_MODEL").ok().filter(|m| !m.is_empty());
    let ctx = Ctx::open()?;
    // A session Axel doesn't know (stale env, other machine's DB): stay silent.
    let Some(session) = ctx.store.get_session(&session_id)? else { return Ok(()) };
    // Everything below is signed by the agent; without its key we can only watch.
    let Some(agent_kp) = agent_key(&ctx, &agent_pk)? else { return Ok(()) };

    // Learn the harness's own session id (for --resume) and transcript path once.
    if harness == HarnessArg::Claude {
        let hs = event.get("session_id").and_then(|s| s.as_str()).map(String::from);
        let tp = event.get("transcript_path").and_then(|s| s.as_str()).map(String::from);
        if (hs.is_some() && session.harness_session_id.is_none()) || (tp.is_some() && session.transcript_path.is_none()) {
            ctx.store.update_session(&agent_kp, &session_id, &SessionUpdate { harness_session_id: hs, transcript_path: tp, ..Default::default() })?;
        }
    } else if let Some(tid) = event.get("thread-id").and_then(|t| t.as_str())
        && session.harness_session_id.is_none() {
            ctx.store.update_session(&agent_kp, &session_id, &SessionUpdate { harness_session_id: Some(tid.to_string()), ..Default::default() })?;
        }

    // Guard first: a denied tool never changes status.
    if harness == HarnessArg::Claude && event.get("hook_event_name").and_then(|n| n.as_str()) == Some("PreToolUse")
        && let Some(reason) = guard::deny_reason(&event, &disallowed_tools()) {
            println!(
                "{}",
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": format!("axel guard: {reason}")
                    }
                })
            );
            post_note(&ctx, &thread_id, &session_id, &agent_kp, &format!("guard denied: {reason}"))?;
            return Ok(());
        }
    if guard_only {
        // Managed sessions: the ACP host records status, usage and permissions itself.
        return Ok(());
    }

    let mapped = match harness {
        HarnessArg::Claude => map_claude(&event),
        HarnessArg::Codex => map_codex(&event),
    };

    // Token usage, even for events we don't otherwise map.
    match harness {
        HarnessArg::Claude => {
            let name = event.get("hook_event_name").and_then(|n| n.as_str()).unwrap_or("");
            if matches!(name, "PostToolUse" | "Stop" | "SubagentStop" | "SessionEnd")
                && let Some(path) = event.get("transcript_path").and_then(|p| p.as_str())
                    && let Err(e) = usage::record_claude(&ctx.store, &agent_kp, &session_id, path, model.as_deref()) {
                        eprintln!("axel: usage: {e}");
                    }
        }
        HarnessArg::Codex => {
            if let Some(tid) = event.get("thread-id").and_then(|t| t.as_str())
                && let Err(e) = usage::record_codex(&ctx.store, &agent_kp, &session_id, tid, model.as_deref()) {
                    eprintln!("axel: usage: {e}");
                }
        }
    }

    if harness == HarnessArg::Claude
        && let Err(e) = claude_turns(&ctx, &agent_kp, &session_id, &thread_id, &event)
    {
        eprintln!("axel: checkpoint: {e:#}");
    }

    let Some(m) = mapped else { return Ok(()) };

    let app = match harness {
        HarnessArg::Claude => "claude-code",
        HarnessArg::Codex => "codex",
    };
    let title = ctx.store.get_thread(&thread_id)?.map(|t| t.title);
    let _ = osc::emit_to_tty(&osc::Report {
        state: m.osc_state,
        id: None,
        kind: m.kind,
        progress: None,
        app: Some(app),
        title: title.as_deref(),
        msg: Some(&m.msg),
    });

    ctx.store.record_status(
        &agent_kp,
        &session_id,
        &StatusReport {
            state: convert::status_state(m.osc_state),
            kind: m.kind.map(convert::blocked_kind),
            title: title.clone(),
            msg: Some(m.msg.clone()),
            progress: None,
        },
    )?;
    if let Some(state) = m.session_state {
        if state == SessionState::Ended {
            ctx.store.end_session(&agent_kp, &session_id, Some("harness exited"))?;
        } else {
            ctx.store.update_session(&agent_kp, &session_id, &SessionUpdate { state: Some(state), ..Default::default() })?;
        }
    }

    if let Some(req_kind) = m.request {
        let owner_pk = ctx.owner()?.public().to_string();
        let env = EnvelopeBuilder::new(req_kind, m.msg.clone())
            .thread(&thread_id)
            .session(&session_id)
            .mention(&owner_pk)
            .tag(vec!["k".into(), m.kind.unwrap_or(osc::Kind::Question).as_str().into()])
            .sign(&agent_kp)?;
        let request_id = env.id.clone();
        ctx.store.post(&env)?;

        if m.can_decide {
            let wait = launch::hook_wait_secs();
            if wait > 0
                && let Some(decision) = wait_for_reply(&ctx, &request_id, Duration::from_secs(wait))? {
                    println!(
                        "{}",
                        serde_json::json!({
                            "hookSpecificOutput": {
                                "hookEventName": "PermissionRequest",
                                "decision": { "behavior": decision }
                            }
                        })
                    );
                }
        }
    }
    Ok(())
}

/// A pane session's turns, from Claude's hooks: `UserPromptSubmit` opens a
/// turn and takes its `before` checkpoint, `PostToolUse` notes the files the
/// edit tools name, `Stop` takes `after` and shares the turn (kind 40) with
/// what changed, as the managed host does.  Each hook is a fresh process, so
/// the turn in progress lives in the store (`TurnMark`).  A snapshot gets
/// [`HOOK_BUDGET`]: the hook itself has 10 s.
fn claude_turns(ctx: &Ctx, agent_kp: &axel_identity::Keypair, session_id: &str, thread_id: &str, event: &Value) -> Result<()> {
    let name = event.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    if !matches!(name, "UserPromptSubmit" | "PostToolUse" | "Stop") {
        return Ok(());
    }
    let cwd = event.get("cwd").and_then(Value::as_str).map(PathBuf::from).or_else(|| std::env::current_dir().ok());
    let mut mark = ctx.store.turn_mark(session_id)?.unwrap_or_else(|| TurnMark { session_id: session_id.to_string(), ..Default::default() });
    let snapshot = |ordinal: i64, which: Which, same_as: Option<&checkpoint::Snapshot>| -> Option<checkpoint::Snapshot> {
        let dir = cwd.as_deref()?;
        match checkpoint::capture(dir, &checkpoint::ref_name(thread_id, session_id, ordinal, which), HOOK_BUDGET, same_as) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("axel: turn {ordinal} checkpoint skipped: {e:#}");
                None
            }
        }
    };
    match name {
        "UserPromptSubmit" => {
            mark.ordinal += 1;
            mark.open = true;
            mark.user_text = event.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
            mark.authored.clear();
            mark.tool_calls = 0;
            mark.started_at = chrono::Utc::now().timestamp_millis();
            let before = snapshot(mark.ordinal, Which::Before, None);
            mark.before_commit = before.as_ref().map(|s| s.commit.clone());
            mark.before_tree = before.map(|s| s.tree);
        }
        "PostToolUse" if mark.open => {
            mark.tool_calls += 1;
            let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or("");
            if matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
                && let Some(p) = event.get("tool_input").and_then(|i| ["file_path", "notebook_path"].iter().find_map(|k| i.get(k).and_then(Value::as_str)))
            {
                let p = PathBuf::from(p);
                let full = if p.is_absolute() { p } else { cwd.as_deref().map_or(p.clone(), |d| d.join(&p)) };
                let full = full.to_string_lossy().into_owned();
                if !mark.authored.contains(&full) {
                    mark.authored.push(full);
                }
            }
        }
        "Stop" if mark.open => {
            let before = match (&mark.before_commit, &mark.before_tree) {
                (Some(c), Some(t)) => Some(checkpoint::Snapshot { commit: c.clone(), tree: t.clone() }),
                _ => None,
            };
            let after = before.as_ref().and_then(|b| snapshot(mark.ordinal, Which::After, Some(b)));
            let changes = match (&before, &after, cwd.as_deref()) {
                (Some(b), Some(a), Some(dir)) => {
                    let authored: Vec<PathBuf> = mark.authored.iter().map(PathBuf::from).collect();
                    match checkpoint::changes(dir, b, a, &authored) {
                        Ok(mut c) => {
                            c.between_turns = mark.last_after_tree.as_ref().is_some_and(|t| *t != b.tree);
                            Some(c)
                        }
                        Err(e) => {
                            eprintln!("axel: turn {} changes unknown: {e:#}", mark.ordinal);
                            None
                        }
                    }
                }
                _ => None,
            };
            let assistant_text = event
                .get("transcript_path")
                .and_then(Value::as_str)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| last_assistant_text(&t))
                .unwrap_or_default();
            let summary = TurnSummary {
                ordinal: mark.ordinal,
                user_text: mark.user_text.clone(),
                assistant_text,
                tool_calls: mark.tool_calls,
                files_edited: mark.authored.len() as i64,
                duration_ms: (chrono::Utc::now().timestamp_millis() - mark.started_at).max(0),
                stop_reason: "end_turn".into(),
                changes,
            };
            ctx.store.record_turn(agent_kp, session_id, &summary)?;
            mark.open = false;
            mark.before_commit = None;
            mark.before_tree = None;
            if let Some(a) = after {
                mark.last_after_tree = Some(a.tree);
            }
        }
        _ => return Ok(()),
    }
    Ok(ctx.store.put_turn_mark(&mark)?)
}

/// The most a hook's snapshot may take (the hook has 10 s in all).
const HOOK_BUDGET: Duration = Duration::from_secs(6);

/// The assistant's prose of the last turn in a Claude transcript: the text
/// blocks of the assistant lines after the last prompt the human wrote (a
/// `user` line whose content is a tool result doesn't start a turn).
fn last_assistant_text(transcript: &str) -> String {
    let mut out = String::new();
    for line in transcript.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let content = v.get("message").and_then(|m| m.get("content"));
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let human = match content {
                    Some(Value::String(_)) => true,
                    Some(Value::Array(blocks)) => blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("text")),
                    _ => false,
                };
                if human {
                    out.clear();
                }
            }
            Some("assistant") => {
                if let Some(Value::Array(blocks)) = content {
                    for b in blocks {
                        if b.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(t) = b.get("text").and_then(Value::as_str)
                        {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

struct Mapped {
    osc_state: osc::State,
    kind: Option<osc::Kind>,
    msg: String,
    session_state: Option<SessionState>,
    /// Post a message of this kind into the owner's inbox.
    request: Option<u32>,
    /// A hook response can allow/deny (Claude `PermissionRequest`).
    can_decide: bool,
}

fn working(msg: String) -> Mapped {
    Mapped { osc_state: osc::State::Working, kind: None, msg, session_state: Some(SessionState::Running), request: None, can_decide: false }
}

fn map_claude(e: &Value) -> Option<Mapped> {
    let name = e.get("hook_event_name")?.as_str()?;
    let tool = e.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    let short_input = e.get("tool_input").map(summarize_tool_input).unwrap_or_default();
    Some(match name {
        "SessionStart" => working("session started".into()),
        "UserPromptSubmit" => working("thinking".into()),
        "PreToolUse" if tool == "AskUserQuestion" => {
            let q = e.get("tool_input").map(summarize_question).unwrap_or_else(|| "design question".into());
            Mapped { osc_state: osc::State::Blocked, kind: Some(osc::Kind::Question), msg: q, session_state: Some(SessionState::Blocked), request: Some(kind::QUESTION), can_decide: false }
        }
        "PreToolUse" => working(format!("{tool} {short_input}").trim().into()),
        "PostToolUse" if tool == "AskUserQuestion" => working("answered".into()),
        "PostToolUse" => Mapped { session_state: None, ..working(format!("{tool} done")) },
        "PermissionRequest" => Mapped {
            osc_state: osc::State::Blocked,
            kind: Some(osc::Kind::Permission),
            msg: format!("permission: {tool} {short_input}").trim().into(),
            session_state: Some(SessionState::Blocked),
            request: Some(kind::PERMISSION_REQUEST),
            can_decide: true,
        },
        "Notification" => {
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("needs attention").to_string();
            let ntype = e.get("notification_type").and_then(|t| t.as_str()).unwrap_or("");
            let is_perm = ntype == "permission_prompt" || msg.to_ascii_lowercase().contains("permission");
            Mapped {
                osc_state: osc::State::Blocked,
                kind: Some(if is_perm { osc::Kind::Permission } else { osc::Kind::Question }),
                msg,
                session_state: Some(SessionState::Blocked),
                // PermissionRequest already posted the permission; only post other notifications.
                request: if is_perm { None } else { Some(kind::QUESTION) },
                can_decide: false,
            }
        }
        "Stop" => Mapped { osc_state: osc::State::Done, kind: None, msg: "turn finished".into(), session_state: Some(SessionState::Idle), request: None, can_decide: false },
        "SubagentStop" => Mapped { session_state: None, ..working("subagent finished".into()) },
        "SessionEnd" => Mapped { osc_state: osc::State::Clear, kind: None, msg: "session ended".into(), session_state: Some(SessionState::Ended), request: None, can_decide: false },
        _ => return None,
    })
}

fn map_codex(e: &Value) -> Option<Mapped> {
    let t = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
    Some(match t {
        "agent-turn-complete" => {
            let msg: String = e
                .get("last-assistant-message")
                .and_then(|m| m.as_str())
                .map(|s| s.lines().next().unwrap_or("").chars().take(120).collect())
                .unwrap_or_else(|| "turn finished".into());
            // Codex has no AskUserQuestion; a turn that ends with a question mark is one.
            let asks = msg.trim_end().ends_with('?');
            Mapped {
                osc_state: if asks { osc::State::Blocked } else { osc::State::Done },
                kind: if asks { Some(osc::Kind::Question) } else { None },
                msg,
                session_state: Some(if asks { SessionState::Blocked } else { SessionState::Idle }),
                request: if asks { Some(kind::QUESTION) } else { None },
                can_decide: false,
            }
        }
        _ => return None,
    })
}

/// The autopilot guard for pane sessions (hook-mediated). The rules live in
/// `axel_acp::policy` so managed sessions apply the same ones.
pub mod guard {
    use serde_json::Value;

    pub fn deny_reason(event: &Value, disallowed_tools: &[String]) -> Option<String> {
        let tool = event.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
        if disallowed_tools.iter().any(|d| d == tool) {
            return Some(format!("tool {tool} is disallowed for this agent"));
        }
        if tool != "Bash" {
            return None;
        }
        let cmd = event.pointer("/tool_input/command").and_then(|c| c.as_str()).unwrap_or("");
        axel_acp::policy::shell_deny_reason(cmd)
    }
}

fn disallowed_tools() -> Vec<String> {
    std::env::var("AXEL_DISALLOWED_TOOLS")
        .ok()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default()
}

fn agent_key(ctx: &Ctx, agent_pk: &str) -> Result<Option<axel_identity::Keypair>> {
    if agent_pk.is_empty() {
        return Ok(None);
    }
    Ok(ctx.keystore.agent(&agent_pk.parse()?)?)
}

fn post_note(ctx: &Ctx, thread_id: &str, session_id: &str, kp: &axel_identity::Keypair, text: &str) -> Result<()> {
    let owner_pk = ctx.owner()?.public().to_string();
    let env = EnvelopeBuilder::new(kind::NOTE, text).thread(thread_id).session(session_id).mention(&owner_pk).sign(kp)?;
    ctx.store.post(&env)?;
    Ok(())
}

fn wait_for_reply(ctx: &Ctx, request_id: &str, timeout: Duration) -> Result<Option<&'static str>> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        for m in ctx.store.replies_to(request_id)? {
            if m.kind == kind::PERMISSION_REPLY {
                return Ok(Some(if m.content.trim() == "allow" { "allow" } else { "deny" }));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(None)
}

fn summarize_tool_input(v: &Value) -> String {
    for key in ["command", "file_path", "pattern", "url", "description"] {
        if let Some(s) = v.get(key).and_then(|s| s.as_str()) {
            let one = s.lines().next().unwrap_or("");
            return one.chars().take(80).collect();
        }
    }
    String::new()
}

/// `AskUserQuestion` input: `{questions:[{question, options:[{label,..}]}]}`.
fn summarize_question(v: &Value) -> String {
    let Some(qs) = v.get("questions").and_then(|q| q.as_array()) else { return "design question".into() };
    let mut out = Vec::new();
    for q in qs {
        let text = q.get("question").and_then(|t| t.as_str()).unwrap_or("?");
        let opts: Vec<&str> = q
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| o.iter().filter_map(|x| x.get("label").and_then(|l| l.as_str())).collect())
            .unwrap_or_default();
        if opts.is_empty() { out.push(text.to_string()) } else { out.push(format!("{text} [{}]", opts.join(" / "))) }
    }
    out.join(" | ")
}

#[cfg(test)]
mod turn_tests {
    #[test]
    fn the_last_turns_prose_comes_after_the_last_human_prompt() {
        let t = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"first\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old answer\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Let me look.\"},{\"type\":\"tool_use\",\"name\":\"Read\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"...\"}]}}\n",
            "not json\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done.\"}]}}\n",
        );
        assert_eq!(super::last_assistant_text(t), "Let me look.\nDone.");
        assert_eq!(super::last_assistant_text(""), "");
    }
}
