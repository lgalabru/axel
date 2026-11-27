//! `axel thread manage`: the thread controls pane.  Loads what the pane
//! shows (sessions, spend, context, the last hour) and carries out what it
//! asks: reset to a fresh context, move to a host, settle, reopen, archive,
//! stop, rename.  Opened from the composer with Ctrl+D, or from a shell.

use std::collections::HashMap;

use anyhow::{Context, Result};
use axel_i18n::t;
use axel_store::{SessionFilter, SessionState, StatusState};
use axel_tui::ThreadOp;
use axel_tui::manage::{ManageData, Outcome, SessionRow, SessionStatus, run_manage};

use super::Ctx;

/// The `ended_reason` a hard reset leaves on a session: a summon that finds
/// it as the agent's last session on the thread starts fresh.
pub const HARD_RESET: &str = "hard reset";

pub fn run(thread: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let t = super::thread::resolve(&ctx, thread)?;
    let mut note: Option<String> = None;
    loop {
        let data = load(&ctx.store, &t.id, note.take())?;
        let outcome = run_manage(data)?;
        note = Some(match act(&ctx, &t.id, &outcome) {
            Ok(None) => break,
            Ok(Some(n)) => n,
            Err(e) => format!("{e:#}"),
        });
        if matches!(outcome, Outcome::Archive) {
            break;
        }
    }
    super::ui::close_own_layer(&ctx);
    Ok(())
}

/// Carry out an outcome; the footer line to show, or `None` to close.
pub(super) fn act(ctx: &Ctx, thread_id: &str, outcome: &Outcome) -> Result<Option<String>> {
    Ok(Some(match outcome {
        Outcome::Close => return Ok(None),
        Outcome::Reset { agent } => return reset(ctx, thread_id, agent),
        Outcome::Move { agent, host } => move_agent(ctx, thread_id, agent, host)?,
        Outcome::Settle => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Settle)?;
            t!("settled").into()
        }
        Outcome::Reopen => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Reopen)?;
            t!("reopened").into()
        }
        Outcome::Archive => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Archive)?;
            t!("archived").into()
        }
        Outcome::PauseAgent { agent } => {
            let n = pause_agent(ctx, thread_id, agent, true)?;
            t!("paused {agent}: {n} session(s)", agent = agent, n = n)
        }
        Outcome::ResumeAgent { agent } => {
            let n = pause_agent(ctx, thread_id, agent, false)?;
            t!("resumed {agent}: {n} session(s)", agent = agent, n = n)
        }
        Outcome::RemoveAgent { agent } => {
            let a = ctx.store.agent_by_name(agent)?.with_context(|| t!("no agent named {name}", name = format!("{agent:?}")))?;
            super::thread::stop_agent(ctx, thread_id, agent)?;
            ctx.store.remove_participant(&ctx.owner()?, thread_id, &a.pubkey)?;
            t!("removed {agent} from this thread; history kept", agent = agent)
        }
        Outcome::StopAgent { agent } => {
            let n = super::thread::stop_agent(ctx, thread_id, agent)?;
            if n == 1 { t!("stopped {agent}: 1 session", agent = agent) } else { t!("stopped {agent}: {n} sessions", agent = agent, n = n) }
        }
        Outcome::Stop => {
            let n = super::thread::stop_thread(ctx, thread_id)?;
            if n == 1 { t!("stopped 1 session").to_string() } else { t!("stopped {n} sessions", n = n) }
        }
        Outcome::Rename(title) => {
            ctx.store.update_thread(&ctx.owner()?, thread_id, Some(title), None)?;
            t!("renamed to {title}", title = title)
        }
        Outcome::HardReset => {
            // Ended as a hard reset: the next summon of these agents starts a
            // new conversation instead of resuming this one.
            let stopped = super::thread::stop_thread_with(ctx, thread_id, HARD_RESET)?;
            if super::ui::relaunch_thread_panes(ctx, thread_id)? {
                return Ok(None);
            }
            if stopped == 1 {
                t!("hard reset: 1 session ended; the next message starts from scratch").to_string()
            } else {
                t!("hard reset: {n} sessions ended; the next message starts from scratch", n = stopped)
            }
        }
    }))
}

/// Characters of a session id the footer shows.
const SESSION_ID_SHORT_CHARS: usize = 8;

fn short_session(id: &str) -> &str {
    id.get(..SESSION_ID_SHORT_CHARS).unwrap_or(id)
}

/// Stop the thread's sessions and start `agent` fresh with a recap.
fn reset(ctx: &Ctx, thread_id: &str, agent: &str) -> Result<Option<String>> {
    let text = recap(ctx, thread_id)?;
    let stopped = super::thread::stop_thread(ctx, thread_id)?;
    let session = super::ui::summon_with(ctx, thread_id, agent, &text, &[], true)?;
    // The thread's own panes come back on the installed binary too;
    // docking closes this pane, so nothing is left to report in it.
    if super::ui::relaunch_thread_panes(ctx, thread_id)? {
        return Ok(None);
    }
    let session = short_session(&session);
    Ok(Some(if stopped == 1 {
        t!("reset: 1 session ended, {agent} starts fresh ({session})", agent = agent, session = session)
    } else {
        t!("reset: {n} sessions ended, {agent} starts fresh ({session})", n = stopped, agent = agent, session = session)
    }))
}

/// Run `agent` on `host` from here on.  A live Claude session travels with
/// its conversation; anything else starts over there with a recap.
fn move_agent(ctx: &Ctx, thread_id: &str, agent: &str, host: &str) -> Result<String> {
    let a = ctx.store.agent_by_name(agent)?.with_context(|| t!("no agent named {name}", name = format!("{agent:?}")))?;
    let live = ctx
        .store
        .list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), agent_pubkey: Some(a.pubkey.clone()), ..Default::default() })?
        .into_iter()
        .find(|s| s.ended_at.is_none());
    Ok(match live {
        Some(s) if s.harness == axel_store::Harness::Claude && super::host::host_of(ctx, &s)?.is_none() => {
            let (new_session, _) = super::host::move_session(ctx, &s.id, host)?;
            t!("moved {agent} to {host} with its conversation ({session})", agent = agent, host = host, session = short_session(&new_session))
        }
        _ => {
            let text = recap(ctx, thread_id)?;
            super::thread::stop_thread(ctx, thread_id)?;
            let session = super::host::launch_remote(ctx, host, thread_id, agent, &text, &[])?;
            t!("{agent} starts on {host} with a recap ({session})", agent = agent, host = host, session = short_session(&session))
        }
    })
}

fn pause_agent(ctx: &Ctx, thread_id: &str, name: &str, paused: bool) -> Result<usize> {
    let agent = ctx.store.agent_by_name(name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
    let sessions = ctx.store.list_sessions(SessionFilter { thread_id: Some(thread_id.into()), agent_pubkey: Some(agent.pubkey), ..Default::default() })?;
    let mut changed = 0;
    for session in sessions.iter().filter(|s| s.ended_at.is_none() && (s.state == SessionState::Paused) != paused) {
        if let Some(host) = super::host::host_of(ctx, session)? {
            super::host::pause_remote(ctx, session, paused)?;
            super::host::sync_host(ctx, &host, true)?;
        } else if paused { super::process_control::pause(ctx, session)?; }
        else { super::process_control::resume(ctx, session)?; }
        changed += 1;
    }
    Ok(changed)
}

/// The first prompt of a fresh session: the intent, then the last turns in
/// a few lines each, so the agent knows where things stand without the
/// context that got there.
pub fn recap(ctx: &Ctx, thread_id: &str) -> Result<String> {
    let t = ctx.store.get_thread(thread_id)?.context("no such thread")?;
    let turns = ctx.store.turns(thread_id)?;
    let head = |s: &str, n: usize| -> String {
        let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > n { format!("{}…", one.chars().take(n).collect::<String>()) } else { one }
    };
    let mut out = format!(
        "Fresh start on this thread, with a new context. The thread: {}\n\nWhat was asked, and what you answered, over the last turns:\n",
        t.intent.trim()
    );
    let recent: Vec<_> = turns.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    if recent.is_empty() {
        out.push_str("(no turns yet)\n");
    }
    for turn in recent {
        if !turn.summary.user_text.trim().is_empty() {
            out.push_str(&format!("> {}\n", head(&turn.summary.user_text, 240)));
        }
        if !turn.summary.assistant_text.trim().is_empty() {
            out.push_str(&format!("< {}\n", head(&turn.summary.assistant_text, 400)));
        }
        if let Some(c) = &turn.summary.changes
            && !c.files.is_empty()
        {
            let names: Vec<&str> = c.files.iter().take(6).map(|f| f.path.as_str()).collect();
            out.push_str(&format!("  changed: {}{}\n", names.join(", "), if c.files.len() > 6 { ", …" } else { "" }));
        }
    }
    out.push_str("\nThe working tree is as the last turn left it: read what you need before changing it. Carry on from here; ask the owner if the next step isn't clear.");
    Ok(out)
}

/// Minutes of usage the pane's sparkline covers, one bar each.
const SPARK_MINUTES: usize = 60;

/// What the pane shows, from the store.
pub(super) fn load(store: &axel_store::Store, thread_id: &str, note: Option<String>) -> Result<ManageData> {
    let now = chrono::Utc::now().timestamp();
    let summary = store.thread_summary(thread_id)?.context(t!("no such thread"))?;
    let t = &summary.thread;
    let known = Known::load(store, now)?;
    let roster = store.thread_agents(thread_id)?;
    let turns = store.turns(thread_id)?;
    let mut sessions = store.list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), ..Default::default() })?;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    let mut rows = Vec::new();
    let mut agent_names: Vec<String> = Vec::new();
    let (mut tokens, mut cost, mut tool_calls, mut files) = (0i64, 0f64, 0i64, 0i64);
    let mut spark = vec![0i64; SPARK_MINUTES];
    for s in &sessions {
        let agent = known.agents.get(&s.agent_pubkey);
        let (row, files_changed) = session_row(store, &known, &turns, s)?;
        add_to_spark(store, &s.id, now, &mut spark)?;
        (tokens, cost, tool_calls, files) = (tokens + row.tokens, cost + row.cost_usd, tool_calls + row.tool_calls, files + files_changed);
        // Every session counts toward the totals; only the roster's are listed.
        if roster.contains(&s.agent_pubkey) {
            if !agent_names.contains(&row.agent) && !agent.is_some_and(|a| a.archived_at.is_some()) {
                agent_names.push(row.agent.clone());
            }
            rows.push(row);
        }
    }
    for pk in &roster {
        if let Some(agent) = known.agents.get(pk)
            && agent.archived_at.is_none() && !agent_names.contains(&agent.name)
        { agent_names.push(agent.name.clone()); }
    }
    debug_assert!(rows.len() <= sessions.len());
    Ok(ManageData {
        thread_id: thread_id.to_string(),
        title: t.title.clone(),
        state: t.state,
        age: axel_tui::manage::duration_label(now - t.created_at),
        projects: summary.projects.iter().map(|p| p.path_hint.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.path_hint.display().to_string())).collect(),
        sessions: rows,
        spark,
        tokens,
        cost_usd: cost,
        turns: count(turns.len()),
        tool_calls,
        files_changed: files,
        agents: agent_names,
        hosts: store.hosts()?.into_iter().map(|h| h.label).collect(),
        note,
    })
}

/// What every session row looks up, read once for the pane.
struct Known {
    agents: HashMap<String, axel_store::Agent>,
    statuses: HashMap<String, axel_store::Status>,
    contexts: HashMap<String, (i64, i64)>,
    local_env: String,
    now: i64,
}

impl Known {
    fn load(store: &axel_store::Store, now: i64) -> Result<Known> {
        Ok(Known {
            agents: store.list_agents()?.into_iter().map(|a| (a.pubkey.clone(), a)).collect(),
            statuses: store.statuses()?.into_iter().map(|s| (s.session_id.clone(), s)).collect(),
            contexts: store.session_contexts()?.into_iter().map(|c| (c.session_id, (c.used, c.size))).collect(),
            local_env: store.local_environment()?.id,
            now,
        })
    }
}

/// One session as the pane lists it, and the files its turns changed.
fn session_row(store: &axel_store::Store, known: &Known, turns: &[axel_store::Turn], s: &axel_store::Session) -> Result<(SessionRow, i64)> {
    let agent = known.agents.get(&s.agent_pubkey);
    let name = agent.map(|a| a.name.clone()).unwrap_or_else(|| s.agent_pubkey.chars().take(8).collect());
    let usage = store.usage(&s.id)?;
    let (tokens, cost_usd) = usage.map(|u| (u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens, u.cost_usd_est)).unwrap_or((0, 0.0));
    let mine: Vec<_> = turns.iter().filter(|x| x.session_id == s.id).collect();
    let files = mine.iter().filter_map(|x| x.summary.changes.as_ref()).map(|c| count(c.files.len())).sum::<i64>();
    let host = (s.environment_id != known.local_env)
        .then(|| store.environment(&s.environment_id).ok().flatten().map_or_else(|| s.environment_label.clone(), |e| e.label));
    let runs = match agent.and_then(|a| a.model.clone()) {
        Some(m) => format!("{}/{m}", s.harness.as_str()),
        None => s.harness.as_str().to_string(),
    };
    let row = SessionRow {
        agent: name,
        color: agent.and_then(|a| a.color),
        runs,
        state: session_status(s, known.statuses.get(&s.id)),
        live: s.ended_at.is_none(),
        host,
        turns: count(mine.len()),
        tool_calls: mine.iter().map(|x| x.summary.tool_calls).sum(),
        tokens,
        cost_usd,
        context: known.contexts.get(&s.id).copied(),
        duration_secs: s.ended_at.unwrap_or(known.now) - s.started_at,
    };
    Ok((row, files))
}

/// A count as the pane's integers hold it.
fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Add the session's tokens of the last hour to `spark`, a bar a minute.
fn add_to_spark(store: &axel_store::Store, session_id: &str, now: i64, spark: &mut [i64]) -> Result<()> {
    assert_eq!(spark.len(), SPARK_MINUTES);
    let since = now - 60 * i64::try_from(SPARK_MINUTES).expect("a small constant");
    for sample in store.usage_samples(Some(session_id), since)? {
        let minute = usize::try_from((sample.at - since) / 60).unwrap_or(0).min(SPARK_MINUTES - 1);
        spark[minute] = spark[minute].saturating_add(sample.input_tokens.saturating_add(sample.output_tokens));
    }
    Ok(())
}

/// What a session is doing, as the pane shows it.
fn session_status(s: &axel_store::Session, status: Option<&axel_store::Status>) -> SessionStatus {
    if s.ended_at.is_some() {
        return SessionStatus::Ended;
    }
    if s.state == SessionState::Paused {
        return SessionStatus::Paused;
    }
    match status {
        Some(st) => match st.state {
            StatusState::Working => SessionStatus::Working,
            StatusState::Blocked => SessionStatus::Blocked,
            StatusState::Done => SessionStatus::Done,
            StatusState::Error => SessionStatus::Error,
            StatusState::Idle => SessionStatus::Idle,
        },
        None => match s.state {
            SessionState::Running => SessionStatus::Working,
            SessionState::Blocked => SessionStatus::Blocked,
            SessionState::Starting | SessionState::Idle | SessionState::Paused | SessionState::Ended => SessionStatus::Idle,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axel_identity::{Keypair, Keystore, attest};
    use axel_store::{Harness, NewAgent, NewSession, RuntimeMode, SessionRuntime, Store};

    #[test]
    fn stopping_one_agent_preserves_other_agents_and_other_threads() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = Ctx { store: Store::open_memory().unwrap(), keystore: Keystore::open_at(temp.path().join("keys.json"), false).unwrap(), rex: None };
        let owner = ctx.owner().unwrap();
        let register = |name: &str| {
            let key = Keypair::generate();
            ctx.store.insert_agent(&NewAgent {
                pubkey: key.public().to_base58(), name: name.into(), harness: Harness::Claude,
                model: None, effort: None, skills: vec![], prompt: None, color: None, runtime_mode: RuntimeMode::default(),
                auth_tag: Some(attest(&owner, &key.public(), "").unwrap().to_tag()),
            }).unwrap();
            key
        };
        let chosen = register("chosen");
        let peer = register("peer");
        let thread = ctx.store.create_thread(&owner, "here", "here", None).unwrap();
        let other = ctx.store.create_thread(&owner, "elsewhere", "elsewhere", None).unwrap();
        let environment = ctx.store.local_environment().unwrap();
        let start = |thread: &str, agent: &Keypair| ctx.store.start_session(agent, &NewSession {
            thread_id: thread.into(), agent_pubkey: agent.public().to_base58(),
            environment_id: environment.id.clone(), environment_label: environment.label.clone(),
            project_repo_key: None, harness: Harness::Claude, runtime_mode: RuntimeMode::default(),
            resumed_from: None, runtime: SessionRuntime::Managed,
        }).unwrap();
        let a = start(&thread.id, &chosen);
        let b = start(&thread.id, &chosen);
        let untouched_peer = start(&thread.id, &peer);
        let untouched_thread = start(&other.id, &chosen);
        let outcome = Outcome::StopAgent { agent: "chosen".into() };
        assert_eq!(act(&ctx, &thread.id, &outcome).unwrap(), Some("stopped chosen: 2 sessions".into()));
        for id in [&a.id, &b.id] {
            let session = ctx.store.get_session(id).unwrap().unwrap();
            assert!(session.ended_at.is_some());
            assert_eq!(session.ended_reason.as_deref(), Some("stopped by owner"));
        }
        for id in [&untouched_peer.id, &untouched_thread.id] {
            assert!(ctx.store.get_session(id).unwrap().unwrap().ended_at.is_none());
        }
        assert!(ctx.store.agent_by_name("chosen").unwrap().is_some());
        assert_eq!(act(&ctx, &thread.id, &outcome).unwrap(), Some("stopped chosen: 0 sessions".into()));
        act(&ctx, &thread.id, &Outcome::RemoveAgent { agent: "chosen".into() }).unwrap();
        assert!(!load(&ctx.store, &thread.id, None).unwrap().agents.contains(&"chosen".into()));
        assert!(ctx.store.agent_by_name("chosen").unwrap().is_some());
        assert!(ctx.store.get_session(&a.id).unwrap().is_some());
        assert!(ctx.store.thread_agents(&other.id).unwrap().contains(&chosen.public().to_base58()));
        assert!(ctx.store.get_session(&untouched_peer.id).unwrap().unwrap().ended_at.is_none());
    }
}
