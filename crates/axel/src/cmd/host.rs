//! Remote hosts: machines that run agents for this one (`axel host`), and
//! replication with them (`axel sync`).
//!
//! The laptop drives everything over SSH: each call runs `axel host rpc` on
//! the host with one JSON request on stdin and one JSON reply on stdout,
//! sharing one SSH connection (ControlMaster).  The host keeps its own store;
//! the two exchange envelopes (signed, so a replica can't forge the other's)
//! and the conversation items of the sessions the host runs.
//!
//! Agents' private keys never leave home.  Starting a remote job, the host
//! makes a key for that job and the agent signs a delegation to it for that
//! one thread, for [`DELEGATION_SECS`]; the host signs the session's events
//! with the job key plus the delegation, and every replica attributes them
//! to the agent.
//!
//! Code travels by git over the same SSH connection: the laptop pushes a
//! snapshot commit of its working tree (uncommitted and untracked files
//! included, ignored ones not) into a bare repo on the host; the job works
//! in a worktree on `axel/<thread>-<agent>`; syncing fetches that branch,
//! and a snapshot of the host's uncommitted work, back.
//!
//! A Claude session can also move between this machine and a host
//! (`axel session move`): the session stops, its code and its Claude
//! conversation file travel, and a new session on the same thread resumes
//! that conversation where the other one stopped.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use axel_acp::socket;
use axel_inbox::Envelope;
use axel_store::{Environment, ItemChange, SessionState, SyncStream};
use serde::{Deserialize, Serialize};

use super::{Ctx, launch};
use crate::paths;

/// How long a remote job may sign for its agent.
pub const DELEGATION_SECS: u64 = 7 * 24 * 3600;
/// Batch size for event and item transfers.
const BATCH: usize = 500;
/// `PATH` on the host: non-interactive SSH sessions get only the system dirs.
const REMOTE_PATH: &str = "/opt/homebrew/bin:/opt/homebrew/sbin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// The Rex CLI inside the Rex app bundle, when `rex` isn't on `PATH`.
const REX_IN_APP: &str = "Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex";
/// The Rex session remote jobs run in, on the host.
const REX_SESSION: &str = "axel";

// ===================================================================== ssh

fn ssh_opts() -> Vec<String> {
    // Unix socket paths are short (104 bytes on macOS); `%C` adds 40.
    let mut dir = paths::home().join("ssh");
    if dir.as_os_str().len() > 60 {
        // SAFETY: getuid can't fail.
        dir = PathBuf::from(format!("/tmp/axel-ssh-{}", unsafe { libc::getuid() }));
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}/%C", dir.display()),
        "-o".into(),
        "ControlPersist=600".into(),
    ]
}

/// `ssh` as a single string, for `GIT_SSH_COMMAND`.
fn ssh_command_line() -> String {
    let mut s = String::from("ssh");
    for o in ssh_opts() {
        s.push(' ');
        s.push_str(&shell_quote(&o));
    }
    s
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:%@,+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Run `remote_cmd` (a shell command line) on `target`, feeding `stdin`.
fn ssh(target: &str, remote_cmd: &str, stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("ssh")
        .args(ssh_opts())
        .arg(target)
        .arg(remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    child.stdin.take().unwrap().write_all(stdin)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("ssh {target}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `axel …` on the host, with a usable PATH and the file keystore (the
/// macOS keychain can't be unlocked from an SSH session).
///
/// `$AXEL_REMOTE_HOME` points the host's Axel at another home directory
/// (for trying things out without touching the host's real one).
fn remote_axel(args: &str) -> String {
    let home = std::env::var("AXEL_REMOTE_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|h| format!(" AXEL_HOME={}", shell_quote(&h)))
        .unwrap_or_default();
    format!("env PATH=\"{REMOTE_PATH}\" AXEL_KEYSTORE=file{home} \"$HOME/.local/bin/axel\" {args}")
}

// ===================================================================== rpc

/// One request to a host.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello,
    PushEvents { events: Vec<Envelope> },
    PullEvents { cursor: i64 },
    PullItems { cursor: i64 },
    PrepareRepo { repo: String },
    /// Make a key for a job (the agent delegates to it).
    JobKey,
    StartJob { job: Box<Job> },
    SessionCommand {
        session_id: String,
        cmd: socket::Command,
        /// The prompt's attachments, carried along (their paths are ours).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileBlob>,
    },
    StopSession { session_id: String },
    PauseSession { session_id: String, paused: bool },
    Snapshot { thread_id: String },
    /// A session's harness conversation, to resume it elsewhere.
    Transcript { session_id: String },
}

/// A host's reply.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Hello { environment_id: String, label: String, host_pubkey: String, version: String, missing: Vec<String> },
    Pushed { applied: usize },
    Events { events: Vec<(i64, Envelope)> },
    Items { changes: Vec<ItemChange> },
    Repo { path: String },
    JobKey { pubkey: String },
    Started { session_id: String },
    Reply { reply: socket::Reply },
    Branches { branches: Vec<RemoteBranch> },
    Transcript { transcript: Option<Transcript> },
    Error { message: String },
}

/// A file sent along with a prompt (an attachment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBlob {
    pub name: String,
    /// Base64.
    pub data: String,
}

/// A Claude conversation file, so another machine can resume it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// Claude's session id (the file is `<id>.jsonl`).
    pub native_id: String,
    pub jsonl: String,
}

/// The agent a job runs as, as the host needs to know it (no key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAgent {
    pub pubkey: String,
    pub name: String,
    pub harness: axel_store::Harness,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub color: Option<axel_store::AgentColor>,
    pub auth_tag: Option<Vec<String>>,
}

impl JobAgent {
    fn of(a: &axel_store::Agent) -> JobAgent {
        JobAgent {
            pubkey: a.pubkey.clone(),
            name: a.name.clone(),
            harness: a.harness.clone(),
            model: a.model.clone(),
            skills: a.skills.clone(),
            prompt: a.prompt.clone(),
            color: a.color,
            auth_tag: a.auth_tag.clone(),
        }
    }

    fn insert_if_missing(&self, ctx: &Ctx, mode: axel_store::RuntimeMode) -> Result<()> {
        if ctx.store.agent_by_pubkey(&self.pubkey)?.is_none() {
            ctx.store.insert_agent(&axel_store::NewAgent {
                pubkey: self.pubkey.clone(),
                name: self.name.clone(),
                harness: self.harness.clone(),
                model: self.model.clone(),
                skills: self.skills.clone(),
                prompt: self.prompt.clone(),
                color: self.color,
                runtime_mode: mode,
                auth_tag: self.auth_tag.clone(),
            })?;
        }
        Ok(())
    }
}

/// Where a job works: a branch in the host's copy of a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobWorkspace {
    /// Directory name of the bare repo under `~/.axel/repos`.
    pub repo: String,
    /// The commit the job starts from (the laptop's working state).
    pub start: String,
    /// The branch the job works on.
    pub branch: String,
    /// Put the branch at `start` even if the host already has it: a moved
    /// session's newest work is on the laptop.
    #[serde(default)]
    pub reset: bool,
}

/// Everything a host needs to start an agent on a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub thread_id: String,
    pub title: String,
    pub agent: JobAgent,
    /// The job's key on the host (from [`Request::JobKey`]).
    pub job_key: String,
    /// The agent's delegation to the job key, for this thread.
    pub delegation: Vec<String>,
    pub prompt: String,
    /// The agent that asked for this job, if any.
    pub from: Option<socket::Sender>,
    pub workspace: Option<JobWorkspace>,
    #[serde(default)]
    pub attachments: Vec<FileBlob>,
    /// Resume this conversation instead of starting one (a moved session).
    #[serde(default)]
    pub resume: Option<Transcript>,
    /// The agent's policy, enforced on the host too.
    #[serde(default)]
    pub reach: axel_store::Reach,
    #[serde(default)]
    pub budget: axel_store::Budget,
    /// The agents it may reach, so `@name` resolves on the host.
    #[serde(default)]
    pub peers: Vec<JobAgent>,
}

/// A job branch on the host, and a snapshot of its uncommitted work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBranch {
    pub branch: String,
    pub head: String,
    /// A commit of the worktree's uncommitted state, if it has any.
    pub wip: Option<String>,
}

fn call(host: &Environment, req: &Request) -> Result<Response> {
    let target = host.ssh_target.as_deref().with_context(|| format!("host {} has no ssh target", host.label))?;
    let out = ssh(target, &remote_axel("host rpc"), &serde_json::to_vec(req)?)?;
    let resp: Response = serde_json::from_slice(&out)
        .with_context(|| format!("bad reply from {}: {}", host.label, String::from_utf8_lossy(&out).chars().take(300).collect::<String>()))?;
    if let Response::Error { message } = resp {
        bail!("{}: {message}", host.label);
    }
    Ok(resp)
}

/// `axel host rpc` (on the host): one request on stdin, one reply on stdout.
pub fn rpc_serve() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let resp = match serde_json::from_slice::<Request>(&input) {
        Ok(req) => handle(req).unwrap_or_else(|e| Response::Error { message: format!("{e:#}") }),
        Err(e) => Response::Error { message: format!("bad request: {e}") },
    };
    println!("{}", serde_json::to_string(&resp)?);
    Ok(())
}

fn handle(req: Request) -> Result<Response> {
    let ctx = Ctx::open()?;
    Ok(match req {
        Request::Hello => {
            let env = ctx.store.local_environment()?;
            // The Claude adapter is built into axel; node only matters to install Claude Code.
            let mut missing: Vec<String> = ["git", "claude"].into_iter().filter(|p| launch::which(p).is_none()).map(String::from).collect();
            if missing.iter().any(|m| m == "claude") && launch::which("node").is_none() {
                missing.push("node".into());
            }
            Response::Hello {
                environment_id: env.id,
                label: env.label,
                host_pubkey: ctx.owner()?.public().to_base58(),
                version: env!("CARGO_PKG_VERSION").into(),
                missing,
            }
        }
        Request::PushEvents { events } => {
            let mut applied = 0;
            for e in &events {
                if ctx.store.append(e)?.projected {
                    applied += 1;
                }
            }
            Response::Pushed { applied }
        }
        Request::PullEvents { cursor } => Response::Events { events: ctx.store.thread_events_since(cursor, None, BATCH)? },
        Request::PullItems { cursor } => {
            let env = ctx.store.local_environment()?;
            Response::Items { changes: ctx.store.item_changes_since(&env.id, cursor, BATCH)? }
        }
        Request::PrepareRepo { repo } => {
            let path = repo_dir(&repo)?;
            if !path.join("HEAD").exists() {
                std::fs::create_dir_all(&path)?;
                git(&path, &["init", "--bare", "--quiet"])?;
            }
            Response::Repo { path: path.to_string_lossy().into_owned() }
        }
        Request::JobKey => {
            let kp = axel_identity::Keypair::generate();
            ctx.keystore.put_agent(&kp)?;
            Response::JobKey { pubkey: kp.public().to_base58() }
        }
        Request::StartJob { job } => Response::Started { session_id: start_job(&ctx, &job)? },
        Request::SessionCommand { session_id, mut cmd, files } => {
            let s = ctx.store.get_session(&session_id)?.with_context(|| format!("no session {session_id}"))?;
            if s.state == axel_store::SessionState::Paused { return Ok(Response::Reply { reply: socket::Reply::Error { message: "this agent is paused; resume it in thread controls".into() } }); }
            if let socket::Command::Prompt { attachments, .. } = &mut cmd
                && !files.is_empty()
            {
                *attachments = unpack_files(&paths::home().join("attachments").join(&session_id), &files)?;
            }
            let sock = s.socket_path.context("its session host isn't running")?;
            Response::Reply { reply: socket::send(&sock, &cmd).context("its session host is unreachable")? }
        }
        Request::PauseSession { session_id, paused } => {
            let session = ctx.store.get_session(&session_id)?.context("no such session")?;
            if paused { super::process_control::pause(&ctx, &session)?; }
            else { super::process_control::resume(&ctx, &session)?; }
            Response::Reply { reply: socket::Reply::Ok }
        }
        Request::StopSession { session_id } => {
            if let Some(session) = ctx.store.get_session(&session_id)? {
                if session.state == axel_store::SessionState::Paused { super::process_control::resume(&ctx, &session)?; }
            }
            if let Some(sock) = ctx.store.get_session(&session_id)?.and_then(|s| s.socket_path) {
                let _ = socket::send(&sock, &socket::Command::Stop);
            }
            Response::Reply { reply: socket::Reply::Ok }
        }
        Request::Snapshot { thread_id } => Response::Branches { branches: snapshot_branches(&thread_id)? },
        Request::Transcript { session_id } => {
            let s = ctx.store.get_session(&session_id)?.with_context(|| format!("no session {session_id}"))?;
            let transcript = match &s.harness_session_id {
                Some(id) => read_transcript(id, s.transcript_path.as_deref())?,
                None => None,
            };
            Response::Transcript { transcript }
        }
    })
}

// ================================================================== setup

/// `axel host add LABEL [--ssh TARGET]`: install Axel on the host, check
/// what it needs, and record it here.
pub fn add(label: &str, ssh_target: Option<&str>, install: bool) -> Result<()> {
    let ctx = Ctx::open()?;
    let target = ssh_target.unwrap_or(label);
    let here = String::from_utf8(Command::new("uname").arg("-sm").output()?.stdout)?.trim().to_string();
    let there = String::from_utf8(ssh(target, "uname -sm", b"")?)?.trim().to_string();
    if here != there {
        bail!("{target} is {there}, this machine is {here}: copying this axel binary won't run there");
    }
    println!("{target}: {there}, reachable");

    let exe = std::fs::read(std::env::current_exe()?)?;
    ssh(
        target,
        "mkdir -p .local/bin && cat > .local/bin/axel.new && chmod +x .local/bin/axel.new && mv .local/bin/axel.new .local/bin/axel",
        &exe,
    )?;
    println!("installed axel {} in ~/.local/bin", env!("CARGO_PKG_VERSION"));

    let skills = paths::global_skills_dir();
    if skills.is_dir() {
        copy_dir(target, &skills)?;
        println!("copied your skills ({})", skills.display());
    }

    let probe = Environment {
        id: String::new(),
        label: label.into(),
        rex_host_id: None,
        is_local: false,
        created_at: 0,
        ssh_target: Some(target.into()),
        host_pubkey: None,
    };
    let Response::Hello { environment_id, label: remote_label, host_pubkey, missing, .. } = call(&probe, &Request::Hello)? else {
        bail!("unexpected reply to hello");
    };
    let mut missing = missing;
    if !missing.is_empty() && install {
        install_deps(target, &missing)?;
        if let Response::Hello { missing: m, .. } = call(&probe, &Request::Hello)? {
            missing = m;
        }
    }
    let rex_host = rex_host_label(label);
    ctx.store.put_host(&environment_id, label, target, &host_pubkey, rex_host.as_deref())?;
    println!("{label}: host key {host_pubkey} ({remote_label})");
    if rex_host.is_some() {
        println!("Rex host {label}: remote jobs will run in its \"{REX_SESSION}\" session");
    }
    if missing.is_empty() {
        println!("ready. If Claude Code has never run on {target}, log in once: rex -S {label} run -- claude");
    } else {
        println!("missing on {target}: {}. Rerun with --install, or install them yourself.", missing.join(", "));
    }
    Ok(())
}

fn install_deps(target: &str, missing: &[String]) -> Result<()> {
    let mut steps: Vec<&str> = Vec::new();
    if missing.iter().any(|m| m == "node") {
        steps.push("brew install node");
    }
    if missing.iter().any(|m| m == "claude") {
        steps.push("npm install -g @anthropic-ai/claude-code");
    }
    if missing.iter().any(|m| m == "git") {
        bail!("git is missing on {target}: install the Xcode command line tools there (xcode-select --install)");
    }
    for step in steps {
        println!("{target}: {step}");
        ssh(target, &format!("env PATH=\"{REMOTE_PATH}\" {step}"), b"")?;
    }
    Ok(())
}

/// The Rex host whose label matches, if Rex knows one.
fn rex_host_label(label: &str) -> Option<String> {
    let out = Command::new("rex").args(["hosts", "list", "--json"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let hosts = v.as_array().cloned().or_else(|| v.get("hosts").and_then(|h| h.as_array()).cloned())?;
    // `{"hosts": [{"host": {"label": …}}]}`; older builds put the label at the top.
    hosts
        .iter()
        .any(|h| h.pointer("/host/label").or_else(|| h.get("label")).and_then(|l| l.as_str()) == Some(label))
        .then(|| label.to_string())
}

/// Copy `dir` to the same path under the host's home.
fn copy_dir(target: &str, dir: &Path) -> Result<()> {
    let home = dirs::home_dir().context("no home directory")?;
    let rel = dir.strip_prefix(&home).with_context(|| format!("{} is outside your home", dir.display()))?;
    let tar = Command::new("tar").arg("-C").arg(dir).args(["-cf", "-", "."]).output()?;
    if !tar.status.success() {
        bail!("tar {}: {}", dir.display(), String::from_utf8_lossy(&tar.stderr));
    }
    let rel = shell_quote(&rel.to_string_lossy());
    ssh(target, &format!("mkdir -p {rel} && tar -C {rel} -xf -"), &tar.stdout)?;
    Ok(())
}

pub fn list() -> Result<()> {
    let ctx = Ctx::open()?;
    let hosts = ctx.store.hosts()?;
    if hosts.is_empty() {
        println!("no hosts; add one with `axel host add LABEL`");
    }
    for h in hosts {
        let live = live_sessions_on(&ctx, &h)?.len();
        println!(
            "{:<10} ssh {:<14} {:<4} {} live session{}",
            h.label,
            h.ssh_target.as_deref().unwrap_or("-"),
            if h.rex_host_id.is_some() { "rex" } else { "" },
            live,
            if live == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

pub fn remove(label: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    if !ctx.store.remove_host(label)? {
        bail!("no host named {label:?}");
    }
    println!("removed {label}; its delegations expire on their own");
    Ok(())
}

// ================================================================== sync

/// What a sync moved, for people.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    pub items: usize,
    pub branches: Vec<String>,
}

/// Exchange envelopes and items with `host`; with `with_git`, also fetch the
/// job branches of shared threads.
pub fn sync_host(ctx: &Ctx, host: &Environment, with_git: bool) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let shared = ctx.store.shared_threads(&host.id)?;
    let mut cursor = ctx.store.peer_cursor(&host.id, SyncStream::EventsOut)?;
    loop {
        let batch = ctx.store.thread_events_since(cursor, Some(&shared), BATCH)?;
        let Some((last, _)) = batch.last() else { break };
        let last = *last;
        let n = batch.len();
        call(host, &Request::PushEvents { events: batch.into_iter().map(|(_, e)| e).collect() })?;
        report.pushed += n;
        cursor = last;
        ctx.store.set_peer_cursor(&host.id, SyncStream::EventsOut, cursor)?;
        if n < BATCH {
            break;
        }
    }
    let mut cursor = ctx.store.peer_cursor(&host.id, SyncStream::EventsIn)?;
    let mut notes = Vec::new();
    loop {
        let Response::Events { events } = call(host, &Request::PullEvents { cursor })? else { bail!("unexpected reply") };
        let n = events.len();
        for (seq, e) in &events {
            let r = ctx.store.append(e)?;
            if r.inserted {
                report.pulled += 1;
                if r.projected && e.kind == axel_inbox::kind::NOTE {
                    notes.push(e.clone());
                }
            }
            cursor = *seq;
        }
        ctx.store.set_peer_cursor(&host.id, SyncStream::EventsIn, cursor)?;
        if n < BATCH {
            break;
        }
    }
    let mut cursor = ctx.store.peer_cursor(&host.id, SyncStream::ItemsIn)?;
    loop {
        let Response::Items { changes } = call(host, &Request::PullItems { cursor })? else { bail!("unexpected reply") };
        let n = changes.len();
        for c in &changes {
            ctx.store.import_item_change(c)?;
            cursor = c.rev();
        }
        report.items += n;
        ctx.store.set_peer_cursor(&host.id, SyncStream::ItemsIn, cursor)?;
        if n < BATCH {
            break;
        }
    }
    // Messages agents there sent to agents that live here: the host could
    // only leave them in the inbox.
    for n in &notes {
        for line in super::inbox::deliver_from_host(ctx, host, n) {
            eprintln!("{line}");
        }
    }
    if with_git {
        for t in &shared {
            report.branches.extend(fetch_results(ctx, host, t)?);
        }
    }
    Ok(report)
}

/// `axel sync [HOST] [--follow]`.
pub fn sync_cmd(label: Option<&str>, follow: bool) -> Result<()> {
    let ctx = Ctx::open()?;
    let hosts: Vec<Environment> = match label {
        Some(l) => vec![ctx.store.host(l)?.with_context(|| format!("no host named {l:?}"))?],
        None => ctx.store.hosts()?,
    };
    if follow {
        let host = hosts.into_iter().next().context("no host to follow")?;
        return follow_host(&ctx, &host);
    }
    for h in &hosts {
        let r = sync_host(&ctx, h, true)?;
        println!("{}: sent {}, received {} events and {} item changes", h.label, r.pushed, r.pulled, r.items);
        for b in r.branches {
            println!("  {b}");
        }
    }
    Ok(())
}

/// Keep syncing with `host` while it runs sessions for us: every couple of
/// seconds (so thread views stream), git every half minute and when a
/// session goes idle or ends.  Survives sleep: failed rounds back off and
/// retry.  Exits a minute after the last live session ends.
fn follow_host(ctx: &Ctx, host: &Environment) -> Result<()> {
    let _lock = match FollowLock::take(&host.id)? {
        Some(l) => l,
        None => return Ok(()), // another follower runs
    };
    let mut last_git = std::time::Instant::now() - std::time::Duration::from_secs(3600);
    let mut idle_since: Option<std::time::Instant> = None;
    let mut backoff = 2u64;
    let mut states: std::collections::HashMap<String, SessionState> = Default::default();
    loop {
        let live = live_sessions_on(ctx, host)?;
        let changed = live.iter().any(|s| states.get(&s.id).is_some_and(|old| *old != s.state)) || live.len() < states.len();
        let git_due = changed || last_git.elapsed().as_secs() >= 30;
        match sync_host(ctx, host, git_due) {
            Ok(_) => {
                backoff = 2;
                if git_due {
                    last_git = std::time::Instant::now();
                }
            }
            Err(_) => backoff = (backoff * 2).min(60),
        }
        states = live_sessions_on(ctx, host)?.into_iter().map(|s| (s.id, s.state)).collect();
        if states.is_empty() {
            let since = *idle_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed().as_secs() >= 60 {
                let _ = sync_host(ctx, host, true);
                return Ok(());
            }
        } else {
            idle_since = None;
        }
        std::thread::sleep(std::time::Duration::from_secs(backoff));
    }
}

/// One follower per host: a pid file under `~/.axel/run`.
struct FollowLock(PathBuf);

impl FollowLock {
    fn take(host_id: &str) -> Result<Option<FollowLock>> {
        let dir = paths::home().join("run");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("sync-{host_id}.pid"));
        if let Ok(pid) = std::fs::read_to_string(&path)
            && let Ok(pid) = pid.trim().parse::<i32>()
            && pid != std::process::id() as i32
            // SAFETY: signal 0 only checks that the process exists.
            && unsafe { libc::kill(pid, 0) } == 0
        {
            return Ok(None);
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Some(FollowLock(path)))
    }
}

impl Drop for FollowLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Start a background follower for `host` (no-op when one runs).
pub fn ensure_follower(host: &Environment) {
    let Ok(exe) = std::env::current_exe() else { return };
    let log = paths::home().join("logs").join(format!("sync-{}.log", host.label));
    let _ = std::fs::create_dir_all(log.parent().unwrap());
    let out = std::fs::File::create(&log).ok();
    let mut cmd = Command::new(exe);
    cmd.args(["sync", &host.label, "--follow"]).stdin(Stdio::null());
    match out.and_then(|f| f.try_clone().ok().map(|g| (f, g))) {
        Some((a, b)) => {
            cmd.stdout(a).stderr(b);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let _ = cmd.spawn();
}

/// Followers for every host that still runs sessions for us (after a
/// reboot, say).  Called when the UI starts.
pub fn resume_followers(ctx: &Ctx) {
    for h in ctx.store.hosts().unwrap_or_default() {
        if live_sessions_on(ctx, &h).is_ok_and(|s| !s.is_empty()) {
            ensure_follower(&h);
        }
    }
}

/// Unended sessions that run on `host`.
pub fn live_sessions_on(ctx: &Ctx, host: &Environment) -> Result<Vec<axel_store::Session>> {
    let all = ctx.store.list_sessions(axel_store::SessionFilter::default())?;
    Ok(all.into_iter().filter(|s| s.environment_id == host.id && s.ended_at.is_none()).collect())
}

/// The host a session runs on, when it isn't this machine.
pub fn host_of(ctx: &Ctx, session: &axel_store::Session) -> Result<Option<Environment>> {
    Ok(ctx.store.environment(&session.environment_id)?.filter(|e| !e.is_local && e.ssh_target.is_some()))
}

/// Send a session command to a session running on its host.
pub fn relay(ctx: &Ctx, session: &axel_store::Session, cmd: &socket::Command) -> Result<socket::Reply> {
    let host = host_of(ctx, session)?.context("not a remote session")?;
    let mut cmd = cmd.clone();
    let files = match &mut cmd {
        socket::Command::Prompt { attachments, .. } => pack_files(&std::mem::take(attachments))?,
        _ => vec![],
    };
    let resp = call(&host, &Request::SessionCommand { session_id: session.id.clone(), cmd, files })?;
    ensure_follower(&host);
    match resp {
        Response::Reply { reply } => Ok(reply),
        _ => bail!("unexpected reply"),
    }
}

pub fn pause_remote(ctx: &Ctx, session: &axel_store::Session, paused: bool) -> Result<()> {
    let host = host_of(ctx, session)?.context("not a remote session")?;
    call(&host, &Request::PauseSession { session_id: session.id.clone(), paused })?;
    Ok(())
}

/// Stop a session on its host.
pub fn stop_remote(ctx: &Ctx, session: &axel_store::Session) -> Result<()> {
    let host = host_of(ctx, session)?.context("not a remote session")?;
    call(&host, &Request::StopSession { session_id: session.id.clone() })?;
    Ok(())
}

// ============================================================ remote jobs

/// Where an agent runs.
#[derive(Debug)]
pub enum Placement {
    Here,
    Host(Environment),
}

/// Where to run `agent`: the host asked for (`local` or this machine's
/// label means here), else the agent's default host, else here.
pub fn placement(ctx: &Ctx, agent: &str, asked: Option<&str>) -> Result<Placement> {
    if let Some(h) = asked.map(str::trim).filter(|h| !h.is_empty()) {
        if is_here(ctx, h)? {
            return Ok(Placement::Here);
        }
        let host = ctx.store.host(h)?.with_context(|| format!("no host named {h:?}; add it with `axel host add {h}`"))?;
        return Ok(Placement::Host(host));
    }
    // Delegated work runs where it was sent; only the agent's home re-routes.
    let home = agent.parse().ok().and_then(|k| ctx.keystore.agent(&k).ok().flatten()).is_some();
    Ok(match ctx.store.agent_host(agent)? {
        Some(h) if home => Placement::Host(h),
        _ => Placement::Here,
    })
}

/// `local`, `here`, or this machine's own label.
pub fn is_here(ctx: &Ctx, label: &str) -> Result<bool> {
    Ok(label.eq_ignore_ascii_case("local") || label.eq_ignore_ascii_case("here") || ctx.store.local_environment()?.label == label)
}

/// Start `agent_name` on `thread_id` on host `host_label`, with `prompt`
/// and `files` attached.  Called where a local summon would be: same policy
/// checks, then the thread, the agent's delegation and the code go to the
/// host.
pub fn launch_remote(ctx: &Ctx, host_label: &str, thread_id: &str, agent_name: &str, prompt: &str, files: &[PathBuf]) -> Result<String> {
    let host = ctx.store.host(host_label)?.with_context(|| format!("no host named {host_label:?}; add it with `axel host add {host_label}`"))?;
    start_remote(ctx, &host, thread_id, agent_name, prompt, files, None)
}

/// A conversation to resume on the host, and the directory whose state
/// goes with it.
struct Carry {
    transcript: Transcript,
    work_dir: Option<PathBuf>,
}

fn start_remote(ctx: &Ctx, host: &Environment, thread_id: &str, agent_name: &str, prompt: &str, files: &[PathBuf], carry: Option<Carry>) -> Result<String> {
    let thread = ctx.store.get_thread(thread_id)?.with_context(|| format!("no thread {thread_id}"))?;
    let agent = ctx.store.agent_by_name(agent_name)?.with_context(|| format!("no agent named {agent_name:?}"))?;
    super::policy::check_summon(ctx, &agent.pubkey).with_context(|| format!("refusing to launch {}", agent.name))?;
    let agent_kp = ctx
        .keystore
        .agent(&agent.pubkey.parse()?)?
        .with_context(|| format!("{}'s key isn't on this machine, so it can't delegate", agent.name))?;
    let attachments = pack_files(files)?;

    // The thread, in full (the host may have none of it).
    ctx.store.share_thread(&host.id, &thread.id)?;
    let events = ctx.store.thread_events(&thread.id)?;
    for chunk in events.chunks(BATCH) {
        call(host, &Request::PushEvents { events: chunk.to_vec() })?;
    }

    // The code: where a moved session worked, else the thread's project.
    let moved_from = carry.as_ref().and_then(|c| c.work_dir.clone()).filter(|d| d.join(".git").exists() || git(d, &["rev-parse", "--git-dir"]).is_ok());
    let workspace = match moved_from.or(local_project_dir(ctx, &thread.id)?) {
        Some(dir) if git(&dir, &["rev-parse", "--git-dir"]).is_ok() => {
            let mut ws = push_workspace(host, &dir, &thread, &agent.name)?;
            ws.reset = carry.is_some();
            if ws.reset && dir.starts_with(paths::worktrees_dir()) {
                // A worktree Axel made for a session that moved back: its
                // work now continues on the host, so it goes back to clean
                // (kept under refs/axel/sent/…) and can take the work again
                // when it returns.
                git(&dir, &["update-ref", &format!("refs/axel/sent/{}", ws.branch), &ws.start])?;
                git(&dir, &["reset", "--quiet", "--hard"])?;
                git(&dir, &["clean", "--quiet", "-fd"])?;
            }
            Some(ws)
        }
        Some(dir) => bail!("{} isn't a git repository; remote agents need git to carry the code", dir.display()),
        None => None,
    };

    let Response::JobKey { pubkey: job_key } = call(host, &Request::JobKey)? else { bail!("unexpected reply") };
    let now = axel_tui::util::now_secs() as u64;
    let delegation = axel_identity::delegate(&agent_kp, &job_key.parse()?, &thread.id, now.saturating_sub(300), now + DELEGATION_SECS)?;
    let from = super::policy::caller_agent()
        .filter(|pk| *pk != agent.pubkey)
        .map(|pk| socket::Sender::new(pk.clone(), super::policy::name_of(ctx, &pk)));
    let policy = ctx.store.agent_policy(&agent.pubkey)?;
    let peers = ctx
        .store
        .list_agents()?
        .iter()
        .filter(|a| a.pubkey != agent.pubkey && policy.reach.allows(&a.pubkey))
        .map(JobAgent::of)
        .collect();
    let job = Job {
        thread_id: thread.id.clone(),
        title: thread.title.clone(),
        agent: JobAgent::of(&agent),
        job_key,
        delegation: delegation.to_tag(),
        prompt: remote_prompt(prompt, workspace.as_ref(), &host.label),
        from,
        workspace,
        attachments,
        resume: carry.map(|c| c.transcript),
        reach: policy.reach,
        budget: policy.budget,
        peers,
    };
    let Response::Started { session_id } = call(host, &Request::StartJob { job: Box::new(job) })? else {
        bail!("unexpected reply");
    };
    let _ = sync_host(ctx, host, false);
    ensure_follower(host);
    Ok(session_id)
}

/// Start a new thread whose first agent runs on `host`: the thread and its
/// projects are recorded here, then the job goes out.
pub fn new_thread_remote(ctx: &Ctx, host: &str, title: &str, projects: &[PathBuf], agent: &str, files: &[PathBuf]) -> Result<(String, String)> {
    let thread = super::thread::record_thread(ctx, title, projects)?;
    let session = launch_remote(ctx, host, &thread.id, agent, &thread.intent, files)?;
    Ok((thread.id, session))
}

/// The first prompt of a remote job: the request, then where the agent is
/// and how its work gets back.
fn remote_prompt(prompt: &str, ws: Option<&JobWorkspace>, host: &str) -> String {
    let mut s = prompt.to_string();
    s.push_str(&format!("\n\n---\nYou are running on the remote host \"{host}\" while the owner is away; nobody can answer permission prompts, so don't wait for one."));
    if let Some(ws) = ws {
        s.push_str(&format!(
            " You work in a copy of the repo on branch `{}`, which started from the owner's working state. \
             Commit your work there as you go: the owner gets the branch (and any uncommitted changes) when they sync. Don't push to other remotes.",
            ws.branch
        ));
    }
    s
}

// ========================================================== moving sessions

const MOVED_THERE: &str = "You were moved to another machine so you can keep working while the owner is away. \
Your conversation so far is above; the files are the same, including uncommitted changes. Continue the task where you left off.";

/// `axel session move SESSION --to HOST|local`: stop a Claude session and
/// resume its conversation, with its code, on the other machine.  Returns
/// the new session's id and where it runs.
pub fn move_session(ctx: &Ctx, session_id: &str, to: &str) -> Result<(String, String)> {
    let s = ctx.store.get_session(session_id)?.with_context(|| format!("no session {session_id}"))?;
    if s.ended_at.is_some() {
        bail!("that session has ended; start a new one with `axel thread spawn` or `axel thread resume`");
    }
    let agent = ctx.store.agent_by_pubkey(&s.agent_pubkey)?.context("agent vanished")?;
    if agent.harness != axel_store::Harness::Claude {
        bail!("only Claude sessions can move: Axel carries their conversation file");
    }
    let native = s.harness_session_id.clone().context("this session hasn't started its conversation yet; there's nothing to carry")?;
    let here = ctx.store.local_environment()?;
    match (host_of(ctx, &s)?, placement(ctx, &agent.pubkey, Some(to))?) {
        (None, Placement::Here) => bail!("{} already runs here", agent.name),
        (Some(h), Placement::Host(d)) if h.id == d.id => bail!("{} already runs on {}", agent.name, h.label),
        (Some(h), Placement::Host(d)) => bail!("{} runs on {}; bring it here first (`--to local`), then send it to {}", agent.name, h.label, d.label),
        (None, Placement::Host(dest)) => {
            stop_local(ctx, &s);
            ctx.store.end_session(&ctx.owner()?, &s.id, Some(&format!("moved to {}", dest.label)))?;
            let transcript = wait_for_transcript(&native, s.transcript_path.as_deref())?;
            let work_dir = transcript_cwd(&transcript.jsonl);
            let id = start_remote(ctx, &dest, &s.thread_id, &agent.name, MOVED_THERE, &[], Some(Carry { transcript, work_dir }))?;
            Ok((id, dest.label))
        }
        (Some(from), Placement::Here) => {
            call(&from, &Request::StopSession { session_id: s.id.clone() })?;
            ctx.store.end_session(&ctx.owner()?, &s.id, Some(&format!("moved to {}", here.label)))?;
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let Response::Transcript { transcript } = call(&from, &Request::Transcript { session_id: s.id.clone() })? else {
                bail!("unexpected reply")
            };
            let transcript = transcript.with_context(|| format!("{} has no conversation file for this session", from.label))?;
            sync_host(ctx, &from, true)?;
            let thread = ctx.store.get_thread(&s.thread_id)?.context("thread vanished")?;
            let work_dir = match local_project_dir(ctx, &thread.id)? {
                Some(dir) if git(&dir, &["rev-parse", "--git-dir"]).is_ok() => checkout_returned(&dir, &from, &job_branch(&thread, &agent.name))?,
                Some(dir) => dir,
                None => scratch_dir(ctx, &thread.id)?,
            };
            write_transcript(&work_dir, &transcript)?;
            let prompt = format!(
                "You were moved back to the owner's machine. Your conversation so far is above. You now work in {}, with the work you did on {} \
                 (committed and uncommitted). Continue the task where you left off.",
                work_dir.display(),
                from.label
            );
            let req = axel_tui::LaunchRequest {
                title: thread.title.clone(),
                projects: vec![],
                agent_pubkey: agent.pubkey.clone(),
                workspace: axel_store::WorkspaceKind::InPlace,
                branch: None,
                thread_id: Some(thread.id.clone()),
                attachments: vec![],
            };
            let opts = launch::LaunchOptions {
                target: launch::Target::NewWindow,
                runtime: Some(axel_store::SessionRuntime::Managed),
                prompt: Some(prompt),
                attachments: vec![],
                work_dir: Some(work_dir),
                resume: Some(transcript.native_id),
            };
            let id = match launch::launch(ctx, req, opts)? {
                launch::Launched::Managed { session_id, host_spec: Some(spec), .. } => {
                    launch::spawn_detached(&spec, &session_id)?;
                    session_id
                }
                launch::Launched::Managed { session_id, .. } => session_id,
                _ => bail!("{} can't run as a managed session here (is its ACP adapter installed?)", agent.name),
            };
            Ok((id, here.label))
        }
    }
}

/// Stop a session running on this machine, and close its Rex pane.
fn stop_local(ctx: &Ctx, s: &axel_store::Session) {
    if s.state == axel_store::SessionState::Paused { let _ = super::process_control::resume(ctx, s); }
    if let Some(sock) = &s.socket_path {
        let _ = socket::send(sock, &socket::Command::Stop);
    }
    if let (Some(rex), Some(block)) = (&ctx.rex, s.rex_block_id.as_deref()) {
        rex.api_call("block.close", serde_json::json!({"block_id": block})).ok();
    }
}

/// The conversation file of a session that was just stopped, once Claude
/// has let go of it.
fn wait_for_transcript(native: &str, hint: Option<&str>) -> Result<Transcript> {
    let mut last = None;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t = read_transcript(native, hint)?;
        if let (Some(a), Some(b)) = (&t, &last)
            && same_transcript(a, b)
        {
            return Ok(t.unwrap());
        }
        last = t;
    }
    last.with_context(|| format!("can't find Claude's conversation file {native}.jsonl"))
}

fn same_transcript(a: &Transcript, b: &Transcript) -> bool {
    a.jsonl.len() == b.jsonl.len()
}

/// The thread's scratch directory here.
fn scratch_dir(ctx: &Ctx, thread_id: &str) -> Result<PathBuf> {
    ctx.store
        .thread_projects(thread_id)?
        .into_iter()
        .map(|tp| tp.path_hint)
        .find(|p| p.is_dir())
        .with_context(|| "this thread has no directory on this machine to work in".to_string())
}

/// A worktree here on the job branch that came back from `host`, with the
/// host's uncommitted work applied (uncommitted here too).
fn checkout_returned(dir: &Path, host: &Environment, branch: &str) -> Result<PathBuf> {
    checkout_returned_in(dir, &host.label, branch, &paths::worktrees_dir())
}

fn checkout_returned_in(dir: &Path, host: &str, branch: &str, worktrees: &Path) -> Result<PathBuf> {
    let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    git(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .with_context(|| format!("branch {branch} didn't come back from {host}"))?;
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let dest = worktrees.join(name).join(branch.replace('/', "-"));
    let wt = axel_config::git::ensure_worktree(&root, branch, &dest)?;
    // Already checked out here (it moved back before): the host's branch
    // was fetched beside it; it descends from this one.
    let beside = format!("refs/axel/{host}/heads/{branch}");
    if let Ok(theirs) = git(&root, &["rev-parse", "--verify", "--quiet", &beside]) {
        git(&wt, &["merge", "--quiet", "--ff-only", &theirs])
            .with_context(|| format!("{} has local changes that conflict with the work coming back", wt.display()))?;
        git(&root, &["update-ref", "-d", &beside])?;
    }
    let head = git(&wt, &["rev-parse", "HEAD"])?;
    let wip_ref = format!("refs/axel/{host}/{branch}");
    if let Ok(wip) = git(&root, &["rev-parse", "--verify", "--quiet", &wip_ref])
        && git(&root, &["rev-parse", &format!("{wip}^")]).ok().as_deref() == Some(head.as_str())
        && git(&wt, &["status", "--porcelain"])?.is_empty()
    {
        git(&wt, &["read-tree", "-u", "--reset", &wip])?;
        git(&wt, &["reset", "--quiet"])?;
    }
    Ok(wt)
}

// ========================================================== conversations

/// Claude's config directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`).
fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

/// Where Claude keeps the conversations of sessions run in `cwd`.
fn claude_project_dir(cwd: &Path) -> PathBuf {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    claude_dir().join("projects").join(claude_project_name(&cwd))
}

/// Claude names a project's directory after its path, every character but
/// letters and digits turned into `-`.
fn claude_project_name(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// A Claude conversation by id: at `hint`, else wherever Claude filed it.
fn read_transcript(native: &str, hint: Option<&str>) -> Result<Option<Transcript>> {
    if native.contains('/') || native.contains("..") {
        bail!("bad session id {native:?}");
    }
    let file = format!("{native}.jsonl");
    let found = hint.map(PathBuf::from).filter(|p| p.is_file() && p.file_name().is_some_and(|n| n == file.as_str())).or_else(|| {
        std::fs::read_dir(claude_dir().join("projects")).ok()?.flatten().map(|e| e.path().join(&file)).find(|p| p.is_file())
    });
    Ok(match found {
        Some(p) => Some(Transcript { native_id: native.to_string(), jsonl: std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))? }),
        None => None,
    })
}

/// File a conversation where Claude looks for it when resuming in `cwd`.
fn write_transcript(cwd: &Path, t: &Transcript) -> Result<()> {
    if t.native_id.contains('/') || t.native_id.contains("..") || t.native_id.is_empty() {
        bail!("bad session id {:?}", t.native_id);
    }
    let dir = claude_project_dir(cwd);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{}.jsonl", t.native_id)), &t.jsonl)?;
    Ok(())
}

/// The directory a conversation last ran in (Claude records `cwd` per entry).
fn transcript_cwd(jsonl: &str) -> Option<PathBuf> {
    jsonl
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()?.get("cwd")?.as_str().map(PathBuf::from))
        .filter(|p| p.is_dir())
}

// ============================================================ attachments

/// Attachments may total this much per prompt.
const MAX_FILES_BYTES: usize = 32 << 20;

fn pack_files(files: &[PathBuf]) -> Result<Vec<FileBlob>> {
    use base64::Engine;
    let mut total = 0;
    files
        .iter()
        .map(|p| {
            let data = std::fs::read(p).with_context(|| format!("reading attachment {}", p.display()))?;
            total += data.len();
            if total > MAX_FILES_BYTES {
                bail!("attachments for a remote agent are limited to {} MB", MAX_FILES_BYTES >> 20);
            }
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
            Ok(FileBlob { name, data: base64::engine::general_purpose::STANDARD.encode(data) })
        })
        .collect()
}

/// Write attachments into `dir`, returning their paths here.
fn unpack_files(dir: &Path, files: &[FileBlob]) -> Result<Vec<PathBuf>> {
    use base64::Engine;
    if files.is_empty() {
        return Ok(vec![]);
    }
    std::fs::create_dir_all(dir)?;
    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let name: String = f.name.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
            let name = name.trim_start_matches('.');
            let path = dir.join(format!("{}-{i}-{}", axel_store::new_id(), if name.is_empty() { "file" } else { name }));
            let data = base64::engine::general_purpose::STANDARD.decode(&f.data).with_context(|| format!("attachment {}", f.name))?;
            std::fs::write(&path, data)?;
            Ok(path)
        })
        .collect()
}

/// The thread's first project that exists on this machine.
fn local_project_dir(ctx: &Ctx, thread_id: &str) -> Result<Option<PathBuf>> {
    for tp in ctx.store.thread_projects(thread_id)? {
        if tp.workspace.kind == axel_store::WorkspaceKind::Scratch {
            continue;
        }
        let local = tp
            .local_project_id
            .as_deref()
            .and_then(|id| ctx.store.get_project(id).ok().flatten())
            .map(|p| p.path)
            .or_else(|| tp.path_hint.is_dir().then(|| tp.path_hint.clone()));
        if let Some(path) = local {
            return Ok(Some(tp.workspace.worktree_path.clone().filter(|w| w.is_dir()).unwrap_or(path)));
        }
    }
    Ok(None)
}

/// The host's directory name for a repo: its repo key, else the folder name
/// and a hash of its path.
fn repo_name(dir: &Path) -> String {
    match launch::repo_key(dir) {
        Some(k) => k.replace('/', "__"),
        None => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            dir.hash(&mut h);
            format!("local__{}-{:08x}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), h.finish() as u32)
        }
    }
}

fn job_branch(thread: &axel_store::Thread, agent: &str) -> String {
    format!("axel/{}-{}", axel_tui::slugify(&thread.title).chars().take(40).collect::<String>(), agent)
}

/// Push a snapshot of `dir`'s working state to the host's copy of the repo.
fn push_workspace(host: &Environment, dir: &Path, thread: &axel_store::Thread, agent: &str) -> Result<JobWorkspace> {
    let repo = repo_name(dir);
    let Response::Repo { path } = call(host, &Request::PrepareRepo { repo: repo.clone() })? else { bail!("unexpected reply") };
    let start = snapshot_commit(dir, "axel: working state handed to a remote agent")?;
    let target = host.ssh_target.as_deref().unwrap_or(&host.label);
    let start_ref = format!("refs/axel/start/{}-{agent}", &thread.id[..12]);
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_SSH_COMMAND", ssh_command_line())
        .args(["push", "--quiet", "--force", &format!("{target}:{path}"), &format!("{start}:{start_ref}")])
        .output()?;
    if !out.status.success() {
        bail!("pushing the code to {}: {}", host.label, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(JobWorkspace { repo, start, branch: job_branch(thread, agent), reset: false })
}

/// Fetch the turn checkpoints of `thread_id` from the host's copy of the
/// repository into `dir`'s, under the same ref names, so a turn that ran
/// there can be read and undone here.
pub fn fetch_checkpoints(host: &Environment, dir: &Path, thread_id: &str) -> Result<()> {
    let target = host.ssh_target.as_deref().with_context(|| format!("host {} has no ssh target", host.label))?;
    let Response::Repo { path } = call(host, &Request::PrepareRepo { repo: repo_name(dir) })? else { bail!("unexpected reply") };
    let spec = format!("+{refs}/{thread_id}/*:{refs}/{thread_id}/*", refs = axel_acp::checkpoint::REFS);
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_SSH_COMMAND", ssh_command_line())
        .args(["fetch", "--quiet", &format!("{target}:{path}"), &spec])
        .output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Fetch the job branches of `thread_id` from the host into the local repo:
/// `axel/<…>` itself, and `refs/axel/<host>/<branch>` for uncommitted work.
fn fetch_results(ctx: &Ctx, host: &Environment, thread_id: &str) -> Result<Vec<String>> {
    let Some(dir) = local_project_dir(ctx, thread_id)? else { return Ok(vec![]) };
    if !dir.join(".git").exists() {
        return Ok(vec![]);
    }
    let Response::Branches { branches } = call(host, &Request::Snapshot { thread_id: thread_id.to_string() })? else {
        bail!("unexpected reply")
    };
    let target = host.ssh_target.as_deref().unwrap_or(&host.label);
    let checked_out = git(&dir, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let mut out = Vec::new();
    for b in branches {
        // A branch checked out here (a session that moved back) can't be
        // overwritten; its host copy goes beside it.
        let busy = checked_out.lines().any(|l| l.strip_prefix("branch refs/heads/") == Some(b.branch.as_str()));
        let head_ref = if busy { format!("refs/axel/{}/heads/{}", host.label, b.branch) } else { format!("refs/heads/{}", b.branch) };
        let local_head = git(&dir, &["rev-parse", "--verify", "--quiet", &head_ref]).ok();
        let wip_ref = format!("refs/axel/{}/{}", host.label, b.branch);
        let local_wip = git(&dir, &["rev-parse", "--verify", "--quiet", &wip_ref]).ok();
        let mut specs = Vec::new();
        if local_head.as_deref() != Some(b.head.as_str()) {
            specs.push(format!("+refs/heads/{}:{head_ref}", b.branch));
        }
        if let Some(w) = &b.wip
            && local_wip.as_deref() != Some(w.as_str())
        {
            specs.push(format!("+refs/axel/result/{}:{wip_ref}", b.branch));
        }
        if specs.is_empty() {
            continue;
        }
        let Response::Repo { path } = call(host, &Request::PrepareRepo { repo: repo_name(&dir) })? else { bail!("unexpected reply") };
        let repo = format!("{target}:{path}");
        let fetched = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .env("GIT_SSH_COMMAND", ssh_command_line())
            .args(["fetch", "--quiet", &repo])
            .args(&specs)
            .output()?;
        if fetched.status.success() {
            let name = if busy { head_ref.clone() } else { b.branch.clone() };
            out.push(match &b.wip {
                Some(_) => format!("{name} (uncommitted work in {wip_ref})"),
                None => name,
            });
        } else {
            out.push(format!("{}: fetch failed: {}", b.branch, String::from_utf8_lossy(&fetched.stderr).trim()));
        }
    }
    Ok(out)
}

// ================================================================ on the host

fn repo_dir(repo: &str) -> Result<PathBuf> {
    if repo.is_empty() || repo.contains("..") || repo.contains('/') {
        bail!("bad repo name {repo:?}");
    }
    Ok(paths::home().join("repos").join(format!("{repo}.git")))
}

fn job_worktree(thread_id: &str, agent: &str) -> PathBuf {
    paths::home().join("worktrees").join(format!("{}-{agent}", &thread_id[..12.min(thread_id.len())]))
}

/// Run a job on this host: learn the agent, hold its delegation, check out
/// its branch, and start a managed session in the `axel` Rex session.
fn start_job(ctx: &Ctx, job: &Job) -> Result<String> {
    ctx.store.get_thread(&job.thread_id)?.context("the thread hasn't arrived here yet")?;
    let key: axel_identity::PublicKey = job.job_key.parse()?;
    ctx.keystore.agent(&key)?.context("that job key wasn't made here")?;
    let deleg = axel_identity::Delegation::from_tag(&job.delegation)?;
    let now = axel_tui::util::now_secs() as u64;
    deleg.verify_for(&key, &job.thread_id, axel_store::kind::SESSION_START, now)?;
    if deleg.delegator.to_base58() != job.agent.pubkey {
        bail!("the delegation is from another agent");
    }
    ctx.store.put_delegation(&job.job_key, &job.delegation, job.agent.auth_tag.as_deref())?;

    // The agent as this host knows it: no key; it always runs on autopilot
    // here, since nobody is around to answer prompts (the guard still runs).
    // Its policy comes along, and the agents it may reach, so `@name` works.
    job.agent.insert_if_missing(ctx, axel_store::RuntimeMode::Autopilot)?;
    for p in &job.peers {
        p.insert_if_missing(ctx, axel_store::RuntimeMode::Autopilot)?;
    }
    ctx.store.set_agent_policy(&job.agent.pubkey, &axel_store::AgentPolicy { reach: job.reach.clone(), budget: job.budget })?;

    let work_dir = match &job.workspace {
        Some(ws) => {
            let repo = repo_dir(&ws.repo)?;
            let wt = job_worktree(&job.thread_id, &job.agent.name);
            if !wt.exists() {
                std::fs::create_dir_all(wt.parent().unwrap())?;
                let exists = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", ws.branch)]).is_ok();
                let wt_s = wt.to_string_lossy().into_owned();
                if exists {
                    git(&repo, &["worktree", "add", "--quiet", &wt_s, &ws.branch])?;
                } else {
                    git(&repo, &["worktree", "add", "--quiet", "-b", &ws.branch, &wt_s, &ws.start])?;
                }
            }
            if ws.reset {
                // A moved session: the laptop has its newest work.
                git(&wt, &["checkout", "--quiet", &ws.branch])?;
                git(&wt, &["reset", "--quiet", "--hard", &ws.start])?;
                git(&wt, &["clean", "--quiet", "-fd"])?;
            }
            wt
        }
        None => {
            let d = paths::home().join("scratch").join(&job.thread_id[..12.min(job.thread_id.len())]);
            std::fs::create_dir_all(&d)?;
            d
        }
    };

    let attachments = unpack_files(&paths::home().join("attachments").join(&job.thread_id[..12.min(job.thread_id.len())]), &job.attachments)?;
    let resume = match &job.resume {
        Some(t) => {
            write_transcript(&work_dir, t)?;
            Some(t.native_id.clone())
        }
        None => None,
    };
    let req = axel_tui::LaunchRequest {
        title: job.title.clone(),
        projects: vec![],
        agent_pubkey: job.agent.pubkey.clone(),
        workspace: axel_store::WorkspaceKind::InPlace,
        branch: None,
        thread_id: Some(job.thread_id.clone()),
        attachments: vec![],
    };
    let opts = launch::LaunchOptions {
        target: launch::Target::NewWindow,
        runtime: Some(axel_store::SessionRuntime::Managed),
        prompt: Some(job.prompt.clone()),
        attachments,
        work_dir: Some(work_dir.clone()),
        resume,
    };
    let (session_id, mut spec) = match launch::launch(ctx, req, opts)? {
        launch::Launched::Managed { session_id, host_spec: Some(spec), .. } => (session_id, spec),
        launch::Launched::Managed { session_id, .. } => return Ok(session_id),
        _ => bail!("{} can't run as a managed session here (is its ACP adapter installed?)", job.agent.name),
    };
    spec.env.push(("PATH".into(), std::env::var("PATH").unwrap_or_default()));
    spec.env.push(("AXEL_KEYSTORE".into(), "file".into()));
    spec.env.push(("AXEL_NOTIFY".into(), "1".into()));
    if let Ok(h) = std::env::var("AXEL_HOME") {
        spec.env.push(("AXEL_HOME".into(), h));
    }
    if let Some(f) = &job.from {
        spec.env.push(("AXEL_PROMPT_FROM".into(), f.pubkey.clone()));
    }
    let label = format!("{} · {}", job.agent.name, axel_tui::util::ellipsize(&job.title, 30));
    if run_in_rex(&spec, &work_dir, &label).is_err() {
        launch::spawn_detached(&spec, &session_id)?;
    }
    Ok(session_id)
}

/// The host's Rex CLI, if Rex is installed.
fn rex_cli() -> Option<PathBuf> {
    launch::which("rex").or_else(|| dirs::home_dir().map(|h| h.join(REX_IN_APP)).filter(|p| p.exists()))
}

/// Run the session host in a new window of the `axel` Rex session, so it
/// survives this SSH call, shows on any Rex client, and its notifications
/// reach them.
fn run_in_rex(spec: &axel_config::launch::LaunchSpec, cwd: &Path, label: &str) -> Result<()> {
    let rex = rex_cli().context("no rex here")?;
    let mut command: Vec<String> = vec!["/usr/bin/env".into()];
    command.extend(spec.env.iter().map(|(k, v)| format!("{k}={v}")));
    command.push(spec.program.clone());
    command.extend(spec.args.iter().cloned());
    let has_session = Command::new(&rex)
        .args(["session", "ls"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().next() == Some(REX_SESSION)))
        .unwrap_or(false);
    let mut cmd = Command::new(&rex);
    if has_session {
        cmd.args(["run", "-s", REX_SESSION, "--focus=false", "--shell=none", "--label", label]);
    } else {
        cmd.args(["new", REX_SESSION, "--window", label, "--shell=none"]);
    }
    let out = cmd.arg("-c").arg(cwd).arg("--").args(&command).output()?;
    if !out.status.success() {
        bail!("rex: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Each job worktree of `thread_id`: its branch head, and a snapshot of its
/// uncommitted work under `refs/axel/result/<branch>`.
fn snapshot_branches(thread_id: &str) -> Result<Vec<RemoteBranch>> {
    let prefix = format!("{}-", &thread_id[..12.min(thread_id.len())]);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(paths::home().join("worktrees")) else { return Ok(out) };
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let wt = e.path();
        let Ok(branch) = git(&wt, &["symbolic-ref", "--short", "HEAD"]) else { continue };
        let head = git(&wt, &["rev-parse", "HEAD"])?;
        let snap = snapshot_commit(&wt, "axel: uncommitted work of a remote agent")?;
        let wip = if snap != head {
            git(&wt, &["update-ref", &format!("refs/axel/result/{branch}"), &snap])?;
            Some(snap)
        } else {
            None
        };
        out.push(RemoteBranch { branch, head, wip });
    }
    Ok(out)
}

// ===================================================================== git

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A commit of `dir`'s working tree on top of HEAD (tracked and untracked
/// files, not ignored ones), or HEAD itself when nothing changed.  The
/// user's index and branches are untouched.  (Turn checkpoints use the same
/// snapshots: `axel_acp::checkpoint`.)
pub fn snapshot_commit(dir: &Path, message: &str) -> Result<String> {
    axel_acp::checkpoint::snapshot_commit(dir, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["commit", "--quiet", "--allow-empty", "-m", "root"],
        ] {
            git(d.path(), &args).unwrap();
        }
        d
    }

    #[test]
    fn claude_files_conversations_under_the_mangled_path() {
        assert_eq!(
            claude_project_name(Path::new("/Users/ludo/.axel/worktrees/3c13d6a04d4e-orion")),
            "-Users-ludo--axel-worktrees-3c13d6a04d4e-orion"
        );
        let d = tempfile::tempdir().unwrap();
        let jsonl = format!(
            "{{\"type\":\"user\",\"cwd\":\"/nowhere\"}}\n{{\"type\":\"assistant\",\"cwd\":{:?}}}\n{{\"type\":\"summary\"}}\n",
            d.path().to_string_lossy()
        );
        assert_eq!(transcript_cwd(&jsonl).as_deref(), Some(d.path()), "the last cwd that exists");
        assert!(write_transcript(d.path(), &Transcript { native_id: "../x".into(), jsonl: String::new() }).is_err());
    }

    #[test]
    fn attachments_travel_with_safe_names() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("shot.png");
        std::fs::write(&a, [0u8, 159, 146, 150]).unwrap();
        let blobs = pack_files(std::slice::from_ref(&a)).unwrap();
        assert_eq!(blobs[0].name, "shot.png");
        let evil = FileBlob { name: "../../.ssh/authorized_keys".into(), data: blobs[0].data.clone() };
        let out = d.path().join("in");
        let paths = unpack_files(&out, &[blobs[0].clone(), evil]).unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), [0u8, 159, 146, 150]);
        for p in &paths {
            assert_eq!(p.parent(), Some(out.as_path()), "{} stays in the folder", p.display());
        }
        assert!(paths[0].to_string_lossy().ends_with("-0-shot.png"));
    }

    #[test]
    fn work_coming_back_lands_in_a_worktree_with_the_uncommitted_part_uncommitted() {
        let d = repo();
        let root = d.path();
        git(root, &["checkout", "--quiet", "-b", "axel/fix-orion"]).unwrap();
        std::fs::write(root.join("done.rs"), "// committed on the host").unwrap();
        git(root, &["add", "."]).unwrap();
        git(root, &["commit", "--quiet", "-m", "host commit"]).unwrap();
        std::fs::write(root.join("wip.rs"), "// not committed yet").unwrap();
        let wip = snapshot_commit(root, "wip").unwrap();
        git(root, &["update-ref", "refs/axel/mini/axel/fix-orion", &wip]).unwrap();
        std::fs::remove_file(root.join("wip.rs")).unwrap();
        git(root, &["checkout", "--quiet", "-"]).unwrap();

        let trees = tempfile::tempdir().unwrap();
        let wt = checkout_returned_in(root, "mini", "axel/fix-orion", trees.path()).unwrap();
        assert!(wt.join("done.rs").exists() && wt.join("wip.rs").exists());
        let status = git(&wt, &["status", "--porcelain"]).unwrap();
        assert_eq!(status, "?? wip.rs", "the host's uncommitted work is uncommitted here");
        // Again, with local changes: nothing is clobbered.
        std::fs::write(wt.join("wip.rs"), "// edited here").unwrap();
        let again = checkout_returned_in(root, "mini", "axel/fix-orion", trees.path()).unwrap();
        assert_eq!(std::fs::canonicalize(again).unwrap(), std::fs::canonicalize(&wt).unwrap());
        assert_eq!(std::fs::read_to_string(wt.join("wip.rs")).unwrap(), "// edited here");
    }

    #[test]
    fn requests_round_trip_as_json() {
        let r: Request = serde_json::from_str(&serde_json::to_string(&Request::PullItems { cursor: 7 }).unwrap()).unwrap();
        assert!(matches!(r, Request::PullItems { cursor: 7 }));
        assert!(repo_dir("../x").is_err() && repo_dir("a/b").is_err() && repo_dir("github.com__a__b").is_ok());
        assert_eq!(shell_quote("ControlPath=/a b/%C"), "'ControlPath=/a b/%C'");
    }
}
