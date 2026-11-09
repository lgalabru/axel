//! `axel session …` — hosting and controlling managed (ACP) sessions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axel_acp::HostConfig;
use axel_store::{SessionFilter, SessionRuntime, SessionState};

use super::Ctx;
use crate::cli::SessionCommand;
use crate::paths;

pub fn run(cmd: SessionCommand) -> Result<()> {
    match cmd {
        SessionCommand::Host { session } => host(&session),
        SessionCommand::Stop { session } => {
            let ctx = Ctx::open()?;
            let s = ctx.store.get_session(&session)?.with_context(|| format!("no session {session}"))?;
            if s.state == SessionState::Paused { super::process_control::resume(&ctx, &s)?; }
            if let Some(sock) = &s.socket_path {
                let _ = axel_acp::socket::send(sock, &axel_acp::socket::Command::Stop);
            }
            ctx.store.end_session(&ctx.owner()?, &s.id, Some("stopped by owner"))?;
            if let (Some(rex), Some(block)) = (&ctx.rex, s.rex_block_id.as_deref()) {
                rex.api_call("block.close", serde_json::json!({"block_id": block})).ok();
            }
            println!("ended {}", s.id);
            Ok(())
        }
        SessionCommand::Move { session, to } => {
            let ctx = Ctx::open()?;
            let s = resolve_session(&ctx, &session)?;
            let (id, place) = super::host::move_session(&ctx, &s, &to)?;
            println!("moved to {place}: session {id} resumes the conversation");
            Ok(())
        }
        SessionCommand::Ls { all } => {
            let ctx = Ctx::open()?;
            let states = if all { None } else { Some(vec![SessionState::Starting, SessionState::Running, SessionState::Blocked, SessionState::Idle, SessionState::Paused]) };
            let agents = ctx.store.list_agents()?;
            for s in ctx.store.list_sessions(SessionFilter { states, limit: Some(200), ..Default::default() })? {
                let agent = agents.iter().find(|a| a.pubkey == s.agent_pubkey).map(|a| a.name.as_str()).unwrap_or("?");
                let thread = ctx.store.get_thread(&s.thread_id)?.map(|t| t.title).unwrap_or_default();
                println!("{}  {:<8} {:<7} {:<12} {:<10} {}", s.id, s.state, s.runtime, agent, s.environment_label, thread);
            }
            Ok(())
        }
    }
}

/// A full session id or a unique prefix.
fn resolve_session(ctx: &Ctx, id: &str) -> Result<String> {
    if ctx.store.get_session(id)?.is_some() {
        return Ok(id.to_string());
    }
    let hits: Vec<_> = ctx
        .store
        .list_sessions(SessionFilter { limit: Some(2000), ..Default::default() })?
        .into_iter()
        .filter(|s| s.id.starts_with(id))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => bail!("no session matches {id:?}"),
        n => bail!("{n} sessions match {id:?}; be more specific"),
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
    let exe = super::launch::which("claude")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Some(("CLAUDE_CODE_EXECUTABLE".into(), exe.to_string_lossy().into_owned()))
}

fn session_meta(agent: &axel_store::Agent, cwd: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    // Codex gets who it is as developer instructions (the built-in adapter
    // reads `systemPrompt.append`); the Claude extras below don't apply.
    if agent.harness == axel_store::Harness::Codex {
        let meta = serde_json::json!({ "systemPrompt": { "append": super::launch::system_prompt(agent, cwd) } });
        return meta.as_object().cloned();
    }
    if agent.harness != axel_store::Harness::Claude {
        return None;
    }
    let path = match super::launch::write_claude_guard_hooks() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("axel: could not write the guard hook settings: {e}");
            return None;
        }
    };
    let mut extra_args = serde_json::json!({ "settings": path.to_string_lossy() });
    match super::launch::write_axel_plugin() {
        Ok(dir) => extra_args["plugin-dir"] = serde_json::json!(dir.to_string_lossy()),
        Err(e) => eprintln!("axel: could not write the axel skill plugin: {e}"),
    }
    let meta = serde_json::json!({
        "systemPrompt": { "append": super::launch::system_prompt(agent, cwd) },
        "claudeCode": { "options": { "extraArgs": extra_args } }
    });
    meta.as_object().cloned()
}

fn host(session_id: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let session = ctx.store.get_session(session_id)?.with_context(|| format!("no session {session_id}"))?;
    if session.runtime != SessionRuntime::Managed {
        bail!("session {session_id} is a pane session, not managed");
    }
    let thread = ctx.store.get_thread(&session.thread_id)?.context("thread vanished")?;
    let agent = ctx.store.agent_by_pubkey(&session.agent_pubkey)?.context("agent vanished")?;
    let signer = ctx.agent_signer(&agent.pubkey, Some(&session.thread_id))?;

    let cwd = std::env::var("AXEL_CWD").map(PathBuf::from).ok().filter(|p| p.is_dir()).unwrap_or(std::env::current_dir()?);
    let initial_prompt = std::env::var("AXEL_PROMPT").ok().filter(|p| !p.trim().is_empty());
    let initial_attachments: Vec<PathBuf> = std::env::var("AXEL_ATTACHMENTS")
        .ok()
        .map(|v| v.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    let disallowed: Vec<String> = Vec::new();

    // The adapter and the harness under it must not look like they run inside
    // Rex: user-level hooks (e.g. the Rex skill's SessionStart hook) would fire
    // and the agent would start renaming the user's Rex session. The host pane
    // keeps its own REX_* for Axel; the children lose them.
    for k in ["REX_SESSION", "REX_BLOCK", "REX_SERVER", "REX_CLIENT", "REX_BIN_DIR", "REX_HOME", "REX_RUNTIME_DIR"] {
        // SAFETY: single-threaded here, before the tokio runtime starts.
        unsafe { std::env::remove_var(k) };
    }
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k.starts_with("ANTHROPIC_") || k.starts_with("OPENAI_") || k.starts_with("CODEX_") || k.starts_with("AXEL_")).collect();
    env.push(("AXEL_THREAD".into(), thread.id.clone()));
    env.push(("AXEL_SESSION".into(), session.id.clone()));
    env.push(("AXEL_AGENT".into(), agent.pubkey.clone()));
    env.push(("AXEL_AGENT_NAME".into(), agent.name.clone()));
    if agent.harness == axel_store::Harness::Claude {
        env.extend(claude_executable_env());
    }

    let meta = session_meta(&agent, &cwd);
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
        initial_from: std::env::var("AXEL_PROMPT_FROM").ok().filter(|pk| !pk.is_empty()).and_then(|pk| {
            let name = ctx.store.agent_by_pubkey(&pk).ok().flatten()?.name;
            Some(axel_acp::socket::Sender::new(pk, name))
        }),
        reply_with: std::env::current_exe().ok(),
        resume_native_id: session.harness_session_id.clone(),
        session_meta: meta,
        // The workspace profile's model wins over the agent's, as for pane sessions.
        model: agent.model.clone(),
        // Axel's own tools (send, set_status, read_inbox), acting as this agent.
        mcp_servers,
    };
    std::fs::create_dir_all(cfg.socket_path.parent().unwrap())?;
    std::fs::create_dir_all(cfg.log_path.parent().unwrap())?;

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    let store = ctx.store;
    local.block_on(&rt, axel_acp::run_host(cfg, store, signer, std::io::stdout()))
}
