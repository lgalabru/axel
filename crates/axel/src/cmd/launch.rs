//! Turn a `LaunchRequest` into a thread (new or existing) plus a running session
//! in a Rex window.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axel_config::{AgentProfile, Harness, HarnessKind, WorkspaceConfig, drivers, git, launch as cfg_launch};
use axel_store::{NewSession, RuntimeMode, SessionRuntime, SessionState, SessionUpdate, StatusReport, StatusState, ThreadWorkspace, WorkspaceKind};
use axel_tui::LaunchRequest;

use super::{Ctx, convert};
use crate::paths;

pub enum Launched {
    RexWindow { window_id: String, thread_id: String, session_id: String },
    /// A managed (ACP) session: its host runs in `host_window` (None when the
    /// caller should exec the host itself, i.e. outside Rex).
    Managed { thread_id: String, session_id: String, host_window: Option<String>, host_spec: Option<cfg_launch::LaunchSpec> },
    /// Run the harness in this very process (exec). Used outside Rex, and for
    /// `Target::Here`, where the launcher pane becomes the agent pane.
    Inline { spec: cfg_launch::LaunchSpec, label: String, thread_id: String, session_id: String },
}

/// How a launch should behave.
#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    pub target: Target,
    /// None = managed when the ACP adapter is installed, else pane.
    pub runtime: Option<SessionRuntime>,
    /// First message for the agent; defaults to the thread intent.
    pub prompt: Option<String>,
    /// Files dropped into the prompt field, sent with the first message.
    pub attachments: Vec<PathBuf>,
    /// Work here instead of the thread's project (a remote job's worktree).
    pub work_dir: Option<PathBuf>,
    /// Continue this harness conversation (its native session id) instead of
    /// starting a new one.  `$AXEL_RESUME` does the same.
    pub resume: Option<String>,
}

/// Where the harness should run when we're inside Rex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Target {
    /// A new Rex tab (window) in the current session.
    #[default]
    NewWindow,
    /// Right here: the calling pane execs the harness (the floating launcher
    /// turns into the agent's pane).
    Here,
}

/// Stable key for "the same repository on another machine": the origin URL,
/// normalized (scheme/user/`.git` stripped, lowercase host).
pub fn repo_key(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(normalize_repo_url(&url))
}

pub fn normalize_repo_url(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if let Some(rest) = u.strip_suffix(".git") {
        u = rest.to_string();
    }
    // git@host:owner/repo → host/owner/repo
    if let Some(rest) = u.strip_prefix("git@") {
        return rest.replacen(':', "/", 1).to_ascii_lowercase();
    }
    // scheme://[user@]host/owner/repo → host/owner/repo
    if let Some(idx) = u.find("://") {
        let rest = &u[idx + 3..];
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        return rest.to_ascii_lowercase();
    }
    u
}

/// Pick the runtime: managed when the harness has an ACP adapter on PATH.
pub fn pick_runtime(harness: &axel_store::Harness, wanted: Option<SessionRuntime>) -> SessionRuntime {
    if let Some(r) = wanted {
        return r;
    }
    let cmd = axel_acp::adapter_command(harness);
    match cmd.first() {
        Some(program) if which(program).is_some() => SessionRuntime::Managed,
        Some(program) => {
            eprintln!("axel: {program} not found on PATH; falling back to the pane runtime for {harness}");
            SessionRuntime::Pane
        }
        None => SessionRuntime::Pane,
    }
}

/// Human description of what `pick_runtime` would choose, for the launcher summary.
pub fn runtime_label(harness: &axel_store::Harness) -> String {
    let cmd = axel_acp::adapter_command(harness);
    match cmd.first() {
        Some(program) if which(program).is_some() => format!("managed ({program})"),
        Some(program) => format!("pane ({program} not found on PATH)"),
        None => "pane".to_string(),
    }
}

/// Start a session host in the background, in its own process group so it
/// outlives the command (often an agent's tool call) that started it.  Its
/// terminal output goes to `~/.axel/logs/SESSION.host.log`.
pub fn spawn_detached(spec: &cfg_launch::LaunchSpec, session_id: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let dir = paths::home().join("logs");
    std::fs::create_dir_all(&dir)?;
    let out = std::fs::File::create(dir.join(format!("{session_id}.host.log")))?;
    std::process::Command::new(&spec.program)
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .process_group(0)
        .spawn()
        .context("starting the session host")?;
    Ok(())
}

pub fn launch(ctx: &Ctx, req: LaunchRequest, opts: LaunchOptions) -> Result<Launched> {
    let target = opts.target;
    let resume = opts.resume.clone().or_else(|| std::env::var("AXEL_RESUME").ok()).filter(|r| !r.trim().is_empty());
    let owner = ctx.owner()?;
    let env = ctx.store.local_environment()?;
    if env.rex_host_id.is_none()
        && let Some(host) = ctx.rex.as_ref().and_then(|r| r.host_id().ok()) {
            ctx.store.set_environment_rex_host(&env.id, &host).ok();
        }
    let agent = ctx
        .store
        .agent_by_pubkey(&req.agent_pubkey)?
        .with_context(|| format!("agent {} not found", req.agent_pubkey))?;
    // The agent's key, or (on a remote host) this host's key under the
    // agent's delegation for the thread.
    let agent_kp = ctx
        .agent_signer(&req.agent_pubkey, req.thread_id.as_deref())
        .with_context(|| format!("refusing to launch {}: its key is missing from the keystore", agent.name))?;
    // Reach, loops, depth and budget; a summoned session never runs broader
    // than the session that summoned it.
    let (parent_session, mode_cap) = super::policy::check_summon(ctx, &agent.pubkey)
        .with_context(|| format!("refusing to launch {}", agent.name))?;
    let runtime_mode = mode_cap.map_or(agent.runtime_mode, |cap| axel_store::narrower(agent.runtime_mode, cap));

    // 1. Thread: new, or an existing one we're spawning another agent onto.
    let thread = match &req.thread_id {
        Some(id) => ctx.store.get_thread(id)?.with_context(|| format!("thread {id} not found"))?,
        // The first line names it; the whole text is what we're building.
        None => ctx.store.create_thread(&owner, &axel_tui::util::title_line(&req.title), &req.title, None)?,
    };

    // 2. Projects (0..n). Zero projects = scratch directory.
    let mut primary_dir: Option<PathBuf> = opts.work_dir.clone();
    let mut primary_repo_key: Option<String> = opts.work_dir.as_deref().and_then(repo_key);
    if primary_dir.is_some() {
        // A remote job: the worktree was prepared by the caller.
    } else if req.thread_id.is_none() {
        for p in &req.projects {
            let path = std::fs::canonicalize(p).unwrap_or(p.clone());
            let key = repo_key(&path);
            ctx.store.upsert_project(&env.id, &path, key.as_deref())?;
            let (work_dir, ws) = prepare_workspace(&path, req.workspace, req.branch.as_deref(), &axel_tui::util::title_line(&req.title))?;
            ctx.store.attach_project(&owner, &thread.id, key.as_deref(), &path, &ws)?;
            if primary_dir.is_none() {
                primary_dir = Some(work_dir);
                primary_repo_key = key;
            }
        }
    } else {
        // Spawning: reuse the thread's first project that exists on this replica.
        for tp in ctx.store.thread_projects(&thread.id)? {
            let local = tp
                .local_project_id
                .as_deref()
                .and_then(|id| ctx.store.get_project(id).ok().flatten())
                .map(|p| p.path)
                .or_else(|| tp.path_hint.is_dir().then(|| tp.path_hint.clone()));
            if let Some(path) = local {
                let dir = tp.workspace.worktree_path.clone().filter(|w| w.is_dir()).unwrap_or(path);
                primary_dir = Some(dir);
                primary_repo_key = tp.repo_key.clone();
                break;
            }
        }
    }
    let work_dir = match primary_dir {
        Some(d) => d,
        None => {
            let dir = paths::home().join("scratch").join(format!("{}-{}", axel_tui::slugify(&thread.title), &thread.id[..8]));
            std::fs::create_dir_all(&dir)?;
            if req.thread_id.is_none() {
                let ws = ThreadWorkspace { kind: WorkspaceKind::Scratch, branch: None, worktree_path: None };
                ctx.store.attach_project(&owner, &thread.id, None, &dir, &ws)?;
            }
            dir
        }
    };

    // 3. Profile, from the agent's row in the store.
    let config = WorkspaceConfig::load(&work_dir).context("reading AXEL.md")?;
    let profile = build_profile(&agent);

    // 4. Skills (remote ones fetched and pinned in AXEL.lock) and the project context.
    if let Some(driver) = drivers::for_harness(profile.harness.kind) {
        install_skills(driver.as_ref(), &work_dir, &profile.skills, config.as_ref(), &paths::global_skills_dir())?;
    }

    // 5. Session, signed by the agent itself.
    let runtime = pick_runtime(&agent.harness, opts.runtime);
    let session = ctx.store.start_session(
        &agent_kp,
        &NewSession {
            thread_id: thread.id.clone(),
            agent_pubkey: agent.pubkey.clone(),
            environment_id: env.id.clone(),
            environment_label: env.label.clone(),
            project_repo_key: primary_repo_key,
            harness: agent.harness.clone(),
            runtime_mode,
            runtime,
            resumed_from: resume.as_ref().and(last_session_id(ctx, &thread.id)),
        },
    )?;
    if let Some(parent) = &parent_session {
        ctx.store.link_session(&session.id, parent)?;
    }
    ctx.store.touch_agent(&agent.pubkey)?;

    // 6. Command line.
    let prompt = match &opts.prompt {
        Some(p) => p.clone(),
        None if req.thread_id.is_some() => format!("{}\n\n(You are joining an existing thread. Read the inbox with `axel inbox` for context.)", thread.intent),
        None => thread.intent.clone(),
    };
    // Picking the project in the launcher is the trust decision, and picking
    // autopilot is the permissions decision: pre-answer Claude Code's two startup
    // dialogs (both default to "No, exit"). This mirrors t3code, which runs Claude
    // through the Agent SDK with bypassPermissions and never shows them.
    if profile.harness.kind == HarnessKind::Claude
        && let Err(e) = prepare_claude_config(&work_dir, runtime_mode == RuntimeMode::Autopilot) {
            eprintln!("axel: could not prepare Claude Code config for {}: {e}", work_dir.display());
        }
    let mut spec = cfg_launch::build(&profile, if resume.is_some() { None } else { Some(&prompt) });
    if let Some(hs) = &resume {
        match profile.harness.kind {
            HarnessKind::Claude => {
                let mut args = vec!["--resume".to_string(), hs.clone()];
                args.append(&mut spec.args);
                spec.args = args;
            }
            HarnessKind::Codex => {
                // `codex resume <id>` is a subcommand: it must come right after the program.
                let mut args = vec!["resume".to_string(), hs.clone()];
                args.append(&mut spec.args);
                spec.args = args;
            }
            _ => {}
        }
    }
    inject_runtime_mode(&mut spec, profile.harness.kind, runtime_mode);
    inject_hooks(&mut spec, profile.harness.kind)?;
    if profile.harness.kind == HarnessKind::Claude {
        // The axel skill and who-you-are, for pane sessions; managed sessions
        // get the same through the adapter (`session_meta`).
        let mut args = vec!["--append-system-prompt".to_string(), system_prompt(&agent, &work_dir)];
        match write_axel_plugin() {
            Ok(dir) => args.extend(["--plugin-dir".to_string(), dir.to_string_lossy().into_owned()]),
            Err(e) => eprintln!("axel: could not write the axel skill plugin: {e}"),
        }
        // Axel's tools; the server inherits the pane's AXEL_* identity.
        let mcp = serde_json::json!({ "mcpServers": { MCP_SERVER: { "command": axel_exe(), "args": ["mcp"] } } });
        // `=` form: both flags take lists, so a bare value would swallow a prompt argument after it.
        args.extend([format!("--mcp-config={mcp}"), format!("--allowedTools={MCP_ALLOW}")]);
        args.append(&mut spec.args);
        spec.args = args;
    }
    spec.env.push(("AXEL_THREAD".into(), thread.id.clone()));
    spec.env.push(("AXEL_SESSION".into(), session.id.clone()));
    spec.env.push(("AXEL_AGENT".into(), agent.pubkey.clone()));
    spec.env.push(("AXEL_AGENT_NAME".into(), agent.name.clone()));
    spec.env.push(("AXEL_RUNTIME_MODE".into(), runtime_mode.to_string()));
    spec.env.push(("AXEL_MODEL".into(), profile.model.clone().unwrap_or_default()));
    spec.env.push(("AXEL_DB".into(), axel_store::Store::default_path().to_string_lossy().into_owned()));
    if !profile.disallowed_tools.is_empty() {
        spec.env.push(("AXEL_DISALLOWED_TOOLS".into(), profile.disallowed_tools.join(",")));
    }
    if let Ok(exe) = std::env::current_exe() {
        spec.env.push(("AXEL_BIN".into(), exe.to_string_lossy().into_owned()));
    }
    // Rex panes we create run with `shell: none`: no login PATH. Resolve the
    // program now, with our PATH, and hand our PATH down so hooks find `axel`.
    if let Some(abs) = which(&spec.program) {
        spec.program = abs.to_string_lossy().into_owned();
    }
    if let Ok(path) = std::env::var("PATH") {
        spec.env.push(("PATH".into(), path));
    }
    if !work_dir.is_dir() {
        bail!("workspace directory does not exist: {}", work_dir.display());
    }
    let label = format!("{} · {}", agent.name, truncate(&thread.title, 40));

    // Managed runtime: the harness is driven over ACP by `axel session host`,
    // which runs in its own pane (that pane is the raw log). The caller opens
    // the thread view wherever it likes.
    if runtime == SessionRuntime::Managed {
        // Resuming: the host loads this conversation (`session/load`).
        ctx.store.update_session(
            &agent_kp,
            &session.id,
            &SessionUpdate { state: Some(SessionState::Starting), harness_session_id: resume.clone(), ..Default::default() },
        )?;
        let axel = std::env::current_exe().context("locating axel")?.to_string_lossy().into_owned();
        let mut host_spec = cfg_launch::LaunchSpec { program: axel, args: vec!["session".into(), "host".into(), session.id.clone()], env: vec![] };
        // The host reads what it needs from the store; pass only process env.
        for (k, v) in &spec.env {
            if k == "PATH" || k.starts_with("ANTHROPIC_") || k.starts_with("OPENAI_") || k.starts_with("CODEX_") || k == "AXEL_API_KEY_ENV" || k.starts_with("AXEL_") {
                host_spec.env.push((k.clone(), v.clone()));
            }
        }
        host_spec.env.push(("AXEL_PROMPT".into(), prompt.clone()));
        // Summoned by another agent: the first message is from it, not the owner.
        if let Some(from) = super::policy::caller_agent().filter(|pk| *pk != agent.pubkey) {
            host_spec.env.push(("AXEL_PROMPT_FROM".into(), from));
        }
        let attachments: Vec<PathBuf> = opts.attachments.iter().chain(req.attachments.iter()).cloned().collect();
        if !attachments.is_empty() {
            let joined = attachments.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
            host_spec.env.push(("AXEL_ATTACHMENTS".into(), joined));
        }
        host_spec.env.push(("AXEL_CWD".into(), work_dir.to_string_lossy().into_owned()));
        let Some(rex) = &ctx.rex else {
            return Ok(Launched::Managed { thread_id: thread.id, session_id: session.id, host_window: None, host_spec: Some(host_spec) });
        };
        let mut command = vec![host_spec.program.clone()];
        command.extend(host_spec.args.iter().cloned());
        let pane_label = format!("{label}{}", super::ui::AGENT_PANE_SUFFIX);
        // The session host runs out of sight, in no window: the thread's own
        // pane reports each agent's status to Rex (see `ui::run_stream`).
        // The thread's window is still its window, for focusing it.
        let window_id = super::ui::window_of_block(rex, &rex.env().block).ok().flatten().unwrap_or_default();
        let block = Some(
            rex.new_detached_terminal(&pane_label, &command, Some(&work_dir), &host_spec.env)
                .context("starting the session host in Rex")?,
        );
        ctx.store.update_session(
            &agent_kp,
            &session.id,
            &SessionUpdate { rex: Some((Some(rex.env().session.clone()), Some(window_id.clone()), block)), ..Default::default() },
        )?;
        return Ok(Launched::Managed { thread_id: thread.id, session_id: session.id, host_window: Some(window_id), host_spec: None });
    }

    let here = match (&ctx.rex, target) {
        (None, _) => None,
        (Some(rex), Target::Here) => Some(rex),
        (Some(_), Target::NewWindow) => None,
    };
    if here.is_some() || ctx.rex.is_none() {
        let rex_ids = here.map(|rex| {
            let block = rex.env().block.clone();
            let window = super::ui::window_of_block(rex, &block).ok().flatten();
            (Some(rex.env().session.clone()), window, Some(block))
        });
        ctx.store.update_session(&agent_kp, &session.id, &SessionUpdate { state: Some(SessionState::Running), rex: rex_ids, ..Default::default() })?;
        ctx.store.record_status(
            &agent_kp,
            &session.id,
            &StatusReport { state: StatusState::Working, kind: None, title: Some(thread.title.clone()), msg: Some("starting".into()), progress: None },
        )?;
        // cwd for the exec'd harness.
        spec.env.push(("AXEL_CWD".into(), work_dir.to_string_lossy().into_owned()));
        return Ok(Launched::Inline { spec, label, thread_id: thread.id, session_id: session.id });
    }
    let rex = ctx.rex.as_ref().expect("checked above");

    // 7. Rex window.
    let mut command = vec![spec.program.clone()];
    command.extend(spec.args.iter().cloned());
    let window = rex
        .new_window(&label, &command, Some(&work_dir), &spec.env, true)
        .context("opening a Rex window for the agent")?;
    let block = window.block_ids.first().cloned();
    ctx.store.update_session(
        &agent_kp,
        &session.id,
        &SessionUpdate {
            state: Some(SessionState::Running),
            rex: Some((Some(rex.env().session.clone()), Some(window.window_id.clone()), block)),
            ..Default::default()
        },
    )?;
    ctx.store.record_status(
        &agent_kp,
        &session.id,
        &StatusReport { state: StatusState::Working, kind: None, title: Some(thread.title.clone()), msg: Some("starting".into()), progress: None },
    )?;
    Ok(Launched::RexWindow { window_id: window.window_id, thread_id: thread.id, session_id: session.id })
}

fn last_session_id(ctx: &Ctx, thread_id: &str) -> Option<String> {
    ctx.store
        .list_sessions(axel_store::SessionFilter { thread_id: Some(thread_id.to_string()), limit: Some(1), ..Default::default() })
        .ok()?
        .into_iter()
        .next()
        .map(|s| s.id)
}

/// Prepare where the agent works for one project.
fn prepare_workspace(project: &Path, kind: WorkspaceKind, branch: Option<&str>, title: &str) -> Result<(PathBuf, ThreadWorkspace)> {
    match kind {
        WorkspaceKind::Worktree => {
            let repo = git::repo_root(project).context("project is not a git repository")?;
            let branch = branch.map(String::from).unwrap_or_else(|| format!("axel/{}", axel_tui::slugify(title)));
            let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
            let dest = paths::worktrees_dir().join(name).join(branch.replace('/', "-"));
            let path = git::ensure_worktree(&repo, &branch, &dest).context("creating worktree")?;
            Ok((path.clone(), ThreadWorkspace { kind, branch: Some(branch), worktree_path: Some(path) }))
        }
        _ => Ok((project.to_path_buf(), ThreadWorkspace { kind: WorkspaceKind::InPlace, branch: git::current_branch(project), worktree_path: None })),
    }
}

/// Pre-answer Claude Code's startup dialogs in `~/.claude.json`: folder trust
/// (`projects.<dir>.hasTrustDialogAccepted`) and, when the agent runs in
/// autopilot, the bypass-permissions acknowledgement
/// (`bypassPermissionsModeAccepted`, a one-time global flag the CLI itself sets
/// when a user accepts). Merges into the existing file and writes atomically;
/// other fields are kept.
pub fn prepare_claude_config(dir: &Path, autopilot: bool) -> Result<()> {
    let path = dirs::home_dir().context("no home dir")?.join(".claude.json");
    let mut root: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let mut changed = false;
    let key = dir.to_string_lossy().into_owned();
    let projects = root.as_object_mut().unwrap().entry("projects").or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        *projects = serde_json::json!({});
    }
    let entry = projects.as_object_mut().unwrap().entry(key).or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    if entry.get("hasTrustDialogAccepted").and_then(|v| v.as_bool()) != Some(true) {
        entry.as_object_mut().unwrap().insert("hasTrustDialogAccepted".into(), serde_json::Value::Bool(true));
        changed = true;
    }
    // Autopilot runs with --dangerously-skip-permissions; pre-accept Claude
    // Code's one-time bypass-permissions warning so the pane doesn't block on it.
    if autopilot && root.get("bypassPermissionsModeAccepted").and_then(|v| v.as_bool()) != Some(true) {
        root.as_object_mut().unwrap().insert("bypassPermissionsModeAccepted".into(), serde_json::Value::Bool(true));
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let tmp = path.with_extension("json.axel-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Replace this process with the harness (cwd from `AXEL_CWD` if set).
pub fn exec_inline(spec: cfg_launch::LaunchSpec) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // Drop any keypress still buffered from the launcher (the Enter that
    // confirmed it must not answer the harness's first prompt).
    // SAFETY: tcflush on fd 0 has no memory-safety preconditions.
    unsafe { libc::tcflush(0, libc::TCIFLUSH) };
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
        if k == "AXEL_CWD" {
            cmd.current_dir(v);
        }
    }
    let err = cmd.exec();
    bail!("failed to exec {}: {err}", spec.program)
}

/// PATH lookup for a bare program name.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| p.is_file())
}

fn build_profile(agent: &axel_store::Agent) -> AgentProfile {
    let mut profile = AgentProfile {
        id: agent.name.clone(),
        name: Some(agent.name.clone()),
        harness: Harness {
            kind: convert::harness_kind(&agent.harness),
            command: convert::pane_command(&agent.harness),
            args: vec![],
        },
        model: agent.model.clone(),
        skills: agent.skills.clone(),
        prompt: agent.prompt.clone(),
        ..Default::default()
    };
    profile.pubkey = Some(agent.pubkey.clone());
    profile
}

/// Every skill a launch in this project can use: the project's sources (remote ones
/// resolved to their pinned checkout, fetched and pinned on first use), then the
/// global directory. A source that can't be used is reported and skipped.
/// Give the agent its skills in `work_dir` (the project's, remote ones, and
/// `global`'s, as `requested` names them) and link `AXEL.md` where the
/// harness reads its context.  The links are this machine's: each goes in
/// the repository's `.git/info/exclude`, so a `git add -A` never commits it.
/// The skills installed, by name.
pub fn install_skills(
    driver: &dyn axel_config::drivers::SkillDriver,
    work_dir: &Path,
    requested: &[String],
    config: Option<&WorkspaceConfig>,
    global: &Path,
) -> Result<Vec<String>> {
    let all = skills_from(config, global)?;
    let skills = axel_config::resolve_skills(requested, &all);
    driver.install(work_dir, &skills).context("installing skills")?;
    let dir = driver.skills_dir(work_dir);
    let mut local: Vec<PathBuf> = skills.iter().map(|s| dir.join(&s.name)).collect();
    if let Some(axel_md) = config.and_then(|c| c.path.clone())
        && driver.install_context(work_dir, &axel_md).unwrap_or(false)
    {
        local.push(work_dir.join(driver.context_filename()));
    }
    exclude_from_git(work_dir, &local);
    Ok(skills.into_iter().map(|s| s.name).collect())
}

/// Add `paths` (inside `dir`'s repository) to its `.git/info/exclude`,
/// each once.  Nothing outside a repository; worktrees share their repo's.
fn exclude_from_git(dir: &Path, paths: &[PathBuf]) {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let (Some(top), Some(exclude)) = (git(&["rev-parse", "--show-toplevel"]), git(&["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"])) else {
        return;
    };
    let top = PathBuf::from(&top).canonicalize().unwrap_or_else(|_| PathBuf::from(&top));
    let exclude = PathBuf::from(exclude);
    let have = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut add = String::new();
    for p in paths {
        let abs = p.parent().and_then(|d| d.canonicalize().ok()).map(|d| d.join(p.file_name().unwrap_or_default())).unwrap_or_else(|| p.clone());
        let Ok(rel) = abs.strip_prefix(&top) else { continue };
        let line = format!("/{}", rel.to_string_lossy());
        if !have.lines().any(|l| l.trim() == line) && !add.lines().any(|l| l == line) {
            add.push_str(&line);
            add.push('\n');
        }
    }
    if add.is_empty() {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sep = if have.is_empty() || have.ends_with('\n') { "" } else { "\n" };
    let header = if have.contains("# axel:") { "" } else { "# axel: skill links installed for agents on this machine\n" };
    let _ = std::fs::write(&exclude, format!("{have}{sep}{header}{add}"));
}

pub fn project_skills(config: Option<&WorkspaceConfig>) -> Result<axel_config::IndexMap<String, axel_config::Skill>> {
    skills_from(config, &paths::global_skills_dir())
}

/// Every skill `config` names, then `global`'s.
fn skills_from(config: Option<&WorkspaceConfig>, global: &Path) -> Result<axel_config::IndexMap<String, axel_config::Skill>> {
    let global = global.to_path_buf();
    let Some(config) = config else {
        return Ok(axel_config::find_skills(&[], Some(global.as_path()))?);
    };
    let resolved = axel_config::remote::resolve(config, &paths::skills_cache_dir());
    for w in &resolved.warnings {
        eprintln!("axel: skills: {w}");
    }
    Ok(axel_config::find_skills(&resolved.roots, Some(global.as_path()))?)
}

/// `AXEL.md`'s body, when this harness won't read it on its own: the project already
/// has its own `CLAUDE.md` / `AGENTS.md`, so Axel couldn't link `AXEL.md` there, or
/// the harness has no driver to link it for (Goose, Cursor, …).
fn unlinked_context(harness: &axel_store::Harness, dir: &Path) -> Option<String> {
    let config = WorkspaceConfig::load(dir).ok().flatten()?;
    if config.body.trim().is_empty() {
        return None;
    }
    let Some(driver) = drivers::for_harness(convert::harness_kind(harness)) else {
        return Some(config.body);
    };
    let axel_md = config.path.as_ref()?.canonicalize().ok()?;
    let ctx = dir.join(driver.context_filename());
    if ctx.canonicalize().ok().as_ref() == Some(&axel_md) {
        return None;
    }
    Some(config.body)
}

/// [`identity_prompt`], plus `AXEL.md` when the harness wouldn't otherwise see it.
pub fn system_prompt(agent: &axel_store::Agent, dir: &Path) -> String {
    let mut s = identity_prompt(agent);
    if let Some(body) = unlinked_context(&agent.harness, dir) {
        s.push_str("\n\n# This project's AXEL.md\n\n");
        s.push_str(&body);
    }
    s
}

/// Autopilot and friends: harness-specific permission flags, inserted before the
/// trailing prompt positional.
fn inject_runtime_mode(spec: &mut cfg_launch::LaunchSpec, kind: HarnessKind, mode: RuntimeMode) {
    let flags: Vec<String> = match (kind, mode) {
        (HarnessKind::Claude, RuntimeMode::Autopilot) => vec!["--permission-mode".into(), "bypassPermissions".into()],
        (HarnessKind::Claude, RuntimeMode::Edits) => vec!["--permission-mode".into(), "acceptEdits".into()],
        (HarnessKind::Claude, RuntimeMode::Plan) => vec!["--permission-mode".into(), "plan".into()],
        (HarnessKind::Codex, RuntimeMode::Autopilot) => vec!["--full-auto".into()],
        (HarnessKind::Codex, RuntimeMode::Edits) => vec!["-a".into(), "on-request".into()],
        (HarnessKind::Codex, RuntimeMode::Plan) => vec!["-s".into(), "read-only".into()],
        // Supervised is every harness's own default; custom harnesses get no flags.
        (_, RuntimeMode::Supervised) | (HarnessKind::Opencode | HarnessKind::Custom, _) => vec![],
    };
    if flags.is_empty() {
        return;
    }
    let mut args = flags;
    args.append(&mut spec.args);
    spec.args = args;
}

/// Give the harness a hooks file that routes events to `axel hook`.
fn inject_hooks(spec: &mut cfg_launch::LaunchSpec, kind: HarnessKind) -> Result<()> {
    match kind {
        HarnessKind::Claude => {
            let path = write_claude_hooks()?;
            let mut args = vec!["--settings".to_string(), path.to_string_lossy().into_owned()];
            args.append(&mut spec.args);
            spec.args = args;
        }
        HarnessKind::Codex => {
            let axel = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
            let mut args = vec!["-c".to_string(), format!(r#"notify=["{axel}","hook","codex"]"#)];
            args.append(&mut spec.args);
            spec.args = args;
        }
        _ => {}
    }
    Ok(())
}

/// Name of Axel's MCP server in agent sessions; its tools are `mcp__axel__*`.
pub const MCP_SERVER: &str = "axel";
/// Claude Code permission rule allowing every tool of [`MCP_SERVER`]: sending
/// and status are policed by Axel itself, so they never wait on a prompt.
pub const MCP_ALLOW: &str = "mcp__axel";

fn axel_exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into())
}

/// `axel mcp` for a managed session (ACP `mcpServers`), with the identity and
/// store variables from the session's environment `env` and nothing else.
pub fn axel_mcp_server(env: &[(String, String)]) -> axel_acp::McpStdio {
    const KEEP: &[&str] = &["PATH", "HOME", "AXEL_THREAD", "AXEL_SESSION", "AXEL_AGENT", "AXEL_AGENT_NAME", "AXEL_HOME", "AXEL_DB", "AXEL_KEYSTORE"];
    let mut vars: Vec<(String, String)> = Vec::new();
    for (k, v) in env.iter().filter(|(k, _)| KEEP.contains(&k.as_str())) {
        // Later entries win, like the environment they were pushed onto.
        vars.retain(|(o, _)| o != k);
        vars.push((k.clone(), v.clone()));
    }
    axel_acp::McpStdio { name: MCP_SERVER.into(), command: axel_exe().into(), args: vec!["mcp".into()], env: vars }
}

/// The `axel` skill every agent gets: who it is, how to reach other agents,
/// what refusals mean.  Compiled in, so it always matches this binary.
pub const AXEL_SKILL_MD: &str = include_str!("../skill/axel/SKILL.md");

/// `~/.axel/plugin`: a Claude Code plugin holding the `axel` skill, loaded per
/// session with `--plugin-dir` so nothing is written into the user's projects
/// or their own `~/.claude`.  Rewritten each launch so it tracks the binary.
pub fn write_axel_plugin() -> Result<PathBuf> {
    let dir = paths::home().join("plugin");
    std::fs::create_dir_all(dir.join(".claude-plugin"))?;
    std::fs::create_dir_all(dir.join("skills").join("axel"))?;
    let manifest = serde_json::json!({
        "name": "axel",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Working as an agent in Axel: messaging and summoning other agents, status, inbox, policy refusals."
    });
    std::fs::write(dir.join(".claude-plugin").join("plugin.json"), serde_json::to_vec_pretty(&manifest)?)?;
    std::fs::write(dir.join("skills").join("axel").join("SKILL.md"), AXEL_SKILL_MD)?;
    Ok(dir)
}

/// Appended to the harness's own system prompt, so the agent knows which
/// Axel agent it is (the thread shows its name; the model otherwise doesn't),
/// followed by the agent's own prompt when it has one.
pub fn identity_prompt(agent: &axel_store::Agent) -> String {
    let mut s = format!(
        "You are {name}, an agent in Axel. People on this thread call you {name}; answer to that name. \
         Your Axel agent pubkey is {pk}. To message or bring in another agent, use the `send` tool of the \
         `axel` MCP server (or `axel send \"@name …\"`); the `axel` skill explains how. Writing @name in \
         your reply alone does nothing. Ask the owner, not other agents, for anything only the owner knows \
         (about themselves, or their decisions). The owner reads every line of the thread: give results, \
         don't narrate what you're about to do.",
        name = agent.name,
        pk = agent.pubkey
    );
    if let Some(p) = agent.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        s.push_str("\n\n");
        s.push_str(p);
    }
    s
}

/// `~/.axel/hooks/claude.json`, regenerated each launch so it tracks the binary.
pub fn write_claude_hooks() -> Result<PathBuf> {
    let dir = paths::hooks_dir();
    std::fs::create_dir_all(&dir)?;
    let axel = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "axel".into());
    let hook = |timeout: u64| {
        serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("{axel} hook claude"), "timeout": timeout }] }])
    };
    let wait = hook_wait_secs();
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": hook(10),
            "UserPromptSubmit": hook(10),
            "PreToolUse": hook(10),
            "PostToolUse": hook(10),
            "Notification": hook(10),
            "PermissionRequest": hook(wait.max(10) + 5),
            "Stop": hook(10),
            "SubagentStop": hook(10),
            "SessionEnd": hook(10)
        }
    });
    let path = dir.join("claude.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
}

/// `~/.axel/hooks/claude-guard.json`: only the `PreToolUse` guard, for managed
/// sessions.  Their adapter runs in `bypassPermissions` under autopilot, so no
/// permission request reaches the host's policy; the guard hook still runs and
/// still denies the irreversible commands.  Status comes from the host itself.
pub fn write_claude_guard_hooks() -> Result<PathBuf> {
    let dir = paths::hooks_dir();
    std::fs::create_dir_all(&dir)?;
    let axel = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "axel".into());
    let settings = serde_json::json!({
        "permissions": { "allow": [MCP_ALLOW] },
        "hooks": {
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": format!("{axel} hook claude --guard-only"), "timeout": 10 }] }]
        }
    });
    let path = dir.join("claude-guard.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
}

/// How long a `PermissionRequest` hook waits for an answer from the Axel inbox
/// before letting Claude Code show its own prompt. 0 disables waiting.
pub fn hook_wait_secs() -> u64 {
    std::env::var("AXEL_HOOK_WAIT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceConfig, drivers, install_skills, normalize_repo_url};

    #[test]
    fn the_session_tool_server_gets_identity_but_not_the_prompt() {
        let env: Vec<(String, String)> = [
            ("PATH", "/bin"),
            ("AXEL_AGENT", "old"),
            ("AXEL_PROMPT", "a long prompt"),
            ("ANTHROPIC_API_KEY", "secret"),
            ("AXEL_AGENT", "pk"),
            ("AXEL_THREAD", "t1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let s = super::axel_mcp_server(&env);
        assert_eq!(s.name, "axel");
        assert_eq!(s.args, vec!["mcp".to_string()]);
        let keys: Vec<&str> = s.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["PATH", "AXEL_AGENT", "AXEL_THREAD"]);
        assert!(s.env.contains(&("AXEL_AGENT".into(), "pk".into())), "the later value wins");
    }

    #[test]
    fn every_agent_gets_the_global_skills_and_git_never_sees_them() {
        let global = tempfile::tempdir().unwrap();
        let skill = global.path().join("conventional-commits");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: conventional-commits\ndescription: Commit messages.\n---\nfeat: …\n").unwrap();
        std::fs::write(global.path().join("tone.md"), "Be brief.\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(repo.path()).args(args).output().unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("AXEL.md"), "Run the tests.\n").unwrap();
        let config = WorkspaceConfig::load(repo.path()).unwrap();
        for kind in [axel_config::HarnessKind::Claude, axel_config::HarnessKind::Codex] {
            let driver = drivers::for_harness(kind).unwrap();
            let names = install_skills(driver.as_ref(), repo.path(), &["*".to_string()], config.as_ref(), global.path()).unwrap();
            assert_eq!(names, vec!["conventional-commits".to_string(), "tone".to_string()], "{kind:?}: `*` is all of them");
            let linked = driver.skills_dir(repo.path()).join("conventional-commits").join("SKILL.md");
            assert!(std::fs::read_to_string(&linked).unwrap().contains("feat:"), "{kind:?}: the skill is where the harness looks");
        }
        // The links are this machine's: git doesn't see them, or the linked context.
        assert_eq!(git(&["status", "--porcelain"]).trim(), "?? AXEL.md", "only the project's own file shows");
        // Installing again adds no lines.
        let driver = drivers::for_harness(axel_config::HarnessKind::Claude).unwrap();
        install_skills(driver.as_ref(), repo.path(), &["*".to_string()], config.as_ref(), global.path()).unwrap();
        let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/.claude/skills/conventional-commits").count(), 1, "{exclude}");
        // An agent given named skills gets those.
        let named = install_skills(driver.as_ref(), repo.path(), &["tone".to_string()], config.as_ref(), global.path()).unwrap();
        assert_eq!(named, vec!["tone".to_string()]);
    }

    #[test]
    fn the_global_skills_live_in_dot_config() {
        assert!(crate::paths::global_skills_dir().ends_with(".config/axel/skills"), "{:?}", crate::paths::global_skills_dir());
    }

    #[test]
    fn a_harness_axel_cant_link_axel_md_for_gets_it_in_its_prompt() {
        use axel_store::Harness;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AXEL.md"), "Run the tests before you say done.\n").unwrap();
        let body = super::unlinked_context(&Harness::Goose, dir.path());
        assert!(body.is_some_and(|b| b.contains("Run the tests")), "goose has no context file Axel links");
        std::os::unix::fs::symlink("AXEL.md", dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(super::unlinked_context(&Harness::Claude, dir.path()), None, "claude reads it through CLAUDE.md");
        assert!(super::unlinked_context(&Harness::Cursor, dir.path()).is_some());
    }

    #[test]
    fn repo_keys_match_across_url_forms() {
        let a = normalize_repo_url("git@github.com:lgalabru/axel.git");
        let b = normalize_repo_url("https://github.com/lgalabru/axel");
        let c = normalize_repo_url("ssh://git@GitHub.com/lgalabru/axel.git");
        assert_eq!(a, "github.com/lgalabru/axel");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }
}
