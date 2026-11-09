//! `axel thread manage`: the thread controls pane.  Loads what the pane
//! shows (sessions, spend, context, the last hour) and carries out what it
//! asks: reset to a fresh context, move to a host, settle, reopen, archive,
//! stop, rename.  Opened from the composer with Ctrl+D, or from a shell.

use std::collections::HashMap;

use anyhow::{Context, Result};
use axel_store::{SessionFilter, SessionState, StatusState, ThreadState};
use axel_tui::ThreadOp;
use axel_tui::manage::{ManageData, Outcome, SessionRow, run_manage};

use super::Ctx;

/// The `ended_reason` a hard reset leaves on a session: a summon that finds
/// it as the agent's last session on the thread starts fresh.
pub const HARD_RESET: &str = "hard reset";

pub fn run(thread: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let t = super::thread::resolve(&ctx, thread)?;
    let mut note: Option<String> = None;
    loop {
        let data = load(&ctx, &t.id, note.take())?;
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
fn act(ctx: &Ctx, thread_id: &str, outcome: &Outcome) -> Result<Option<String>> {
    Ok(Some(match outcome {
        Outcome::Close => return Ok(None),
        Outcome::Reset { agent } => {
            let text = recap(ctx, thread_id)?;
            let stopped = super::thread::stop_thread(ctx, thread_id)?;
            let session = super::ui::summon_with(ctx, thread_id, agent, &text, &[], true)?;
            // The thread's own panes come back on the installed binary too;
            // docking closes this pane, so nothing is left to report in it.
            if super::ui::relaunch_thread_panes(ctx, thread_id)? {
                return Ok(None);
            }
            format!("reset: {stopped} session{} ended, {agent} starts fresh ({})", if stopped == 1 { "" } else { "s" }, &session[..8.min(session.len())])
        }
        Outcome::Move { agent, host } => {
            let a = ctx.store.agent_by_name(agent)?.with_context(|| format!("no agent named {agent:?}"))?;
            // A live Claude session travels with its conversation; anything
            // else starts over there with a recap.
            let live = ctx
                .store
                .list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), agent_pubkey: Some(a.pubkey.clone()), ..Default::default() })?
                .into_iter()
                .find(|s| s.ended_at.is_none());
            match live {
                Some(s) if s.harness == axel_store::Harness::Claude && super::host::host_of(ctx, &s)?.is_none() => {
                    let (new_session, _) = super::host::move_session(ctx, &s.id, host)?;
                    format!("moved {agent} to {host} with its conversation ({})", &new_session[..8.min(new_session.len())])
                }
                _ => {
                    let text = recap(ctx, thread_id)?;
                    super::thread::stop_thread(ctx, thread_id)?;
                    let session = super::host::launch_remote(ctx, host, thread_id, agent, &text, &[])?;
                    format!("{agent} starts on {host} with a recap ({})", &session[..8.min(session.len())])
                }
            }
        }
        Outcome::Settle => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Settle)?;
            "settled".into()
        }
        Outcome::Reopen => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Reopen)?;
            "reopened".into()
        }
        Outcome::Archive => {
            super::thread::set_state(ctx, thread_id, ThreadOp::Archive)?;
            "archived".into()
        }
        Outcome::PauseAgent { agent } => {
            let n = pause_agent(ctx, thread_id, agent, true)?;
            format!("paused {agent}: {n} session(s)")
        }
        Outcome::ResumeAgent { agent } => {
            let n = pause_agent(ctx, thread_id, agent, false)?;
            format!("resumed {agent}: {n} session(s)")
        }
        Outcome::RemoveAgent { agent } => {
            let a = ctx.store.agent_by_name(agent)?.with_context(|| format!("no agent named {agent:?}"))?;
            super::thread::stop_agent(ctx, thread_id, agent)?;
            ctx.store.remove_participant(&ctx.owner()?, thread_id, &a.pubkey)?;
            format!("removed {agent} from this thread; history kept")
        }
        Outcome::StopAgent { agent } => {
            let n = super::thread::stop_agent(ctx, thread_id, agent)?;
            format!("stopped {agent}: {n} session{}", if n == 1 { "" } else { "s" })
        }
        Outcome::Stop => {
            let n = super::thread::stop_thread(ctx, thread_id)?;
            format!("stopped {n} session{}", if n == 1 { "" } else { "s" })
        }
        Outcome::Rename(title) => {
            ctx.store.update_thread(&ctx.owner()?, thread_id, Some(title), None)?;
            format!("renamed to {title}")
        }
        Outcome::HardReset => {
            // Ended as a hard reset: the next summon of these agents starts a
            // new conversation instead of resuming this one.
            let stopped = super::thread::stop_thread_with(ctx, thread_id, HARD_RESET)?;
            if super::ui::relaunch_thread_panes(ctx, thread_id)? {
                return Ok(None);
            }
            format!("hard reset: {stopped} session{} ended; the next message starts from scratch", if stopped == 1 { "" } else { "s" })
        }
    }))
}

fn pause_agent(ctx: &Ctx, thread_id: &str, name: &str, paused: bool) -> Result<usize> {
    let agent = ctx.store.agent_by_name(name)?.with_context(|| format!("no agent named {name:?}"))?;
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

/// What the pane shows, from the store.
fn load(ctx: &Ctx, thread_id: &str, note: Option<String>) -> Result<ManageData> {
    let now = chrono::Utc::now().timestamp();
    let summary = ctx.store.thread_summary(thread_id)?.context("no such thread")?;
    let t = &summary.thread;
    let agents: HashMap<String, axel_store::Agent> = ctx.store.list_agents()?.into_iter().map(|a| (a.pubkey.clone(), a)).collect();
    let roster = ctx.store.thread_agents(thread_id)?;
    let statuses: HashMap<String, axel_store::Status> = ctx.store.statuses()?.into_iter().map(|s| (s.session_id.clone(), s)).collect();
    let contexts: HashMap<String, (i64, i64)> = ctx.store.session_contexts()?.into_iter().map(|c| (c.session_id, (c.used, c.size))).collect();
    let turns = ctx.store.turns(thread_id)?;
    let mut sessions = ctx.store.list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), ..Default::default() })?;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    let local_env = ctx.store.local_environment()?.id;
    let mut rows = Vec::new();
    let mut agent_names: Vec<String> = Vec::new();
    let (mut tokens, mut cost, mut tool_calls, mut files) = (0i64, 0f64, 0i64, 0i64);
    let mut spark = vec![0i64; 60];
    for s in &sessions {
        let agent = agents.get(&s.agent_pubkey);
        let name = agent.map(|a| a.name.clone()).unwrap_or_else(|| s.agent_pubkey.chars().take(8).collect());
        if roster.contains(&s.agent_pubkey) && !agent_names.contains(&name) && !agent.is_some_and(|a| a.archived_at.is_some()) {
            agent_names.push(name.clone());
        }
        let usage = ctx.store.usage(&s.id)?;
        let (toks, usd) = usage.map(|u| (u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens, u.cost_usd_est)).unwrap_or((0, 0.0));
        let mine: Vec<_> = turns.iter().filter(|x| x.session_id == s.id).collect();
        let calls: i64 = mine.iter().map(|x| x.summary.tool_calls).sum();
        files += mine.iter().filter_map(|x| x.summary.changes.as_ref()).map(|c| c.files.len() as i64).sum::<i64>();
        for sample in ctx.store.usage_samples(Some(&s.id), now - 3600)? {
            let minute = ((sample.at - (now - 3600)) / 60).clamp(0, 59) as usize;
            spark[minute] += sample.input_tokens + sample.output_tokens;
        }
        let live = s.ended_at.is_none();
        let state = match (live, statuses.get(&s.id)) {
            (false, _) => "ended".to_string(),
            (true, _) if s.state == SessionState::Paused => "paused".into(),
            (true, Some(st)) => match st.state {
                StatusState::Working => "working",
                StatusState::Blocked => "blocked",
                StatusState::Done => "done",
                StatusState::Error => "error",
                StatusState::Idle => "idle",
            }
            .to_string(),
            (true, None) => match s.state {
                SessionState::Running => "working",
                SessionState::Blocked => "blocked",
                _ => "idle",
            }
            .to_string(),
        };
        let host = (s.environment_id != local_env).then(|| ctx.store.environment(&s.environment_id).ok().flatten().map_or_else(|| s.environment_label.clone(), |e| e.label));
        let runs = match agent.and_then(|a| a.model.clone()) {
            Some(m) => format!("{}/{m}", s.harness.as_str()),
            None => s.harness.as_str().to_string(),
        };
        tokens += toks;
        cost += usd;
        tool_calls += calls;
        if roster.contains(&s.agent_pubkey) { rows.push(SessionRow {
            agent: name,
            color: agent.and_then(|a| a.color),
            runs,
            state,
            live,
            host,
            turns: mine.len() as i64,
            tool_calls: calls,
            tokens: toks,
            cost_usd: usd,
            context: contexts.get(&s.id).copied(),
            duration_secs: s.ended_at.unwrap_or(now) - s.started_at,
        }); }
    }
    for pk in &roster {
        if let Some(agent) = agents.get(pk)
            && agent.archived_at.is_none() && !agent_names.contains(&agent.name)
        { agent_names.push(agent.name.clone()); }
    }
    let state = match t.state {
        ThreadState::Open => "open",
        ThreadState::Done => "done",
        ThreadState::Archived => "archived",
        ThreadState::Deleted => "deleted",
    };
    Ok(ManageData {
        thread_id: thread_id.to_string(),
        title: t.title.clone(),
        state: state.into(),
        age: axel_tui::manage::duration_label(now - t.created_at),
        projects: summary.projects.iter().map(|p| p.path_hint.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.path_hint.display().to_string())).collect(),
        sessions: rows,
        spark,
        tokens,
        cost_usd: cost,
        turns: turns.len() as i64,
        tool_calls,
        files_changed: files,
        agents: agent_names,
        hosts: ctx.store.hosts()?.into_iter().map(|h| h.label).collect(),
        note,
    })
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
                model: None, skills: vec![], prompt: None, color: None, runtime_mode: RuntimeMode::default(),
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
        assert!(!load(&ctx, &thread.id, None).unwrap().agents.contains(&"chosen".into()));
        assert!(ctx.store.agent_by_name("chosen").unwrap().is_some());
        assert!(ctx.store.get_session(&a.id).unwrap().is_some());
        assert!(ctx.store.thread_agents(&other.id).unwrap().contains(&chosen.public().to_base58()));
        assert!(ctx.store.get_session(&untouched_peer.id).unwrap().unwrap().ended_at.is_none());
    }
}
