//! `axel session …` — hosting and controlling managed (ACP) sessions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_acp::HostConfig;
use axel_store::{SessionFilter, SessionRuntime, SessionState};

use super::{Ctx, claude_profile::ClaudeProfile};
use crate::cli::SessionCommand;
use crate::paths;

pub fn run(cmd: SessionCommand) -> Result<()> {
    match cmd {
        SessionCommand::Host { session } => host(&session),
        SessionCommand::Resume { session } => resume(&session),
        SessionCommand::Stop { session } => {
            let ctx = Ctx::open()?;
            let s = ctx.store.get_session(&session)?.with_context(|| t!("no session {id}", id = session))?;
            if s.state == SessionState::Paused { super::process_control::resume(&ctx, &s)?; }
            // A host that doesn't answer has already gone; ending the row is what counts.
            if let Some(sock) = &s.socket_path {
                let _ = axel_acp::socket::send(sock, &axel_acp::socket::Command::Stop);
            }
            ctx.store.end_session(&ctx.owner()?, &s.id, Some("stopped by owner"))?;
            if let (Some(rex), Some(block)) = (&ctx.rex, s.rex_block_id.as_deref()) {
                rex.api_call("block.close", serde_json::json!({"block_id": block})).ok();
            }
            println!("{}", t!("ended {session}", session = s.id));
            Ok(())
        }
        SessionCommand::Move { session, to } => {
            let ctx = Ctx::open()?;
            let s = resolve_session(&ctx, &session)?;
            let (id, place) = super::host::move_session(&ctx, &s, &to)?;
            println!("{}", t!("moved to {place}: session {session} resumes the conversation", place = place, session = id));
            Ok(())
        }
        SessionCommand::Ls { all } => {
            let ctx = Ctx::open()?;
            let states = if all { None } else { Some(vec![SessionState::Starting, SessionState::Running, SessionState::Blocked, SessionState::Idle, SessionState::Paused]) };
            let agents = ctx.store.list_agents()?;
            for s in ctx.store.list_sessions(SessionFilter { states, limit: Some(LIST_SESSIONS_MAX), ..Default::default() })? {
                let agent = agents.iter().find(|a| a.pubkey == s.agent_pubkey).map(|a| a.name.as_str()).unwrap_or("?");
                let thread = ctx.store.get_thread(&s.thread_id)?.map(|t| t.title).unwrap_or_default();
                println!("{}  {:<8} {:<7} {} {} {}", s.id, s.state, s.runtime, super::text::pad(agent, 12), super::text::pad(&s.environment_label, 10), thread);
            }
            Ok(())
        }
    }
}

/// Most sessions `axel session ls` lists, newest first.
const LIST_SESSIONS_MAX: usize = 200;
/// How many of the newest sessions an id prefix is matched against: an old
/// session is still reachable by its full id.
const RESOLVE_SESSIONS_MAX: usize = 2000;

/// A full session id or a unique prefix.
fn resolve_session(ctx: &Ctx, id: &str) -> Result<String> {
    if ctx.store.get_session(id)?.is_some() {
        return Ok(id.to_string());
    }
    let hits: Vec<_> = ctx
        .store
        .list_sessions(SessionFilter { limit: Some(RESOLVE_SESSIONS_MAX), ..Default::default() })?
        .into_iter()
        .filter(|s| s.id.starts_with(id))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => bail!("{}", t!("no session matches {query}", query = format!("{id:?}"))),
        n => bail!("{}", t!("{n} sessions match {query}; be more specific", n = n, query = format!("{id:?}"))),
    }
}

/// Socket path for a session (local).
pub fn socket_path(session_id: &str) -> PathBuf {
    paths::home().join("run").join(format!("{session_id}.sock"))
}

pub fn log_path(session_id: &str) -> PathBuf {
    paths::home().join("logs").join(format!("{session_id}.jsonl"))
}

/// `_meta` for `session/new`: Claude gets the guard-only hook settings through
/// the adapter's `claudeCode.options.extraArgs` (`--settings <file>`).
/// The adapter runs the `claude` you have installed (the built-in one starts
/// `$CLAUDE_CODE_EXECUTABLE`; Zed's Node adapter would otherwise use the one
/// bundled in its SDK, which lags), so pane and managed sessions behave alike.
/// `$CLAUDE_CODE_EXECUTABLE`, when set, wins.
fn claude_executable_env() -> Option<(String, String)> {
    if let Ok(v) = std::env::var("CLAUDE_CODE_EXECUTABLE")
        && !v.trim().is_empty()
    {
        return Some(("CLAUDE_CODE_EXECUTABLE".into(), v));
    }
    let exe = claude_executable(super::launch::which("claude")?);
    Some(("CLAUDE_CODE_EXECUTABLE".into(), exe.to_string_lossy().into_owned()))
}

/// The path to start Claude Code from, given where `claude` was found on
/// `PATH`.  An npm install links `claude` to a JavaScript entry point, which
/// the adapter runs with node, so that link is resolved.  Any other link is
/// kept as found: a version manager's shim (mise) links to the manager's own
/// binary, which only acts as claude under the shim's name, so resolving it
/// would start the manager instead.
fn claude_executable(found: PathBuf) -> PathBuf {
    let is_js = |p: &Path| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "mjs" | "cjs"));
    match std::fs::canonicalize(&found) {
        Ok(real) if is_js(&real) => real,
        _ => std::path::absolute(&found).unwrap_or(found),
    }
}

fn session_meta(agent: &axel_store::Agent, cwd: &Path, roster: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    // Codex gets who it is as developer instructions (the built-in adapter
    // reads `systemPrompt.append`); the Claude extras below don't apply.
    if agent.harness == axel_store::Harness::Codex {
        let meta = serde_json::json!({ "systemPrompt": { "append": super::launch::system_prompt(agent, cwd, roster) } });
        return meta.as_object().cloned();
    }
    if agent.harness != axel_store::Harness::Claude {
        return None;
    }
    let path = match super::launch::write_claude_guard_hooks() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("axel: {}", t!("could not write the guard hook settings: {error}", error = e));
            return None;
        }
    };
    let mut extra_args = serde_json::json!({ "settings": path.to_string_lossy() });
    match super::launch::write_axel_plugin() {
        Ok(dir) => extra_args["plugin-dir"] = serde_json::json!(dir.to_string_lossy()),
        Err(e) => eprintln!("axel: {}", t!("could not write the axel skill plugin: {error}", error = e)),
    }
    let meta = serde_json::json!({
        "systemPrompt": { "append": super::launch::system_prompt(agent, cwd, roster) },
        "claudeCode": { "options": { "extraArgs": extra_args } }
    });
    meta.as_object().cloned()
}

/// The most sessions of one agent on one thread looked at when resuming:
/// a thread resumed every day for years stays far below it.
const RESUME_SESSIONS_MAX: usize = 1000;

/// `axel session resume SESSION`: what Rex runs to bring an agent's session
/// back after it restarts.  The agent comes back in this pane, continuing
/// the newest conversation it had on the thread, with no prompt sent again.
/// Safe to run twice: when the agent already runs there, it changes nothing.
fn resume(session_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let reported = ctx.store.get_session(session_id)?.with_context(|| t!("no session {id}", id = session_id))?;
    // Sessions whose pane is gone end first, so only live ones remain live.
    super::reconcile::run(&ctx).ok();
    let mine = sessions_of(&ctx, &reported)?;
    if let Some(live) = mine.iter().find(|s| still_running(&ctx, s)) {
        println!("{}", t!("already running as session {id}", id = live.id));
        return Ok(());
    }
    let Some(latest) = mine.iter().find(|s| s.harness_session_id.is_some()) else {
        bail!("{}", t!("session {id} has no conversation to continue", id = session_id));
    };
    for s in mine.iter().filter(|s| s.state != SessionState::Ended) {
        ctx.store.end_session(&ctx.owner()?, &s.id, Some("its terminal restarted"))?;
    }
    let thread = ctx.store.get_thread(&reported.thread_id)?.context(t!("the thread vanished"))?;
    let req = axel_tui::LaunchRequest {
        title: thread.title.clone(),
        projects: vec![],
        agent_pubkey: reported.agent_pubkey.clone(),
        workspace: axel_store::WorkspaceKind::InPlace,
        branch: None,
        thread_id: Some(thread.id.clone()),
        attachments: vec![],
    };
    let opts = super::launch::LaunchOptions {
        target: super::launch::Target::ThisPane,
        runtime: Some(latest.runtime),
        prompt: None,
        attachments: vec![],
        work_dir: Some(std::env::current_dir()?),
        resume: latest.harness_session_id.clone(),
    };
    match super::launch::launch(&ctx, req, opts)? {
        super::launch::Launched::Managed { host_spec: Some(spec), .. } | super::launch::Launched::Inline { spec, .. } => {
            super::launch::exec_inline(spec)
        }
        // Without a spec, the session already runs elsewhere.
        _ => Ok(()),
    }
}

/// The agent's sessions on the reported session's thread, newest first.
fn sessions_of(ctx: &Ctx, reported: &axel_store::Session) -> Result<Vec<axel_store::Session>> {
    let filter = SessionFilter {
        thread_id: Some(reported.thread_id.clone()),
        agent_pubkey: Some(reported.agent_pubkey.clone()),
        limit: Some(RESUME_SESSIONS_MAX),
        ..Default::default()
    };
    let mut sessions = ctx.store.list_sessions(filter)?;
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
    debug_assert!(sessions.iter().any(|s| s.id == reported.id), "the reported session is among its agent's");
    Ok(sessions)
}

/// Whether `s` still runs somewhere: a managed host that answers, or a pane
/// session in a pane that isn't this one (this pane is where it died).
fn still_running(ctx: &Ctx, s: &axel_store::Session) -> bool {
    if s.state == SessionState::Ended {
        return false;
    }
    if s.environment_id != ctx.store.local_environment().map(|e| e.id).unwrap_or_default() {
        // Another machine's session: it runs there.
        return true;
    }
    match s.runtime {
        SessionRuntime::Managed => s
            .socket_path
            .as_deref()
            .is_some_and(|sock| axel_acp::socket::send(sock, &axel_acp::socket::Command::Status).is_ok()),
        SessionRuntime::Pane => {
            let here = ctx.rex.as_ref().map(|r| r.env().block.clone());
            s.rex_block_id.is_some() && s.rex_block_id != here
        }
    }
}

fn host(session_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let session = ctx.store.get_session(session_id)?.with_context(|| t!("no session {id}", id = session_id))?;
    if session.runtime != SessionRuntime::Managed {
        bail!("{}", t!("session {id} is a pane session, not managed", id = session_id));
    }
    let thread = ctx.store.get_thread(&session.thread_id)?.context(t!("the thread vanished"))?;
    let agent = ctx.store.agent_by_pubkey(&session.agent_pubkey)?.context(t!("the agent vanished"))?;
    let signer = ctx.agent_signer(&agent.pubkey, Some(&session.thread_id))?;

    let cwd = std::env::var("AXEL_CWD").map(PathBuf::from).ok().filter(|p| p.is_dir()).unwrap_or(std::env::current_dir()?);
    // If Rex restarts, this pane comes back as the session, its conversation
    // continued (OSC 7577).
    super::resume::come_back_as(&["session", "resume", &session.id], Some(&cwd));
    let initial_prompt = std::env::var("AXEL_PROMPT").ok().filter(|p| !p.trim().is_empty());
    let initial_attachments: Vec<PathBuf> = std::env::var("AXEL_ATTACHMENTS")
        .ok()
        .map(|v| v.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    let disallowed: Vec<String> = Vec::new();

    if let Some(profile) = ClaudeProfile::load(&agent.pubkey)? {
        profile.configure_host()?;
    }
    let env = harness_env(&thread.id, &session.id, &agent);

    let roster = super::team::roster(&ctx.store, &agent.profile_id)?;
    let meta = session_meta(&agent, &cwd, &roster);
    let mcp_servers = vec![super::launch::axel_mcp_server(&env)];
    let cfg = HostConfig {
        session_id: session.id.clone(),
        thread_id: thread.id.clone(),
        agent_pubkey: agent.pubkey.clone(),
        cwd,
        harness: agent.harness.clone(),
        // The session's mode, not the agent's: a summoned session may run narrower.
        runtime_mode: session.runtime_mode,
        disallowed_tools: disallowed,
        log_path: log_path(&session.id),
        socket_path: socket_path(&session.id),
        env,
        initial_prompt,
        initial_attachments,
        initial_from: prompt_sender(&ctx),
        reply_with: std::env::current_exe().ok(),
        resume_native_id: session.harness_session_id.clone(),
        session_meta: meta,
        // The workspace profile's model wins over the agent's, as for pane sessions.
        model: agent.model.clone(),
        // Axel's own tools (send, set_status, read_inbox), acting as this agent.
        mcp_servers,
        // The agent's reasoning effort; the host applies it per harness and
        // warns where it cannot.
        effort: agent.effort,
    };
    std::fs::create_dir_all(cfg.socket_path.parent().expect("socket_path joins onto the run directory"))?;
    std::fs::create_dir_all(cfg.log_path.parent().expect("log_path joins onto the logs directory"))?;

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    let store = ctx.store;
    local.block_on(&rt, axel_acp::run_host(cfg, store, signer, std::io::stdout()))
}

/// The environment the adapter and harness run with: credentials, PATH and
/// Axel's own variables from ours, plus the session's identity.
fn harness_env(thread_id: &str, session_id: &str, agent: &axel_store::Agent) -> Vec<(String, String)> {
    // The adapter and the harness under it must not look like they run inside
    // Rex: user-level hooks (e.g. the Rex skill's SessionStart hook) would fire
    // and the agent would start renaming the user's Rex session. The host pane
    // keeps its own REX_* for Axel; the children lose them.
    for k in ["REX_SESSION", "REX_BLOCK", "REX_SERVER", "REX_CLIENT", "REX_BIN_DIR", "REX_HOME", "REX_RUNTIME_DIR"] {
        // SAFETY: single-threaded here, before the tokio runtime starts.
        unsafe { std::env::remove_var(k) };
    }
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k.starts_with("ANTHROPIC_") || k.starts_with("OPENAI_") || k.starts_with("CODEX_") || k.starts_with("AXEL_")).collect();
    env.push(("AXEL_THREAD".into(), thread_id.to_string()));
    env.push(("AXEL_SESSION".into(), session_id.to_string()));
    env.push(("AXEL_AGENT".into(), agent.pubkey.clone()));
    env.push(("AXEL_AGENT_NAME".into(), agent.name.clone()));
    if agent.harness == axel_store::Harness::Claude {
        env.extend(claude_executable_env());
    }
    env
}

/// The agent that summoned this session, whose first message it is.  One
/// this machine doesn't know (or can't look up) sends as the owner.
fn prompt_sender(ctx: &Ctx) -> Option<axel_acp::socket::Sender> {
    let pk = std::env::var("AXEL_PROMPT_FROM").ok().filter(|pk| !pk.is_empty())?;
    let name = ctx.store.agent_by_pubkey(&pk).ok().flatten()?.name;
    Some(axel_acp::socket::Sender::new(pk, name))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// `bin/claude` in a fresh directory, linked to `target` (created empty).
    fn linked(target: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join(target);
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        let link = root.join("bin").join("claude");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&real, &link).unwrap();
        (dir, link, real)
    }

    #[test]
    fn a_link_to_a_javascript_entry_point_is_resolved() {
        let (_dir, link, real) = linked("lib/node_modules/claude-code/cli.js");
        assert_eq!(claude_executable(link), real);
    }

    #[test]
    fn a_version_manager_shim_keeps_its_own_name() {
        let (_dir, link, _real) = linked("usr/bin/mise");
        assert_eq!(claude_executable(link.clone()), link);
    }

    #[test]
    fn a_link_to_a_versioned_binary_is_kept() {
        let (_dir, link, _real) = linked("share/claude/versions/2.1.322");
        assert_eq!(claude_executable(link.clone()), link);
    }

    #[test]
    fn a_plain_file_is_returned_as_found() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().canonicalize().unwrap().join("claude");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(claude_executable(exe.clone()), exe);
    }
}
