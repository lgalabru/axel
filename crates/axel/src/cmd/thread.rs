//! `axel thread …` — the thread is the primitive.

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_store::{SessionFilter, Thread, ThreadFilter, ThreadState, ThreadWorkspace, WorkspaceKind};
use axel_tui::{LaunchRequest, ThreadOp};

use super::text::{fit, pad, pad_left};
use super::{Ctx, launch};
use crate::cli::{RuntimeArg, ThreadCommand};
use axel_store::SessionRuntime;

pub fn run(cmd: ThreadCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    match cmd {
        ThreadCommand::New { title, agent, projects, scratch, worktree, no_launch, runtime, host } => {
            new_thread(&ctx, NewThread { title: title.join(" "), agent, projects, scratch, worktree, no_launch, runtime, host })
        }
        ThreadCommand::Spawn { thread, agent, prompt, runtime, host } => spawn(&ctx, &thread, &agent, prompt, runtime, host.as_deref()),
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
                bail!("{}", t!("this thread has no open views here"));
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
        ThreadCommand::Resume { thread, agent } => resume(&ctx, &thread, agent),
        ThreadCommand::Attach { thread, project, worktree } => attach(&ctx, &thread, project, worktree),
        ThreadCommand::Ls { json, all } => list(&ctx, json, all),
        ThreadCommand::Show { thread } => show(&ctx, &thread),
        ThreadCommand::Stop { thread } => {
            let t = resolve(&ctx, &thread)?;
            let n = stop_thread(&ctx, &t.id)?;
            println!("{}", t!("ended {n} session(s) on {title}", n = n, title = t.title));
            Ok(())
        }
        ThreadCommand::Done { thread } => change_state(&ctx, &thread, ThreadOp::Settle),
        ThreadCommand::Reopen { thread } => change_state(&ctx, &thread, ThreadOp::Reopen),
        ThreadCommand::Archive { thread } => change_state(&ctx, &thread, ThreadOp::Archive),
        ThreadCommand::Delete { thread } => change_state(&ctx, &thread, ThreadOp::Delete),
    }
}

/// Characters of a thread id shown where a short one is enough to find it.
const THREAD_ID_SHORT_CHARS: usize = 12;
/// Characters of an agent's pubkey shown when its name is unknown.
const PUBKEY_SHORT_CHARS: usize = 8;
/// Most threads `axel thread ls` lists.
const LIST_THREADS_MAX: usize = 100;
/// Most recent messages `axel thread show` prints.
const SHOW_MESSAGES_MAX: usize = 10;

/// The first `chars` characters of an id (all of it when shorter).
fn short_id(id: &str, chars: usize) -> &str {
    id.char_indices().nth(chars).map_or(id, |(end, _)| &id[..end])
}

/// `axel thread new`'s arguments.
struct NewThread {
    title: String,
    agent: String,
    projects: Vec<std::path::PathBuf>,
    scratch: bool,
    worktree: Option<String>,
    no_launch: bool,
    runtime: Option<RuntimeArg>,
    host: Option<String>,
}

fn new_thread(ctx: &Ctx, args: NewThread) -> Result<()> {
    let NewThread { title, agent, projects, scratch, worktree, no_launch, runtime, host } = args;
    if title.trim().is_empty() {
        bail!("{}", t!("give the thread a title: what are we building?"));
    }
    let a = ctx.store.agent_by_name(&agent)?.with_context(|| t!("no agent named {name}", name = format!("{agent:?}")))?;
    let projects = if scratch { vec![] } else if projects.is_empty() { vec![std::env::current_dir()?] } else { projects };
    let (workspace, branch) = workspace_args(worktree);
    if no_launch {
        let t = record_thread(ctx, &title, &projects)?;
        println!("{}", t.id);
        return Ok(());
    }
    if let super::host::Placement::Host(h) = super::host::placement(ctx, &a.pubkey, host.as_deref())? {
        let (thread, session) = super::host::new_thread_remote(ctx, &h.label, &title, &projects, &a.name, &[])?;
        println!("{}", t!("thread {thread}\nsession {session} on {host}\nopen the thread: axel thread open {short}", thread = thread, session = session, host = h.label, short = short_id(&thread, THREAD_ID_SHORT_CHARS)));
        return Ok(());
    }
    let req = LaunchRequest { title, projects, agent_pubkey: a.pubkey, workspace, branch, thread_id: None, attachments: vec![] };
    let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: runtime.map(rt), prompt: None, attachments: vec![], work_dir: None, resume: None };
    report(launch::launch(ctx, req, opts)?)
}

fn spawn(ctx: &Ctx, thread: &str, agent: &str, prompt: Option<String>, runtime: Option<RuntimeArg>, host: Option<&str>) -> Result<()> {
    let t = resolve(ctx, thread)?;
    let a = ctx.store.agent_by_name(agent)?.with_context(|| t!("no agent named {name}", name = format!("{agent:?}")))?;
    if let super::host::Placement::Host(h) = super::host::placement(ctx, &a.pubkey, host)? {
        let prompt = prompt.unwrap_or_else(|| t.intent.clone());
        let session = super::host::launch_remote(ctx, &h.label, &t.id, &a.name, &prompt, &[])?;
        println!("{}", t!("thread {thread}\nsession {session} on {host}", thread = t.id, session = session, host = h.label));
        return Ok(());
    }
    let req = LaunchRequest { title: t.title.clone(), projects: vec![], agent_pubkey: a.pubkey, workspace: WorkspaceKind::InPlace, branch: None, thread_id: Some(t.id), attachments: vec![] };
    let opts = launch::LaunchOptions { target: launch::Target::NewWindow, runtime: runtime.map(rt), prompt, attachments: vec![], work_dir: None, resume: None };
    report(launch::launch(ctx, req, opts)?)
}

fn resume(ctx: &Ctx, thread: &str, agent: Option<String>) -> Result<()> {
    let t = resolve(ctx, thread)?;
    let last = ctx
        .store
        .list_sessions(SessionFilter { thread_id: Some(t.id.clone()), limit: Some(1), ..Default::default() })?
        .into_iter()
        .next()
        .context(t!("this thread has no session to resume"))?;
    let agent_pk = match agent {
        Some(n) => ctx.store.agent_by_name(&n)?.with_context(|| t!("no agent named {name}", name = format!("{n:?}")))?.pubkey,
        None => last.agent_pubkey.clone(),
    };
    // Resuming is a spawn whose harness gets `--resume <id>`; the launch
    // code reads AXEL_RESUME from the request's environment hint.
    if let Some(hs) = &last.harness_session_id {
        // SAFETY: single-threaded CLI; set before launch reads it.
        unsafe { std::env::set_var("AXEL_RESUME", hs) };
    } else {
        eprintln!("axel: {}", t!("the last session has no harness session id yet; starting fresh on the same thread"));
    }
    let req = LaunchRequest { title: t.title.clone(), projects: vec![], agent_pubkey: agent_pk, workspace: WorkspaceKind::InPlace, branch: None, thread_id: Some(t.id), attachments: vec![] };
    report(launch::launch(ctx, req, launch::LaunchOptions::default())?)
}

fn attach(ctx: &Ctx, thread: &str, project: std::path::PathBuf, worktree: Option<String>) -> Result<()> {
    let t = resolve(ctx, thread)?;
    let owner = ctx.owner()?;
    let env = ctx.store.local_environment()?;
    // A path that can't be canonicalized is still the user's choice; keep it as given.
    let path = std::fs::canonicalize(&project).unwrap_or(project);
    let key = launch::repo_key(&path);
    ctx.store.upsert_project(&env.id, &path, key.as_deref())?;
    let (kind, branch) = workspace_args(worktree);
    let ws = match kind {
        WorkspaceKind::Worktree => {
            let repo = axel_config::git::repo_root(&path).context(t!("not a git repository"))?;
            let branch = branch.unwrap_or_else(|| format!("axel/{}", axel_tui::slugify(&t.title)));
            let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
            let dest = crate::paths::worktrees_dir().join(name).join(branch.replace('/', "-"));
            let wt = axel_config::git::ensure_worktree(&repo, &branch, &dest)?;
            ThreadWorkspace { kind, branch: Some(branch), worktree_path: Some(wt) }
        }
        _ => ThreadWorkspace { kind: WorkspaceKind::InPlace, branch: axel_config::git::current_branch(&path), worktree_path: None },
    };
    ctx.store.attach_project(&owner, &t.id, key.as_deref(), &path, &ws)?;
    println!("{}", t!("attached {path} to {title}", path = path.display(), title = t.title));
    Ok(())
}

fn list(ctx: &Ctx, json: bool, all: bool) -> Result<()> {
    // Reconciling only freshens session states; a stale row still lists.
    super::reconcile::run(ctx).ok();
    let states = if all { vec![ThreadState::Open, ThreadState::Done, ThreadState::Archived] } else { vec![ThreadState::Open] };
    let states = Some(states);
    let rows = ctx.store.thread_summaries(ThreadFilter { states, limit: Some(LIST_THREADS_MAX), ..Default::default() })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let agents = ctx.store.list_agents()?;
    for r in rows {
        let names: Vec<String> = r
            .agents
            .iter()
            .map(|pk| agents.iter().find(|a| &a.pubkey == pk).map(|a| a.name.clone()).unwrap_or_else(|| short_id(pk, PUBKEY_SHORT_CHARS).to_string()))
            .collect();
        let live = r.live.as_ref().map(|s| s.state.as_str()).unwrap_or("-");
        let projects: Vec<String> = r.projects.iter().map(|p| p.path_hint.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
        let places = if projects.is_empty() { t!("scratch").to_string() } else { projects.join(",") };
        // Padded by terminal columns: names and projects may be Chinese.
        println!(
            "{}  {} {} {} {} {}s {}  {}",
            short_id(&r.thread.id, THREAD_ID_SHORT_CHARS),
            pad(&r.thread.state.to_string(), 8),
            pad(live, 8),
            pad(&fit(&names.join(","), 20), 20),
            pad(&fit(&places, 24), 24),
            pad_left(&r.sessions_total.to_string(), 2),
            pad_left(&fmt_tokens(r.usage.input_tokens.saturating_add(r.usage.output_tokens)), 7),
            r.thread.title
        );
    }
    Ok(())
}

fn show(ctx: &Ctx, thread: &str) -> Result<()> {
    let t = resolve(ctx, thread)?;
    let s = ctx.store.thread_summary(&t.id)?.context(t!("the thread vanished"))?;
    println!("{}", serde_json::to_string_pretty(&s)?);
    let sessions = ctx.store.list_sessions(SessionFilter { thread_id: Some(t.id.clone()), ..Default::default() })?;
    println!("{}", t!("sessions:"));
    for se in sessions {
        let line = t!(
            "agent {agent}  env {env}  harness {harness}  resume {resume}",
            agent = short_id(&se.agent_pubkey, PUBKEY_SHORT_CHARS),
            env = se.environment_label,
            harness = se.harness,
            resume = se.harness_session_id.as_deref().unwrap_or("-")
        );
        println!("  {}  {} {line}", se.id, pad(&se.state.to_string(), 8));
    }
    let msgs = ctx.store.thread_messages(&t.id)?;
    println!("{}", t!("messages: {n}", n = msgs.len()));
    for m in msgs.iter().rev().take(SHOW_MESSAGES_MAX).rev() {
        println!("  {:<18} {}", axel_inbox::kind::name(m.kind), m.content.lines().next().unwrap_or(""));
    }
    Ok(())
}

/// Settle, reopen, archive or delete the thread, and say so.
fn change_state(ctx: &Ctx, thread: &str, op: ThreadOp) -> Result<()> {
    let t = resolve(ctx, thread)?;
    set_state(ctx, &t.id, op)?;
    let done = match op {
        ThreadOp::Settle => t!("settled: {title}", title = t.title),
        ThreadOp::Reopen => t!("reopened: {title}", title = t.title),
        ThreadOp::Archive => t!("archived: {title}", title = t.title),
        ThreadOp::Delete => t!("deleted: {title}", title = t.title),
    };
    println!("{done}");
    Ok(())
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
    let agent = ctx.store.agent_by_name(agent)?.with_context(|| t!("no agent named {name}", name = format!("{agent:?}")))?;
    stop_sessions(ctx, thread_id, Some(&agent.pubkey), "stopped by owner")
}

fn stop_sessions(ctx: &Ctx, thread_id: &str, agent_pubkey: Option<&str>, reason: &str) -> Result<usize> {
    let live = ctx.store.list_sessions(SessionFilter { thread_id: Some(thread_id.to_string()), agent_pubkey: agent_pubkey.map(str::to_owned), states: Some(vec![axel_store::SessionState::Starting, axel_store::SessionState::Running, axel_store::SessionState::Blocked, axel_store::SessionState::Idle, axel_store::SessionState::Paused]), ..Default::default() })?;
    end_sessions(ctx, &live, reason)
}

/// End each of `sessions`, recording `reason` (remote ones through their host).
pub fn end_sessions(ctx: &Ctx, live: &[axel_store::Session], reason: &str) -> Result<usize> {
    let owner = ctx.owner()?;
    let mut hosts = Vec::new();
    for s in live {
        if let Some(h) = super::host::host_of(ctx, s)? {
            // A remote session: its host stops it; the end reaches it at sync.
            if let Err(e) = super::host::stop_remote(ctx, s) {
                eprintln!("{}", t!("{host}: couldn't reach it to stop the session ({error}); it ends there at the next sync", host = h.label, error = format!("{e:#}")));
            }
            ctx.store.end_session(&owner, &s.id, Some(reason))?;
            hosts.push(h);
            continue;
        }
        // A frozen host cannot answer Stop until it and its tools wake up.
        if s.state == axel_store::SessionState::Paused { super::process_control::resume(ctx, s)?; }
        // Hosts without a pane (detached, or started outside Rex) only stop when asked.
        // A host that doesn't answer has already gone: ending the row is what counts.
        if let Some(sock) = &s.socket_path {
            let _ = axel_acp::socket::send(sock, &axel_acp::socket::Command::Stop);
        }
        ctx.store.end_session(&owner, &s.id, Some(reason))?;
        // The pane may already be closed by hand; nothing is left to close then.
        if let (Some(rex), Some(block)) = (&ctx.rex, s.rex_block_id.as_deref()) {
            rex.api_call("block.close", serde_json::json!({"block_id": block})).ok();
        }
    }
    // The sessions have ended here already; a host we can't reach now learns
    // of it at its next sync.
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
        // Collecting is housekeeping: what it misses now, the next run takes.
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
        let path = std::fs::canonicalize(p).unwrap_or(p.clone()); // kept as given when it can't be resolved
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
        0 => bail!("{}", t!("no thread matches {query}", query = id_or_prefix)),
        1 => Ok(matches.into_iter().next().expect("one match, checked by the arm")),
        n => bail!("{}", t!("{n} threads match {query}; be more specific", n = n, query = id_or_prefix)),
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
            println!("{}", t!("thread {thread}\nsession {session}", thread = thread_id, session = session_id));
            match (host_window, host_spec) {
                (Some(w), _) => {
                    println!("{}", t!("host in Rex window {window}\nopen the thread: axel thread open {short}", window = w, short = short_id(&thread_id, THREAD_ID_SHORT_CHARS)));
                    Ok(())
                }
                (None, Some(spec)) => {
                    eprintln!("{}", t!("hosting here; open the thread elsewhere: axel thread open {short}", short = short_id(&thread_id, THREAD_ID_SHORT_CHARS)));
                    launch::exec_inline(spec)
                }
                (None, None) => Ok(()),
            }
        }
        launch::Launched::RexWindow { window_id, thread_id, session_id } => {
            println!("{}", t!("thread {thread}\nsession {session}\nrex window {window}", thread = thread_id, session = session_id, window = window_id));
            Ok(())
        }
        launch::Launched::Inline { spec, thread_id, session_id, .. } => {
            eprintln!("{}", t!("thread {thread}\nsession {session}", thread = thread_id, session = session_id));
            launch::exec_inline(spec)
        }
    }
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else if n >= 1000 { format!("{:.1}k", n as f64 / 1e3) } else { n.to_string() }
}
