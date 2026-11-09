//! `axel thread …` — the thread is the primitive.

use anyhow::{Context, Result, bail};
use axel_store::{SessionFilter, Thread, ThreadFilter, ThreadState, ThreadWorkspace, WorkspaceKind};
use axel_tui::{LaunchRequest, ThreadOp};

use super::{Ctx, launch};
use crate::cli::{RuntimeArg, ThreadCommand};
use axel_store::SessionRuntime;

pub fn run(cmd: ThreadCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    match cmd {
        ThreadCommand::New { title, agent, projects, scratch, worktree, no_launch, runtime, host } => {
            let title = title.join(" ");
            if title.trim().is_empty() {
                bail!("give the thread a title: what are we building?");
            }
            let a = ctx.store.agent_by_name(&agent)?.with_context(|| format!("no agent named {agent:?}"))?;
            let projects = if scratch { vec![] } else if projects.is_empty() { vec![std::env::current_dir()?] } else { projects };
            let (workspace, branch) = workspace_args(worktree);
            if no_launch {
                let t = record_thread(&ctx, &title, &projects)?;
                println!("{}", t.id);
                return Ok(());
            }
            if let super::host::Placement::Host(h) = super::host::placement(&ctx, &a.pubkey, host.as_deref())? {
                let (thread, session) = super::host::new_thread_remote(&ctx, &h.label, &title, &projects, &a.name, &[])?;
                println!("thread {thread}\nsession {session} on {}\nopen the thread: axel thread open {}", h.label, &thread[..12]);
                return Ok(());
            }
            let req = LaunchRequest { title, projects, agent_pubkey: a.pubkey, workspace, branch, thread_id: None, attachments: vec![] };
            let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: runtime.map(rt), prompt: None, attachments: vec![], work_dir: None, resume: None };
            report(launch::launch(&ctx, req, opts)?)
        }
        ThreadCommand::Spawn { thread, agent, prompt, runtime, host } => {
            let t = resolve(&ctx, &thread)?;
            let a = ctx.store.agent_by_name(&agent)?.with_context(|| format!("no agent named {agent:?}"))?;
            if let super::host::Placement::Host(h) = super::host::placement(&ctx, &a.pubkey, host.as_deref())? {
                let prompt = prompt.unwrap_or_else(|| t.intent.clone());
                let session = super::host::launch_remote(&ctx, &h.label, &t.id, &a.name, &prompt, &[])?;
                println!("thread {}\nsession {session} on {}", t.id, h.label);
                return Ok(());
            }
            let req = LaunchRequest { title: t.title.clone(), projects: vec![], agent_pubkey: a.pubkey, workspace: WorkspaceKind::InPlace, branch: None, thread_id: Some(t.id), attachments: vec![] };
            let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: runtime.map(rt), prompt, attachments: vec![], work_dir: None, resume: None };
            report(launch::launch(&ctx, req, opts)?)
        }
        ThreadCommand::Split { thread } => {
            let t = resolve(&ctx, &thread)?;
            drop(ctx);
            super::ui::open_split(&t.id)
        }
        ThreadCommand::Stream { thread } => {
            let t = resolve(&ctx, &thread)?;
            drop(ctx);
            super::ui::run_stream(&t.id)
        }
        ThreadCommand::Compose { thread } => {
            let t = resolve(&ctx, &thread)?;
            drop(ctx);
            super::ui::run_compose(&t.id)
        }
        ThreadCommand::Tools { thread, fold } => {
            drop(ctx);
            super::links::show_tools(&thread, &fold)
        }
        ThreadCommand::Diff { thread, session, turn, stat, pane } => {
            drop(ctx);
            super::diff::run(&thread, session.as_deref(), turn, stat, pane)
        }
        ThreadCommand::Manage { thread } => {
            drop(ctx);
            super::manage::run(&thread)
        }
        ThreadCommand::Refresh { thread } => {
            let t = resolve(&ctx, &thread)?;
            if !super::ui::relaunch_thread_panes(&ctx, &t.id)? {
                bail!("this thread has no open views here");
            }
            Ok(())
        }
        ThreadCommand::Undo { thread, session, turn, restore, yes } => {
            drop(ctx);
            super::undo::run(&thread, session.as_deref(), turn, restore, yes)
        }
        ThreadCommand::Answer { thread, item } => {
            let t = resolve(&ctx, &thread)?;
            drop(ctx);
            super::ui::run_answer(&t.id, &item)
        }
        ThreadCommand::Open { thread } => {
            let t = resolve(&ctx, &thread)?;
            drop(ctx);
            super::ui::open_here(&t.id)
        }
        ThreadCommand::Resume { thread, agent } => {
            let t = resolve(&ctx, &thread)?;
            let last = ctx
                .store
                .list_sessions(SessionFilter { thread_id: Some(t.id.clone()), limit: Some(1), ..Default::default() })?
                .into_iter()
                .next()
                .context("this thread has no session to resume")?;
            let agent_pk = match agent {
                Some(n) => ctx.store.agent_by_name(&n)?.with_context(|| format!("no agent named {n:?}"))?.pubkey,
                None => last.agent_pubkey.clone(),
            };
            // Resuming is a spawn whose harness gets `--resume <id>`; the launch
            // code reads AXEL_RESUME from the request's environment hint.
            if let Some(hs) = &last.harness_session_id {
                // SAFETY: single-threaded CLI; set before launch reads it.
                unsafe { std::env::set_var("AXEL_RESUME", hs) };
            } else {
                eprintln!("axel: last session has no harness session id yet; starting fresh on the same thread");
            }
            let req = LaunchRequest { title: t.title.clone(), projects: vec![], agent_pubkey: agent_pk, workspace: WorkspaceKind::InPlace, branch: None, thread_id: Some(t.id), attachments: vec![] };
            report(launch::launch(&ctx, req, launch::LaunchOptions::default())?)
        }
        ThreadCommand::Attach { thread, project, worktree } => {
            let t = resolve(&ctx, &thread)?;
            let owner = ctx.owner()?;
            let env = ctx.store.local_environment()?;
            let path = std::fs::canonicalize(&project).unwrap_or(project);
            let key = launch::repo_key(&path);
            ctx.store.upsert_project(&env.id, &path, key.as_deref())?;
            let (kind, branch) = workspace_args(worktree);
            let ws = match kind {
                WorkspaceKind::Worktree => {
                    let repo = axel_config::git::repo_root(&path).context("not a git repository")?;
                    let branch = branch.unwrap_or_else(|| format!("axel/{}", axel_tui::slugify(&t.title)));
                    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
                    let dest = crate::paths::worktrees_dir().join(name).join(branch.replace('/', "-"));
                    let wt = axel_config::git::ensure_worktree(&repo, &branch, &dest)?;
                    ThreadWorkspace { kind, branch: Some(branch), worktree_path: Some(wt) }
                }
                _ => ThreadWorkspace { kind: WorkspaceKind::InPlace, branch: axel_config::git::current_branch(&path), worktree_path: None },
            };
            ctx.store.attach_project(&owner, &t.id, key.as_deref(), &path, &ws)?;
            println!("attached {} to {}", path.display(), t.title);
            Ok(())
        }
        ThreadCommand::Ls { json, all } => {
            super::reconcile::run(&ctx).ok();
            let states = if all { vec![ThreadState::Open, ThreadState::Done, ThreadState::Archived] } else { vec![ThreadState::Open] };
            let states = Some(states);
            let rows = ctx.store.thread_summaries(ThreadFilter { states, limit: Some(100), ..Default::default() })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            let agents = ctx.store.list_agents()?;
            for r in rows {
                let names: Vec<String> = r.agents.iter().map(|pk| agents.iter().find(|a| &a.pubkey == pk).map(|a| a.name.clone()).unwrap_or_else(|| pk[..8].to_string())).collect();
                let live = r.live.as_ref().map(|s| s.state.as_str()).unwrap_or("-");
                let projects: Vec<String> = r.projects.iter().map(|p| p.path_hint.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
                println!(
                    "{}  {:<8} {:<8} {:<20} {:<24} {:>2}s {:>7}  {}",
                    &r.thread.id[..12],
                    r.thread.state,
                    live,
                    truncate(&names.join(","), 20),
                    truncate(&if projects.is_empty() { "scratch".into() } else { projects.join(",") }, 24),
                    r.sessions_total,
                    fmt_tokens(r.usage.input_tokens + r.usage.output_tokens),
                    r.thread.title
                );
            }
            Ok(())
        }
        ThreadCommand::Show { thread } => {
            let t = resolve(&ctx, &thread)?;
            let s = ctx.store.thread_summary(&t.id)?.context("thread vanished")?;
            println!("{}", serde_json::to_string_pretty(&s)?);
            let sessions = ctx.store.list_sessions(SessionFilter { thread_id: Some(t.id.clone()), ..Default::default() })?;
            println!("sessions:");
            for se in sessions {
                println!("  {}  {:<8} agent {}  env {}  harness {}  resume {}", se.id, se.state, &se.agent_pubkey[..8], se.environment_label, se.harness, se.harness_session_id.as_deref().unwrap_or("-"));
            }
            let msgs = ctx.store.thread_messages(&t.id)?;
            println!("messages: {}", msgs.len());
            for m in msgs.iter().rev().take(10).rev() {
                println!("  {:<18} {}", axel_inbox::kind::name(m.kind), m.content.lines().next().unwrap_or(""));
            }
            Ok(())
        }
        ThreadCommand::Stop { thread } => {
            let t = resolve(&ctx, &thread)?;
            let n = stop_thread(&ctx, &t.id)?;
            println!("ended {n} session(s) on {}", t.title);
            Ok(())
        }
        ThreadCommand::Done { thread } => {
            let t = resolve(&ctx, &thread)?;
            set_state(&ctx, &t.id, ThreadOp::Settle)?;
            println!("settled: {}", t.title);
            Ok(())
        }
        ThreadCommand::Reopen { thread } => {
            let t = resolve(&ctx, &thread)?;
            set_state(&ctx, &t.id, ThreadOp::Reopen)?;
            println!("reopened: {}", t.title);
            Ok(())
        }
        ThreadCommand::Archive { thread } => {
            let t = resolve(&ctx, &thread)?;
            set_state(&ctx, &t.id, ThreadOp::Archive)?;
            println!("archived: {}", t.title);
            Ok(())
        }
        ThreadCommand::Delete { thread } => {
            let t = resolve(&ctx, &thread)?;
            set_state(&ctx, &t.id, ThreadOp::Delete)?;
            println!("deleted: {}", t.title);
            Ok(())
        }
    }
}

/// End every live session on a thread (closing their panes; remote ones
/// through their host).  Returns how many ended.
pub fn stop_thread(ctx: &Ctx, thread_id: &str) -> Result<usize> {
    stop_thread_with(ctx, thread_id, "stopped by owner")
}

/// [`stop_thread`], recording `reason` on each session it ends.
pub fn stop_thread_with(ctx: &Ctx, thread_id: &str, reason: &str) -> Result<usize> {
    stop_sessions(ctx, thread_id, None, reason)
}

/// Stop one agent on this thread, preserving every other agent and thread.
pub fn stop_agent(ctx: &Ctx, thread_id: &str, agent: &str) -> Result<usize> {
    let agent = ctx.store.agent_by_name(agent)?.with_context(|| format!("no agent named {agent:?}"))?;
    stop_sessions(ctx, thread_id, Some(&agent.pubkey), "stopped by owner")
}

fn stop_sessions(ctx: &Ctx, thread_id: &str, agent_pubkey: Option<&str>, reason: &str) -> Result<usize> {
    let owner = ctx.owner()?;
    let live = ctx.store.list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), agent_pubkey: agent_pubkey.map(str::to_owned), states: Some(vec![axel_store::SessionState::Starting, axel_store::SessionState::Running, axel_store::SessionState::Blocked, axel_store::SessionState::Idle]), ..Default::default() })?;
    let mut hosts = Vec::new();
    for s in &live {
        if let Some(h) = super::host::host_of(ctx, s)? {
            // A remote session: its host stops it; the end reaches it at sync.
            if let Err(e) = super::host::stop_remote(ctx, s) {
                eprintln!("{}: couldn't reach it to stop the session ({e:#}); it ends there at the next sync", h.label);
            }
            ctx.store.end_session(&owner, &s.id, Some(reason))?;
            hosts.push(h);
            continue;
        }
        // Hosts without a pane (detached, or started outside Rex) only stop when asked.
        if let Some(sock) = &s.socket_path {
            let _ = axel_acp::socket::send(sock, &axel_acp::socket::Command::Stop);
        }
        ctx.store.end_session(&owner, &s.id, Some(reason))?;
        if let (Some(rex), Some(block)) = (&ctx.rex, s.rex_block_id.as_deref()) {
            rex.api_call("block.close", serde_json::json!({"block_id": block})).ok();
        }
    }
    for h in &hosts {
        let _ = super::host::sync_host(ctx, h, true);
    }
    Ok(live.len())
}

/// Settle, reopen, archive or delete a thread.  Archiving and deleting stop
/// its sessions first; settling leaves them running.
pub fn set_state(ctx: &Ctx, thread_id: &str, op: ThreadOp) -> Result<()> {
    let state = match op {
        ThreadOp::Settle => ThreadState::Done,
        ThreadOp::Reopen => ThreadState::Open,
        ThreadOp::Archive => ThreadState::Archived,
        ThreadOp::Delete => ThreadState::Deleted,
    };
    if matches!(op, ThreadOp::Archive | ThreadOp::Delete) {
        stop_thread(ctx, thread_id)?;
    }
    ctx.store.update_thread(&ctx.owner()?, thread_id, None, Some(state))?;
    if matches!(op, ThreadOp::Archive | ThreadOp::Delete) {
        // Old checkpoints go with it (this thread's a week from now; others due).
        let _ = super::gc::run(ctx, true);
    }
    Ok(())
}

/// Create a thread here with its projects (worked on in place), without
/// starting anyone on it.
pub fn record_thread(ctx: &Ctx, title: &str, projects: &[std::path::PathBuf]) -> Result<Thread> {
    let owner = ctx.owner()?;
    let env = ctx.store.local_environment()?;
    let t = ctx.store.create_thread(&owner, &axel_tui::util::title_line(title), title, None)?;
    for p in projects {
        let path = std::fs::canonicalize(p).unwrap_or(p.clone());
        let key = launch::repo_key(&path);
        ctx.store.upsert_project(&env.id, &path, key.as_deref())?;
        let ws = ThreadWorkspace { kind: WorkspaceKind::InPlace, branch: axel_config::git::current_branch(&path), worktree_path: None };
        ctx.store.attach_project(&owner, &t.id, key.as_deref(), &path, &ws)?;
    }
    Ok(t)
}

/// Accept a full id or a unique prefix.
pub(super) fn resolve(ctx: &Ctx, id_or_prefix: &str) -> Result<Thread> {
    if let Some(t) = ctx.store.get_thread(id_or_prefix)? {
        return Ok(t);
    }
    let all = ctx.store.list_threads(ThreadFilter::default())?;
    let matches: Vec<Thread> = all.into_iter().filter(|t| t.id.starts_with(id_or_prefix) || t.title.to_lowercase().contains(&id_or_prefix.to_lowercase())).collect();
    match matches.len() {
        0 => bail!("no thread matches {id_or_prefix}"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => bail!("{n} threads match {id_or_prefix}; be more specific"),
    }
}

fn workspace_args(worktree: Option<String>) -> (WorkspaceKind, Option<String>) {
    match worktree {
        None => (WorkspaceKind::InPlace, None),
        Some(b) if b.is_empty() => (WorkspaceKind::Worktree, None),
        Some(b) => (WorkspaceKind::Worktree, Some(b)),
    }
}

fn rt(r: RuntimeArg) -> SessionRuntime {
    match r {
        RuntimeArg::Managed => SessionRuntime::Managed,
        RuntimeArg::Pane => SessionRuntime::Pane,
    }
}

fn report(l: launch::Launched) -> Result<()> {
    match l {
        launch::Launched::Managed { thread_id, session_id, host_window, host_spec } => {
            println!("thread {thread_id}\nsession {session_id}");
            match (host_window, host_spec) {
                (Some(w), _) => {
                    println!("host in Rex window {w}\nopen the thread: axel thread open {}", &thread_id[..12]);
                    Ok(())
                }
                (None, Some(spec)) => {
                    eprintln!("hosting here; open the thread elsewhere: axel thread open {}", &thread_id[..12]);
                    launch::exec_inline(spec)
                }
                (None, None) => Ok(()),
            }
        }
        launch::Launched::RexWindow { window_id, thread_id, session_id } => {
            println!("thread {thread_id}\nsession {session_id}\nrex window {window_id}");
            Ok(())
        }
        launch::Launched::Inline { spec, thread_id, session_id, .. } => {
            eprintln!("thread {thread_id}\nsession {session_id}");
            launch::exec_inline(spec)
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else if n >= 1000 { format!("{:.1}k", n as f64 / 1e3) } else { n.to_string() }
}
