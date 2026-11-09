//! The session host: one managed ACP session, end to end.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder};
use anyhow::{Context, Result};
use axel_identity::Keypair;
use axel_store::{
    BlockedKind, ConversationItem, Harness, ItemKind, ItemStatus, NewItem, RuntimeMode,
    SessionState, SessionUpdate, StatusReport, StatusState, Store, TurnChanges, TurnSummary,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use crate::adapter::adapter_command;
use crate::checkpoint;
use crate::usage::TurnUsage;
use crate::attach::{self, PromptCaps};
use crate::log::FrameLog;
use crate::policy::{self, Decision, ToolKind};
use crate::socket::{Command, Reply, Sender};
use crate::transport;

/// Everything `run_host` needs to know about the session it hosts.
#[derive(Clone, Debug)]
pub struct HostConfig {
    pub session_id: String,
    pub thread_id: String,
    pub agent_pubkey: String,
    /// The thread's workspace: the adapter's cwd and the ACP session's cwd.
    pub cwd: PathBuf,
    pub harness: Harness,
    pub runtime_mode: RuntimeMode,
    pub disallowed_tools: Vec<String>,
    /// `~/.axel/logs/SESSION.jsonl`
    pub log_path: PathBuf,
    /// `$AXEL_HOME/run/SESSION.sock`
    pub socket_path: PathBuf,
    /// Extra environment for the adapter.
    pub env: Vec<(String, String)>,
    /// Sent as turn 1 right after the session is created.
    pub initial_prompt: Option<String>,
    /// Attachments for the initial prompt.
    pub initial_attachments: Vec<PathBuf>,
    /// The agent that summoned this session with the initial prompt, if any.
    pub initial_from: Option<Sender>,
    /// The `axel` binary: a turn another agent's message started ends by
    /// returning its answer to that agent (`axel send --reply`). `None`
    /// returns nothing.
    pub reply_with: Option<PathBuf>,
    /// A native session id to `session/load` instead of `session/new`.
    pub resume_native_id: Option<String>,
    /// `_meta` sent with `session/new` / `session/load`.  For Claude this
    /// carries `claudeCode.options.extraArgs.settings`: the guard hook that
    /// still runs when the adapter bypasses permission prompts.
    pub session_meta: Option<serde_json::Map<String, Value>>,
    /// The agent's model (`fable`, `opus`, a full model id), switched to with
    /// ACP config options, or legacy `session/set_model`, once the session
    /// exists. `None` keeps the default.
    pub model: Option<String>,
    /// MCP servers the agent gets in this session (ACP `mcpServers`): Axel's
    /// own tools, running as this session's agent.
    pub mcp_servers: Vec<McpStdio>,
}

/// A stdio MCP server for `session/new` / `session/load`.
#[derive(Clone, Debug)]
pub struct McpStdio {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpStdio {
    fn to_acp(&self) -> acp::McpServer {
        acp::McpServer::Stdio(
            acp::McpServerStdio::new(self.name.clone(), self.command.clone())
                .args(self.args.clone())
                .env(self.env.iter().map(|(k, v)| acp::EnvVariable::new(k.clone(), v.clone())).collect()),
        )
    }
}

/// Pick the adapter's model id for `want`, matching like Claude Code's
/// adapter matches a settings `model`: exact id, then id or name containing
/// it.  `available` is `models.availableModels` from `session/new`.
pub fn resolve_model(want: &str, available: &[Value]) -> Option<String> {
    let want_l = want.to_lowercase();
    let ids: Vec<(String, String)> = available
        .iter()
        .filter_map(|m| Some((m.get("modelId")?.as_str()?.to_string(), m.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase())))
        .collect();
    ids.iter()
        .find(|(id, _)| id.to_lowercase() == want_l)
        .or_else(|| ids.iter().find(|(_, name)| *name == want_l))
        .or_else(|| ids.iter().find(|(id, name)| id.to_lowercase().contains(&want_l) || name.contains(&want_l)))
        .map(|(id, _)| id.clone())
}

/// Config selectors may group their values by provider.
fn model_values(options: &[Value]) -> Vec<Value> {
    options.iter().flat_map(|option| {
        if let Some(group) = option.get("options").and_then(Value::as_array) {
            model_values(group)
        } else {
            option.get("value").and_then(Value::as_str)
                .map(|id| vec![json!({ "modelId": id, "name": option.get("name").and_then(Value::as_str).unwrap_or(id) })])
                .unwrap_or_default()
        }
    }).collect()
}

/// The adapter's native permission mode for a runtime mode, by the mode ids
/// Claude Code's adapter advertises (`default`, `acceptEdits`, `plan`,
/// `dontAsk`, `bypassPermissions`).
///
/// Leaving the adapter in `default` while Axel answers by policy is not
/// enough: Claude's background sub-agents cannot ask, so the adapter refuses
/// their tools (reads outside the cwd, shell) without a request ever reaching
/// the host.  Matching the native mode keeps the harness and the policy in
/// agreement, as t3code does with the Agent SDK's `permissionMode`.
pub fn native_mode(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Autopilot => "bypassPermissions",
        RuntimeMode::Edits => "acceptEdits",
        RuntimeMode::Supervised => "default",
        RuntimeMode::Plan => "plan",
    }
}

/// Raw input / output stored on tool-call items is cut at this many bytes.
/// Stored tool input and output stop at this many bytes (a read file, a
/// command's output): enough to see it, not the store's whole size.
const RAW_LIMIT: usize = 16 * 1024;
/// `TurnSummary.assistant_text` keeps this many chars.
const ASSISTANT_HEAD: usize = 2000;

/// Host one managed session until it is stopped, the adapter exits, or the
/// host's stdin (the pane) closes.  See the crate docs for what it does.
pub async fn run_host(
    cfg: HostConfig,
    store: Store,
    signer: Keypair,
    out: impl Write + Send,
) -> Result<()> {
    let log = Arc::new(
        FrameLog::open(&cfg.log_path)
            .with_context(|| format!("opening frame log {}", cfg.log_path.display()))?,
    );
    let (tx, rx) = mpsc::unbounded_channel::<Event>();

    // Adapter.
    let cmd = adapter_command(&cfg.harness);
    let (stderr_tx, mut stderr_rx) = mpsc::unbounded_channel::<String>();
    let (child, transport_) = transport::spawn(&cmd, &cfg.cwd, &cfg.env, log.clone(), stderr_tx)?;
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(line) = stderr_rx.recv().await {
                if tx.send(Event::Stderr(line)).is_err() {
                    break;
                }
            }
        });
    }

    // Command socket.
    if let Some(parent) = cfg.socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&cfg.socket_path);
    let listener = UnixListener::bind(&cfg.socket_path)
        .with_context(|| format!("binding {}", cfg.socket_path.display()))?;
    let socket_task = tokio::spawn(serve_socket(listener, tx.clone()));

    // The pane closing shows up as EOF on our stdin — only meaningful when stdin is a terminal.
    if std::io::stdin().is_terminal() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
            let _ = tx.send(Event::StdinClosed);
        });
    }

    let socket_path = cfg.socket_path.clone();
    let mut host = Host::new(cfg, store, signer, out, cmd);
    host.line(CYAN, "●", &format!("adapter: {}", host.adapter.join(" ")));

    let tx_notif = tx.clone();
    let tx_ext = tx.clone();
    let tx_perm = tx.clone();
    let cwd = host.cfg.cwd.clone();
    let cwd2 = cwd.clone();
    // Shared with the fs handler so a mid-session mode switch reaches writes.
    let mode_cell = host.mode_cell.clone();
    let connected = Client
        .builder()
        .name("axel")
        .on_receive_notification(
            async move |n: acp::SessionNotification, _cx: ConnectionTo<Agent>| {
                let _ = tx_notif.send(Event::Update(n));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: agent_client_protocol::UntypedMessage, _cx: ConnectionTo<Agent>| {
                if n.method().starts_with("_axel/") {
                    let _ = tx_ext.send(Event::Ext { method: n.method().to_string(), params: n.params().clone() });
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: acp::RequestPermissionRequest,
                        responder: Responder<acp::RequestPermissionResponse>,
                        _cx: ConnectionTo<Agent>| {
                let (reply_tx, reply_rx) = oneshot::channel();
                if tx_perm
                    .send(Event::Permission {
                        req,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return responder.respond(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    ));
                }
                match reply_rx.await {
                    Ok(resp) => responder.respond(resp),
                    Err(_) => responder.respond(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    )),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::ReadTextFileRequest,
                        responder: Responder<acp::ReadTextFileResponse>,
                        _cx: ConnectionTo<Agent>| {
                fs_read(&cwd, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::WriteTextFileRequest,
                        responder: Responder<acp::WriteTextFileResponse>,
                        _cx: ConnectionTo<Agent>| {
                let mode = *mode_cell.lock().expect("mode lock");
                fs_write(&cwd2, mode, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport_, async |cx: ConnectionTo<Agent>| {
            let reason = host.run(&cx, rx).await;
            Ok::<String, acp::Error>(reason)
        })
        .await;

    let reason = match connected {
        Ok(reason) => reason,
        Err(e) => format!("connection error: {e}"),
    };
    socket_task.abort();
    host.finish(&reason);
    let _ = std::fs::remove_file(&socket_path);
    // The adapter and everything it started (Claude, `npx` wrappers) share a
    // process group: end the group, so no harness outlives its session.
    if let Some(pid) = child.id() {
        // SAFETY: signalling a process group we created (`process_group(0)`).
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    drop(child); // kill_on_drop
    Ok(())
}

// --- events ------------------------------------------------------------------

enum Event {
    Update(acp::SessionNotification),
    Permission {
        req: acp::RequestPermissionRequest,
        reply: oneshot::Sender<acp::RequestPermissionResponse>,
    },
    Command {
        cmd: Command,
        reply: oneshot::Sender<Reply>,
    },
    Stderr(String),
    StdinClosed,
    /// One of the adapter's own notifications (`_axel/…`).
    Ext { method: String, params: Value },
}

type PromptFuture = Pin<Box<dyn Future<Output = Result<acp::PromptResponse, acp::Error>> + Send>>;

/// What the loop must do after a command was handled.
enum Action {
    None,
    StartPrompt(String, Vec<PathBuf>, Option<Sender>),
    SetMode(RuntimeMode),
    /// Send this prompt (in `steering`) into the running turn.
    Steer(String),
    /// Withdraw a prompt sent into the turn; then put `edit` in, or drop it.
    /// The reply waits for the adapter's answer.
    Withdraw { item_id: String, edit: Option<String>, reply: oneshot::Sender<Reply> },
    Stop,
}

struct TextItem {
    id: String,
    kind: ItemKind,
    text: String,
}

/// A prompt that arrived while a turn was running, persisted as a `pending`
/// `UserMessage` item so every viewer of the thread sees it.
struct Queued {
    item_id: String,
    text: String,
    files: Vec<PathBuf>,
    from: Option<Sender>,
}

struct PendingPermission {
    reply: oneshot::Sender<acp::RequestPermissionResponse>,
    title: String,
    /// `(option_id, kind)` with kind as the protocol string.
    options: Vec<(String, String)>,
}

// --- ansi --------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

/// A returned answer is cut to this many characters.
const REPLY_MAX_CHARS: usize = 8000;

/// `app` in this host's OSC 7501 reports: the program in the pane is Axel's
/// session host, whatever harness it drives.
const PROGRAM_STATUS_APP: &str = "axel";

struct Host<W: Write> {
    cfg: HostConfig,
    store: Store,
    signer: Keypair,
    out: W,
    adapter: Vec<String>,
    native_session_id: Option<acp::SessionId>,
    prompt_caps: PromptCaps,
    state: SessionState,
    turn: i64,
    turn_started: Instant,
    turn_user_text: String,
    /// The agent whose message started this turn, if it wasn't the owner.
    turn_from: Option<Sender>,
    turn_assistant_text: String,
    turn_tool_calls: i64,
    turn_files_edited: i64,
    /// The tree when this turn started (its `before` checkpoint), if the
    /// workspace is a git repository.
    turn_before: Option<checkpoint::Snapshot>,
    /// The tree when the previous turn ended: a different tree at the next
    /// start means edits were made between turns.
    last_after: Option<checkpoint::Snapshot>,
    /// Files this turn's edit tools named (absolute), for `authored`.
    turn_authored: Vec<PathBuf>,
    /// A slow checkpoint has been pointed out once already.
    checkpoint_slow_told: bool,
    /// What this turn's `_axel/usage` reports were costed at: the turn's
    /// own cost, when the harness gives one at the end, already holds it.
    turn_progress_cost: f64,
    /// An `_axel/usage` report came in this turn.
    turn_progress: bool,
    /// Cost already recorded for this native session, from any usage channel.
    session_cost: f64,
    /// Standard cumulative cost takes precedence over adapter extensions.
    standard_cost_seen: bool,
    cur_text: Option<TextItem>,
    /// Pending `Permission` items by item id.  Permission items carry
    /// `ref_id = "permission:<toolCallId>"` so tool-call updates never touch them.
    pending: HashMap<String, PendingPermission>,
    /// Prompts waiting for the running turn to end, next first.
    queue: VecDeque<Queued>,
    /// The adapter can take prompts into a running turn (`_axel/steer`).
    can_steer: bool,
    /// Prompts sent into the running turn and not taken in yet, oldest first.
    steering: VecDeque<Queued>,
    /// The runtime mode the fs handler reads (kept equal to `cfg.runtime_mode`).
    mode_cell: Arc<std::sync::Mutex<RuntimeMode>>,
    /// What `session/new` said about native modes, for later switches.
    native_modes: Option<acp::SessionModeState>,
    ended: bool,
    /// When this agent last started a turn (Unix ms): the thread after that is
    /// what it hasn't seen.  `None` before its first turn here.
    seen_through: Option<i64>,
}

impl<W: Write> Host<W> {
    fn new(cfg: HostConfig, store: Store, signer: Keypair, out: W, adapter: Vec<String>) -> Self {
        let mode_cell = Arc::new(std::sync::Mutex::new(cfg.runtime_mode));
        let mut session_cost = store.usage(&cfg.session_id).ok().flatten().map_or(0.0, |u| u.cost_usd_est);
        if let Some(native) = &cfg.resume_native_id {
            // A cumulative native cost includes the earlier Axel sessions
            // of a resumed conversation. Their spend is already in the store.
            for session in store.list_sessions(axel_store::SessionFilter {
                thread_id: Some(cfg.thread_id.clone()),
                agent_pubkey: Some(cfg.agent_pubkey.clone()),
                ..Default::default()
            }).unwrap_or_default() {
                if session.id != cfg.session_id && session.harness_session_id.as_ref() == Some(native) {
                    session_cost += store.usage(&session.id).ok().flatten().map_or(0.0, |u| u.cost_usd_est);
                }
            }
        }
        Host {
            cfg,
            store,
            signer,
            out,
            adapter,
            native_session_id: None,
            prompt_caps: PromptCaps::default(),
            state: SessionState::Starting,
            turn: 0,
            turn_started: Instant::now(),
            turn_user_text: String::new(),
            turn_assistant_text: String::new(),
            turn_tool_calls: 0,
            turn_files_edited: 0,
            turn_before: None,
            last_after: None,
            turn_authored: Vec::new(),
            checkpoint_slow_told: false,
            turn_progress_cost: 0.0,
            turn_progress: false,
            session_cost,
            standard_cost_seen: false,
            cur_text: None,
            pending: HashMap::new(),
            queue: VecDeque::new(),
            can_steer: false,
            steering: VecDeque::new(),
            mode_cell,
            native_modes: None,
            turn_from: None,
            ended: false,
            seen_through: None,
        }
    }

    // --- output ---

    fn line(&mut self, colour: &str, glyph: &str, text: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let _ = writeln!(self.out, "{DIM}{ts}{RESET} {colour}{glyph}{RESET} {text}");
        let _ = self.out.flush();
    }

    /// A desktop notification (OSC 9) for the terminal the host runs in, when
    /// `$AXEL_NOTIFY` is set: remote jobs run in a Rex pane on their host, and
    /// Rex shows it on every client attached there.
    fn notify(&mut self, text: &str) {
        if std::env::var_os("AXEL_NOTIFY").is_none() {
            return;
        }
        let clean: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
        let _ = write!(self.out, "\x1b]9;{clean}\x07");
        let _ = self.out.flush();
    }

    /// Report this session's state to the terminal its pane runs in, as an
    /// OSC 7501 Program Status record (revision 0.4).  Each report replaces the
    /// record whole, so `app` and `title` (the agent's name) go in every one.
    fn program_status(&mut self, state: axel_rex::osc::State, kind: Option<axel_rex::osc::Kind>, msg: &str) {
        let title = match self.store.agent_by_pubkey(&self.cfg.agent_pubkey) {
            Ok(Some(a)) => a.name,
            _ => std::env::var("AXEL_AGENT_NAME").unwrap_or_else(|_| "agent".into()),
        };
        let msg = msg.trim();
        let report = axel_rex::osc::Report {
            state,
            id: None,
            kind,
            progress: None,
            app: Some(PROGRAM_STATUS_APP),
            title: Some(&title),
            msg: (!msg.is_empty()).then_some(msg),
        };
        let _ = write!(self.out, "{}", axel_rex::osc::encode(&report));
        let _ = self.out.flush();
    }

    fn dim(&mut self, text: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let _ = writeln!(self.out, "{DIM}{ts}   {text}{RESET}");
        let _ = self.out.flush();
    }

    // --- the loop ---

    /// Initialise, create the session, then serve events until a stop reason.
    async fn run(
        &mut self,
        cx: &ConnectionTo<Agent>,
        mut rx: mpsc::UnboundedReceiver<Event>,
    ) -> String {
        let mut in_flight: Option<PromptFuture> = None;
        match self.setup(cx).await {
            Ok(Some((text, files))) => {
                let from = self.cfg.initial_from.clone();
                in_flight = Some(self.start_turn(cx, text, files, from));
            }
            Ok(None) => {}
            Err(e) => {
                self.line(RED, "✖", &format!("setup failed: {e:#}"));
                return format!("setup failed: {e:#}");
            }
        }
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { return "host channel closed".into() };
                    match self.handle_event(cx, ev, in_flight.is_some()) {
                        Action::None => {}
                        Action::StartPrompt(text, files, from) => in_flight = Some(self.start_turn(cx, text, files, from)),
                        Action::SetMode(mode) => self.set_mode(cx, mode).await,
                        Action::Steer(item_id) => self.send_steer(cx, &item_id).await,
                        Action::Withdraw { item_id, edit, reply } => {
                            let r = self.withdraw(cx, &item_id, edit).await;
                            let _ = reply.send(r);
                        }
                        Action::Stop => {
                            // Let the socket reply drain before the listener is torn down.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            return "stopped".into();
                        }
                    }
                    // A prompt the turn couldn't take (it ended meanwhile) waits
                    // in the queue: start it.
                    if in_flight.is_none()
                        && let Some((text, files, from)) = self.take_queued(None)
                    {
                        in_flight = Some(self.start_turn(cx, text, files, from));
                    }
                }
                res = async { in_flight.as_mut().expect("guarded").await }, if in_flight.is_some() => {
                    in_flight = None;
                    // The agent's last `session/update`s precede its response on the wire but
                    // reach us through the channel: project them before closing the turn.
                    tokio::task::yield_now().await;
                    let mut deferred = Vec::new();
                    while let Ok(ev) = rx.try_recv() {
                        match ev {
                            Event::Update(_) | Event::Permission { .. } | Event::Stderr(_) | Event::Ext { .. } => {
                                let _ = self.handle_event(cx, ev, true);
                            }
                            other => deferred.push(other),
                        }
                    }
                    self.end_turn(res);
                    for ev in deferred {
                        match self.handle_event(cx, ev, in_flight.is_some()) {
                            Action::None => {}
                            Action::StartPrompt(text, files, from) => in_flight = Some(self.start_turn(cx, text, files, from)),
                            Action::SetMode(mode) => self.set_mode(cx, mode).await,
                            Action::Steer(item_id) => self.send_steer(cx, &item_id).await,
                            Action::Withdraw { item_id, edit, reply } => {
                                let r = self.withdraw(cx, &item_id, edit).await;
                                let _ = reply.send(r);
                            }
                            Action::Stop => {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                return "stopped".into();
                            }
                        }
                    }
                    if in_flight.is_none()
                        && let Some((text, files, from)) = self.take_queued(None)
                    {
                        in_flight = Some(self.start_turn(cx, text, files, from));
                    }
                }
                _ = cx.incoming_closed() => {
                    if in_flight.is_some() {
                        self.end_turn(Err(acp::Error::internal_error().data(json!("adapter exited mid-turn"))));
                    }
                    return "adapter exited".into();
                }
            }
        }
    }

    /// initialize → session/new (or session/load); returns the initial prompt to send.
    async fn setup(&mut self, cx: &ConnectionTo<Agent>) -> Result<Option<(String, Vec<PathBuf>)>> {
        let caps = acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true))
            .terminal(false);
        let init = cx
            .send_request(
                acp::InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(caps)
                    .client_info(acp::Implementation::new("axel", env!("CARGO_PKG_VERSION"))),
            )
            .block_task()
            .await
            .map_err(|e| anyhow::anyhow!("initialize: {e}"))?;
        self.prompt_caps = PromptCaps::from(&init.agent_capabilities.prompt_capabilities);
        self.can_steer = init
            .agent_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("axel"))
            .and_then(|a| a.get("steer"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let agent_name = init
            .agent_info
            .as_ref()
            .map(|i| format!("{} {}", i.name, i.version))
            .unwrap_or_else(|| self.cfg.harness.to_string());
        self.line(
            CYAN,
            "●",
            &format!(
                "initialized: {agent_name} (protocol v{:?})",
                init.protocol_version
            ),
        );

        let sid = match (
            &self.cfg.resume_native_id,
            init.agent_capabilities.load_session,
        ) {
            (Some(native), true) => {
                let raw = self
                    .raw_request(
                        cx,
                        "session/load",
                        acp::LoadSessionRequest::new(native.clone(), self.cfg.cwd.clone())
                            .mcp_servers(self.cfg.mcp_servers.iter().map(McpStdio::to_acp).collect())
                            .meta(self.cfg.session_meta.clone()),
                    )
                    .await?;
                let resp: acp::LoadSessionResponse = serde_json::from_value(raw.clone()).context("session/load reply")?;
                (acp::SessionId::new(native.clone()), resp.modes, raw)
            }
            (resume, _) => {
                if resume.is_some() {
                    self.dim("agent cannot load sessions; starting a new one");
                }
                let raw = self
                    .raw_request(
                        cx,
                        "session/new",
                        acp::NewSessionRequest::new(self.cfg.cwd.clone())
                            .mcp_servers(self.cfg.mcp_servers.iter().map(McpStdio::to_acp).collect())
                            .meta(self.cfg.session_meta.clone()),
                    )
                    .await?;
                let resp: acp::NewSessionResponse = serde_json::from_value(raw.clone()).context("session/new reply")?;
                (resp.session_id, resp.modes, raw)
            }
        };
        let (sid, modes, raw) = sid;
        self.apply_native_mode(cx, &sid, modes.as_ref()).await;
        self.apply_model(cx, &sid, &raw).await;
        self.line(
            CYAN,
            "●",
            &format!("session {sid} · cwd {}", self.cfg.cwd.display()),
        );
        self.native_session_id = Some(sid.clone());
        self.store.update_session(
            &self.signer,
            &self.cfg.session_id,
            &SessionUpdate {
                harness_session_id: Some(sid.to_string()),
                ..Default::default()
            },
        )?;
        self.store
            .set_session_socket(&self.cfg.session_id, Some(&self.cfg.socket_path))?;
        self.set_state(SessionState::Running)?;
        if self.cfg.initial_prompt.is_none() {
            self.set_state(SessionState::Idle)?;
        }
        Ok(self
            .cfg
            .initial_prompt
            .take()
            .map(|t| (t, std::mem::take(&mut self.cfg.initial_attachments))))
    }

    /// A request by method name, returning the raw reply: the typed schema
    /// drops fields adapters add (Claude's `models`).
    async fn raw_request(&self, cx: &ConnectionTo<Agent>, method: &str, params: impl serde::Serialize) -> Result<Value> {
        let msg = agent_client_protocol::UntypedMessage::new(method, params).map_err(|e| anyhow::anyhow!("{method}: {e}"))?;
        cx.send_request(msg).block_task().await.map_err(|e| anyhow::anyhow!("{method}: {e}"))
    }

    /// Switch to the agent's model, when it has one.  The adapter picks its
    /// own default right after `session/new`, so this has to come after.
    async fn apply_model(&mut self, cx: &ConnectionTo<Agent>, sid: &acp::SessionId, session_reply: &Value) {
        let Some(want) = self.cfg.model.clone().filter(|m| !m.trim().is_empty()) else { return };
        let selector = session_reply.get("configOptions").and_then(Value::as_array).and_then(|options| {
            options.iter().find(|o| o.get("type").and_then(Value::as_str) == Some("select")
                && (o.get("category").and_then(Value::as_str) == Some("model")
                    || (o.get("category").is_none() && o.get("id").and_then(Value::as_str) == Some("model"))))
        });
        let models = session_reply.get("models");
        let available: Vec<Value> = match selector {
            Some(option) => model_values(option.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])),
            None => models.and_then(|m| m.get("availableModels")).and_then(Value::as_array).cloned().unwrap_or_default(),
        };
        let current = match selector {
            Some(option) => option.get("currentValue"),
            None => models.and_then(|m| m.get("currentModelId")),
        }.and_then(Value::as_str).unwrap_or("");
        // An unknown name goes through as given: the harness may accept full ids it doesn't list.
        let id = resolve_model(&want, &available).unwrap_or_else(|| want.clone());
        if id == current {
            self.line(CYAN, "●", &format!("model {id}"));
            return;
        }
        let (method, params) = match selector {
            Some(option) => ("session/set_config_option", json!({ "sessionId": sid.to_string(), "configId": option["id"], "value": id })),
            None => ("session/set_model", json!({ "sessionId": sid.to_string(), "modelId": id })),
        };
        match self.raw_request(cx, method, params).await {
            Ok(_) => self.line(CYAN, "●", &format!("model {id}")),
            Err(e) => {
                let listed: Vec<&str> = available.iter().filter_map(|m| m.get("modelId")?.as_str()).collect();
                let msg = format!(
                    "could not switch the agent to model {want} ({e}); it runs {}{}",
                    if current.is_empty() { "its default" } else { current },
                    if listed.is_empty() { String::new() } else { format!(" · available: {}", listed.join(", ")) }
                );
                self.line(YELLOW, "!", &msg);
                if let Err(e) = self.push(ItemKind::Error, Some(ItemStatus::Failed), None, json!({ "message": msg })) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
            }
        }
    }

    /// Put the adapter in the native mode matching the session's runtime mode,
    /// when it advertises one.  A failure is surfaced on the thread: the policy
    /// still answers what reaches it, but sub-agents may be refused.
    async fn apply_native_mode(
        &mut self,
        cx: &ConnectionTo<Agent>,
        sid: &acp::SessionId,
        modes: Option<&acp::SessionModeState>,
    ) {
        let Some(modes) = modes.cloned() else { return };
        let modes = &modes;
        self.native_modes = Some(modes.clone());
        let want = native_mode(self.cfg.runtime_mode);
        if modes.current_mode_id.to_string() == want {
            return;
        }
        if !modes.available_modes.iter().any(|m| m.id.to_string() == want) {
            self.dim(&format!("agent has no {want} mode; staying in {}", modes.current_mode_id));
            return;
        }
        let res = cx
            .send_request(acp::SetSessionModeRequest::new(sid.clone(), want.to_string()))
            .block_task()
            .await;
        match res {
            Ok(_) => {
                self.line(CYAN, "●", &format!("mode {want} ({})", self.cfg.runtime_mode));
                if let Some(m) = self.native_modes.as_mut() {
                    m.current_mode_id = acp::SessionModeId::new(want.to_string());
                }
            }
            Err(e) => {
                let msg = format!(
                    "could not switch the agent to {want} for {} ({e}); it stays in {}, so background sub-agents may be refused",
                    self.cfg.runtime_mode, modes.current_mode_id
                );
                self.line(YELLOW, "!", &msg);
                if let Err(e) = self.push(ItemKind::Error, Some(ItemStatus::Failed), None, json!({ "message": msg })) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
            }
        }
    }

    /// Switch the runtime mode mid-session: the policy, the fs handler, the
    /// adapter's native mode, and the session row (so viewers see it).
    async fn set_mode(&mut self, cx: &ConnectionTo<Agent>, mode: RuntimeMode) {
        if mode == self.cfg.runtime_mode {
            return;
        }
        self.cfg.runtime_mode = mode;
        *self.mode_cell.lock().expect("mode lock") = mode;
        if let Err(e) = self.store.update_session(
            &self.signer,
            &self.cfg.session_id,
            &SessionUpdate { runtime_mode: Some(mode), ..Default::default() },
        ) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        self.line(CYAN, "●", &format!("runtime mode → {mode}"));
        if let Some(sid) = self.native_session_id.clone() {
            let modes = self.native_modes.clone();
            self.apply_native_mode(cx, &sid, modes.as_ref()).await;
        }
    }

    fn handle_event(&mut self, cx: &ConnectionTo<Agent>, ev: Event, in_turn: bool) -> Action {
        match ev {
            Event::Update(n) => {
                if let Err(e) = self.on_update(n.update) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
                Action::None
            }
            Event::Permission { req, reply } => {
                if let Err(e) = self.on_permission(req, reply) {
                    self.line(RED, "✖", &format!("permission: {e}"));
                }
                Action::None
            }
            // Changing or withdrawing a prompt sent into the turn asks the
            // adapter first: the reply waits for its answer.
            Event::Command { cmd: Command::Edit { item_id, text }, reply } if self.steering.iter().any(|q| q.item_id == item_id) => {
                Action::Withdraw { item_id, edit: Some(text), reply }
            }
            Event::Command { cmd: Command::Dequeue { item_id }, reply } if self.steering.iter().any(|q| q.item_id == item_id) => {
                Action::Withdraw { item_id, edit: None, reply }
            }
            Event::Command { cmd, reply } => {
                let (r, action) = self.on_command(cx, cmd, in_turn);
                let _ = reply.send(r);
                action
            }
            Event::Ext { method, params } => {
                self.on_ext(&method, &params);
                Action::None
            }
            Event::Stderr(line) => {
                let shown: String = line.chars().take(200).collect();
                self.dim(&format!("‹adapter› {shown}"));
                Action::None
            }
            Event::StdinClosed => Action::Stop,
        }
    }

    fn on_command(
        &mut self,
        cx: &ConnectionTo<Agent>,
        cmd: Command,
        in_turn: bool,
    ) -> (Reply, Action) {
        match cmd {
            Command::Prompt {
                text,
                attachments,
                steer,
                from,
            } => {
                if text.trim().is_empty() && attachments.is_empty() {
                    return (
                        Reply::Error {
                            message: "empty prompt".into(),
                        },
                        Action::None,
                    );
                }
                if !in_turn {
                    return (Reply::Ok, Action::StartPrompt(text, attachments, from));
                }
                // The owner's message goes into the running turn. Another
                // agent's waits for its own turn, so its answer goes back to it.
                if !steer && self.can_steer && from.is_none() {
                    return match self.record_steer(text, attachments) {
                        Ok(item_id) => (Reply::Steering { item_id: item_id.clone() }, Action::Steer(item_id)),
                        Err(e) => (Reply::Error { message: format!("store: {e}") }, Action::None),
                    };
                }
                match self.enqueue(text, attachments, steer, from) {
                    Ok(item_id) => {
                        if steer && let Err(e) = self.cancel_turn(cx) {
                            return (Reply::Error { message: e }, Action::None);
                        }
                        (Reply::Queued { item_id }, Action::None)
                    }
                    Err(e) => (
                        Reply::Error {
                            message: format!("queue: {e}"),
                        },
                        Action::None,
                    ),
                }
            }
            Command::Note { text, from } => {
                if text.trim().is_empty() {
                    return (Reply::Error { message: "empty note".into() }, Action::None);
                }
                let content = json!({ "text": text, "from": from.pubkey, "fyi": true });
                match self.push(ItemKind::UserMessage, Some(ItemStatus::Completed), None, content) {
                    Ok(_) => {
                        self.line(DIM, "✎", &format!("note from {}  {}", from.name, head(&text, 100)));
                        (Reply::Ok, Action::None)
                    }
                    Err(e) => (Reply::Error { message: format!("store: {e}") }, Action::None),
                }
            }
            Command::Steer { item_id } => {
                let Some(pos) = self.queue.iter().position(|q| q.item_id == item_id) else {
                    return (
                        Reply::Error {
                            message: "that message is no longer queued".into(),
                        },
                        Action::None,
                    );
                };
                if !in_turn {
                    let (text, files, from) = self.take_queued(Some(pos)).expect("position checked");
                    return (Reply::Ok, Action::StartPrompt(text, files, from));
                }
                // Into the running turn, rather than cancelling it.
                if self.can_steer && self.queue[pos].from.is_none() {
                    let q = self.queue.remove(pos).expect("position checked");
                    self.patch_item(&q.item_id, json!({ "steering": true }));
                    let id = q.item_id.clone();
                    self.steering.push_back(q);
                    return (Reply::Steering { item_id: id.clone() }, Action::Steer(id));
                }
                let q = self.queue.remove(pos).expect("position checked");
                self.queue.push_front(q);
                match self.cancel_turn(cx) {
                    Ok(()) => (Reply::Ok, Action::None),
                    Err(e) => (Reply::Error { message: e }, Action::None),
                }
            }
            Command::Dequeue { item_id } => {
                let Some(pos) = self.queue.iter().position(|q| q.item_id == item_id) else {
                    return (
                        Reply::Error {
                            message: "that message is no longer queued".into(),
                        },
                        Action::None,
                    );
                };
                self.queue.remove(pos);
                if let Err(e) = self.store.delete_item(&item_id) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
                self.dim(&format!("dequeued {item_id}"));
                (Reply::Ok, Action::None)
            }
            Command::Edit { item_id, text } => {
                if text.trim().is_empty() {
                    return (Reply::Error { message: "empty message: remove it instead".into() }, Action::None);
                }
                let Some(q) = self.queue.iter_mut().find(|q| q.item_id == item_id) else {
                    return (Reply::Error { message: "the agent has read that message already".into() }, Action::None);
                };
                q.text = text.clone();
                self.patch_item(&item_id, json!({ "text": text }));
                self.dim(&format!("edited {item_id}"));
                (Reply::Ok, Action::None)
            }
            Command::SetMode { mode } => (Reply::Ok, Action::SetMode(mode)),
            Command::Cancel => {
                self.cancel_pending("cancelled");
                if in_turn && let Err(e) = self.cancel_turn(cx) {
                    return (Reply::Error { message: e }, Action::None);
                }
                (Reply::Ok, Action::None)
            }
            Command::Answer { item_id, option_id } => match self.answer(&item_id, &option_id) {
                Ok(()) => (Reply::Ok, Action::None),
                Err(e) => (
                    Reply::Error {
                        message: e.to_string(),
                    },
                    Action::None,
                ),
            },
            Command::Status => (
                Reply::Status {
                    state: self.state,
                    turn: self.turn,
                    pending: self.pending.len(),
                    queued: self.queue.len() + self.steering.len(),
                },
                Action::None,
            ),
            Command::Stop => (Reply::Ok, Action::Stop),
        }
    }

    // --- turns ---

    /// `session/cancel` the running turn (pending permissions are rejected).
    fn cancel_turn(&mut self, cx: &ConnectionTo<Agent>) -> std::result::Result<(), String> {
        self.cancel_pending("cancelled");
        let Some(sid) = self.native_session_id.clone() else {
            return Ok(());
        };
        cx.send_notification(acp::CancelNotification::new(sid))
            .map_err(|e| format!("session/cancel: {e}"))?;
        self.line(YELLOW, "■", &format!("cancelling turn {}", self.turn));
        Ok(())
    }

    /// Persist a prompt sent mid-turn as a pending `UserMessage` and queue it
    /// (first when steering).  Returns the item id.
    fn enqueue(&mut self, text: String, files: Vec<PathBuf>, steer: bool, from: Option<Sender>) -> Result<String> {
        let paths = attach::attachment_paths(&files);
        let mut content = json!({ "text": text, "attachments": paths, "queued": true });
        if let Some(f) = &from {
            content["from"] = json!(f.pubkey);
        }
        let item = self.push(ItemKind::UserMessage, Some(ItemStatus::Pending), None, content)?;
        let q = Queued {
            item_id: item.id.clone(),
            text,
            files,
            from,
        };
        let verb = if steer { "steering" } else { "queued" };
        self.line(BLUE, "…", &format!("{verb}  {DIM}{}{RESET}", head(&q.text, 100)));
        if steer {
            self.queue.push_front(q);
        } else {
            self.queue.push_back(q);
        }
        Ok(item.id)
    }

    /// Record a prompt sent into the running turn, as a pending `UserMessage`
    /// (it shows as not read yet until the agent takes it in). Its item id.
    fn record_steer(&mut self, text: String, files: Vec<PathBuf>) -> Result<String> {
        let paths = attach::attachment_paths(&files);
        let content = json!({ "text": text, "attachments": paths, "queued": true, "steering": true });
        let item = self.push(ItemKind::UserMessage, Some(ItemStatus::Pending), None, content)?;
        self.line(BLUE, "↳", &format!("into the turn  {DIM}{}{RESET}", head(&text, 100)));
        self.steering.push_back(Queued { item_id: item.id.clone(), text, files, from: None });
        Ok(item.id)
    }

    /// Hand a prompt in `steering` to the adapter for the running turn. If it
    /// can't take it (the turn just ended), it waits in the queue instead.
    async fn send_steer(&mut self, cx: &ConnectionTo<Agent>, item_id: &str) {
        let Some(q) = self.steering.iter().find(|q| q.item_id == item_id) else { return };
        let mut warnings = Vec::new();
        let blocks = attach::build_prompt_blocks(&q.text, &q.files, self.prompt_caps, &mut |w| warnings.push(w));
        for w in warnings {
            self.line(YELLOW, "!", &w);
        }
        let Some(sid) = self.native_session_id.clone() else { return };
        let params = json!({ "sessionId": sid.to_string(), "messageId": item_id, "prompt": blocks });
        let accepted = match self.raw_request(cx, "_axel/steer", params).await {
            Ok(v) => v.get("accepted").and_then(Value::as_bool).unwrap_or(false),
            Err(e) => {
                self.line(YELLOW, "!", &format!("{e}"));
                false
            }
        };
        if !accepted {
            self.unsteer_to_queue(item_id, false);
        }
    }

    /// A prompt sent into the turn goes back to waiting in the queue (first
    /// when `front`, as for the turn that was cancelled under it).
    fn unsteer_to_queue(&mut self, item_id: &str, front: bool) {
        let Some(pos) = self.steering.iter().position(|q| q.item_id == item_id) else { return };
        let q = self.steering.remove(pos).expect("position checked");
        self.patch_item(item_id, json!({ "steering": null }));
        if front {
            self.queue.push_front(q);
        } else {
            self.queue.push_back(q);
        }
    }

    /// Withdraw a prompt sent into the turn, if the agent hasn't taken it in;
    /// with `edit`, send the new text in its place.
    async fn withdraw(&mut self, cx: &ConnectionTo<Agent>, item_id: &str, edit: Option<String>) -> Reply {
        if edit.as_deref().is_some_and(|t| t.trim().is_empty()) {
            return Reply::Error { message: "empty message: remove it instead".into() };
        }
        let Some(sid) = self.native_session_id.clone() else {
            return Reply::Error { message: "no session".into() };
        };
        let withdrawn = self
            .raw_request(cx, "_axel/unsteer", json!({ "sessionId": sid.to_string(), "messageId": item_id }))
            .await
            .ok()
            .and_then(|v| v.get("withdrawn").and_then(Value::as_bool))
            .unwrap_or(false);
        // Taken in meanwhile (its `_axel/steered` may still be on its way).
        if !withdrawn || !self.steering.iter().any(|q| q.item_id == item_id) {
            return Reply::Error { message: "the agent has read that message already".into() };
        }
        match edit {
            Some(text) => {
                if let Some(q) = self.steering.iter_mut().find(|q| q.item_id == item_id) {
                    q.text = text.clone();
                }
                self.patch_item(item_id, json!({ "text": text }));
                self.dim(&format!("edited {item_id}"));
                self.send_steer(cx, item_id).await;
            }
            None => {
                self.steering.retain(|q| q.item_id != item_id);
                if let Err(e) = self.store.delete_item(item_id) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
                self.dim(&format!("withdrew {item_id}"));
            }
        }
        Reply::Ok
    }

    /// The adapter's own notifications: a prompt sent into the turn landed
    /// (`_axel/steered`), or went back unread when the turn was cancelled
    /// (`_axel/unsteered`); where the plan's limits stand (`_axel/limits`).
    fn on_ext(&mut self, method: &str, params: &Value) {
        match method {
            "_axel/steered" => {
                let Some(id) = params.get("messageId").and_then(Value::as_str) else { return };
                let Some(pos) = self.steering.iter().position(|q| q.item_id == id) else { return };
                let q = self.steering.remove(pos).expect("position checked");
                if let Err(e) = self.store.update_item(id, Some(ItemStatus::Completed), Some(json!({ "queued": null, "steering": null }))) {
                    self.line(RED, "✖", &format!("store: {e}"));
                }
                self.line(BLUE, "↳", &format!("read  {DIM}{}{RESET}", head(&q.text, 100)));
            }
            // Spend so far in a running turn: tokens now, the turn counted
            // with its reply.
            "_axel/usage" => {
                let Some(usage) = TurnUsage::from_reply(params) else { return };
                let mut delta = axel_store::UsageDelta { turns: 0, ..usage_delta(&usage) };
                if self.standard_cost_seen {
                    delta.cost_usd_est = 0.0;
                }
                self.turn_progress_cost += delta.cost_usd_est;
                self.turn_progress = true;
                if let Err(e) = self.store.add_usage(&self.signer, &self.cfg.session_id, &delta, None) {
                    self.line(RED, "✖", &format!("usage: {e}"));
                } else {
                    self.session_cost += delta.cost_usd_est;
                }
            }
            "_axel/limits" => {
                for l in crate::usage::limits_from_params(params) {
                    if let Err(e) = self.store.set_plan_limit(&self.cfg.harness, l.window, l.used_pct, l.resets_at) {
                        self.line(RED, "✖", &format!("store: {e}"));
                    }
                }
            }
            "_axel/unsteered" => {
                let ids: Vec<String> = params.get("messageIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
                // Oldest first, ahead of what was queued.
                for id in ids.iter().rev() {
                    self.unsteer_to_queue(id, true);
                }
            }
            _ => {}
        }
    }

    /// Merge `patch` into an item's content.
    fn patch_item(&mut self, item_id: &str, patch: Value) {
        if let Err(e) = self.store.update_item(item_id, None, Some(patch)) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
    }

    /// Pop a queued prompt (the first, or the one at `pos`) and drop its
    /// placeholder item: `start_turn` records the message again at its real
    /// place in the timeline.
    fn take_queued(&mut self, pos: Option<usize>) -> Option<(String, Vec<PathBuf>, Option<Sender>)> {
        let q = self.queue.remove(pos.unwrap_or(0))?;
        if let Err(e) = self.store.delete_item(&q.item_id) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        Some((q.text, q.files, q.from))
    }

    /// Return this turn's answer to the agent whose message started it, with
    /// `axel send --reply` (signed, policy-checked and delivered like any
    /// message).  Not for the owner's prompts, and not for a turn an answer
    /// started, so two agents can't keep answering each other.
    fn return_answer(&mut self) {
        let (Some(from), Some(exe)) = (self.turn_from.take(), self.cfg.reply_with.clone()) else { return };
        if from.reply {
            return;
        }
        // The turn's last message is the answer; what came before a tool call
        // was working out loud.
        self.finalize_text();
        let last = self
            .store
            .items(&self.cfg.session_id, 0, 100_000)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .find(|i| i.turn == self.turn && i.kind == ItemKind::AssistantMessage)
            .and_then(|i| i.content.get("text").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let answer = last.trim();
        if answer.is_empty() {
            return;
        }
        let answer: String = answer.chars().take(REPLY_MAX_CHARS).collect();
        match std::process::Command::new(exe)
            .args(["send", "--reply", "--to", &from.name, "--"])
            .arg(&answer)
            // As this agent, on this thread (the adapter's environment says who).
            .envs(self.cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(o) if o.status.success() => self.line(DIM, "↩", &format!("answer returned to {}", from.name)),
            Ok(o) => self.line(RED, "✖", &format!("returning the answer to {}: {}", from.name, String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => self.line(RED, "✖", &format!("returning the answer to {}: {e}", from.name)),
        }
    }

    /// What this agent missed on its thread since its last turn, and who
    /// else is working there (see [`crate::catchup`]).
    fn catch_up(&self) -> Option<String> {
        use crate::catchup::{Entry, Peer, Speaker};
        let me = &self.cfg.session_id;
        let items = self.store.thread_items(&self.cfg.thread_id, 400).ok()?;
        let sessions = self
            .store
            .list_sessions(axel_store::SessionFilter { thread_id: Some(self.cfg.thread_id.clone()), ..Default::default() })
            .unwrap_or_default();
        let name = |pk: &str| -> String {
            match self.store.agent_by_pubkey(pk) {
                Ok(Some(a)) => a.name,
                _ => pk.chars().take(8).collect(),
            }
        };
        let agent_of = |sid: &str| sessions.iter().find(|s| s.id == sid).map(|s| name(&s.agent_pubkey));
        let entries: Vec<Entry> = items
            .iter()
            .filter(|it| self.seen_through.is_none_or(|t| it.at > t))
            .filter_map(|it| {
                let text = it.content.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
                if text.is_empty() {
                    return None;
                }
                let from = it.content.get("from").and_then(|f| f.as_str());
                let fyi = it.content.get("fyi").and_then(|f| f.as_bool()).unwrap_or(false);
                let speaker = match (it.session_id == *me, it.kind, from) {
                    (true, ItemKind::UserMessage, Some(f)) if fyi => Speaker::Note { from: name(f) },
                    (true, _, _) => return None,
                    (false, ItemKind::AssistantMessage, _) => Speaker::Agent { name: agent_of(&it.session_id)? },
                    // Queued prompts that haven't started aren't said yet.
                    (false, ItemKind::UserMessage, _) if it.status == Some(ItemStatus::Pending) => return None,
                    (false, ItemKind::UserMessage, Some(f)) if f == self.cfg.agent_pubkey => return None,
                    (false, ItemKind::UserMessage, Some(f)) => Speaker::Relay { from: name(f), to: agent_of(&it.session_id)? },
                    (false, ItemKind::UserMessage, None) => Speaker::Owner { to: agent_of(&it.session_id)? },
                    _ => return None,
                };
                Some(Entry { speaker, text: text.to_string() })
            })
            .collect();
        let peers: Vec<Peer> = sessions
            .iter()
            .filter(|s| s.ended_at.is_none() && s.id != *me && s.agent_pubkey != self.cfg.agent_pubkey)
            .map(|s| {
                let status = self.store.status(&s.id).ok().flatten();
                let working = status.as_ref().is_some_and(|st| st.state == StatusState::Working);
                Peer { name: name(&s.agent_pubkey), working, doing: status.and_then(|st| st.msg).filter(|_| working) }
            })
            .collect();
        crate::catchup::render(&entries, &peers, self.seen_through.is_none())
    }

    fn start_turn(
        &mut self,
        cx: &ConnectionTo<Agent>,
        text: String,
        files: Vec<PathBuf>,
        from: Option<Sender>,
    ) -> PromptFuture {
        // Text Claude was streaming on its own between turns ends here.
        self.finalize_text();
        self.turn += 1;
        self.turn_started = Instant::now();
        self.turn_user_text = text.clone();
        self.turn_from = from.clone();
        // Publish before Git snapshots or context preparation: the message
        // view must not wait for repository I/O to acknowledge what was typed.
        let paths = attach::attachment_paths(&files);
        let mut content = json!({ "text": text, "attachments": paths });
        if let Some(f) = &from {
            content["from"] = json!(f.pubkey);
        }
        if let Err(e) = self.push(ItemKind::UserMessage, Some(ItemStatus::Completed), None, content) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        // The thread shows the message as written; the agent also reads who
        // wrote it, and first what it missed on the thread.
        let framed = from.as_ref().map_or_else(|| text.clone(), |f| f.frame(&text));
        let catch_up = self.catch_up();
        self.seen_through = Some(chrono::Utc::now().timestamp_millis());
        let wire_text = crate::catchup::prepend(catch_up.as_deref(), &framed);
        self.turn_assistant_text.clear();
        self.turn_tool_calls = 0;
        self.turn_files_edited = 0;
        self.turn_authored.clear();
        self.turn_progress_cost = 0.0;
        self.turn_progress = false;
        self.cur_text = None;
        if let Err(e) = self.set_state(SessionState::Running) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        let _ = self.store.record_status(
            &self.signer,
            &self.cfg.session_id,
            &StatusReport {
                state: StatusState::Working,
                title: Some(format!("turn {}", self.turn)),
                msg: Some(head(&text, 120)),
                ..Default::default()
            },
        );
        self.program_status(axel_rex::osc::State::Working, None, &head(&text, 200));
        // Still before session/prompt, so the agent's first edit cannot enter
        // the before snapshot. Only message visibility moves ahead of this I/O.
        self.turn_before = self.checkpoint(checkpoint::Which::Before);
        let turn = self.turn;
        let files_note = if files.is_empty() {
            String::new()
        } else {
            format!(" {DIM}+{} file(s){RESET}", files.len())
        };
        self.line(
            BOLD,
            "▶",
            &format!(
                "prompt {turn}  {DIM}{}{RESET}{files_note}",
                head(&text, 100)
            ),
        );
        let mut warnings = Vec::new();
        let blocks =
            attach::build_prompt_blocks(&wire_text, &files, self.prompt_caps, &mut |w| warnings.push(w));
        for w in warnings {
            self.line(YELLOW, "!", &w);
        }
        let sid = self
            .native_session_id
            .clone()
            .expect("session created before prompting");
        let fut = cx
            .send_request(acp::PromptRequest::new(sid, blocks))
            .block_task();
        Box::pin(fut)
    }

    /// What the turn spent, when the reply says it in Axel's terms (our own
    /// adapters do). ACP leaves what `usage` counts open, so a reply without
    /// `_meta.axel` isn't summed.
    fn record_usage(&mut self, resp: &acp::PromptResponse) {
        let progress_cost = std::mem::take(&mut self.turn_progress_cost);
        let progress = std::mem::take(&mut self.turn_progress);
        let Some(usage) = serde_json::to_value(resp).ok().as_ref().and_then(TurnUsage::from_reply) else {
            // Progress reports carried all of it: the turn still counts.
            if progress
                && let Err(e) = self.store.add_usage(&self.signer, &self.cfg.session_id, &axel_store::UsageDelta { turns: 1, ..Default::default() }, None)
            {
                self.line(RED, "✖", &format!("usage: {e}"));
            }
            return;
        };
        let mut delta = usage_delta(&usage);
        if self.standard_cost_seen {
            delta.cost_usd_est = 0.0;
        } else if usage.cost_usd.is_some() {
            delta.cost_usd_est = (delta.cost_usd_est - progress_cost).max(0.0);
        }
        if let Err(e) = self.store.add_usage(&self.signer, &self.cfg.session_id, &delta, None) {
            self.line(RED, "✖", &format!("usage: {e}"));
        } else {
            self.session_cost += delta.cost_usd_est;
        }
    }

    fn end_turn(&mut self, res: Result<acp::PromptResponse, acp::Error>) {
        self.finalize_text();
        self.cancel_pending("turn ended");
        self.close_open_tool_calls();
        // Sent into the turn and never taken in: they wait for the next one.
        let unread: Vec<String> = self.steering.iter().map(|q| q.item_id.clone()).collect();
        for id in unread.iter().rev() {
            self.unsteer_to_queue(id, true);
        }
        let duration_ms = self.turn_started.elapsed().as_millis() as i64;
        let (stop_reason, status) = match &res {
            Ok(resp) => {
                let s = enum_str(&resp.stop_reason);
                let st = if s == "cancelled" {
                    ItemStatus::Cancelled
                } else {
                    ItemStatus::Completed
                };
                (s, st)
            }
            Err(e) => {
                if let Err(err) = self.push(
                    ItemKind::Error,
                    Some(ItemStatus::Failed),
                    None,
                    json!({ "message": e.to_string() }),
                ) {
                    self.line(RED, "✖", &format!("store: {err}"));
                }
                ("error".to_string(), ItemStatus::Failed)
            }
        };
        if let Ok(resp) = &res {
            self.record_usage(resp);
        }
        let changes = self.turn_changes();
        let mut turn_end = json!({
            "stop_reason": stop_reason,
            "tool_calls": self.turn_tool_calls,
            "files_edited": self.turn_files_edited,
            "duration_ms": duration_ms,
        });
        if let Some(c) = &changes {
            turn_end["changes"] = serde_json::to_value(c).unwrap_or(Value::Null);
        }
        if let Err(e) = self.push(ItemKind::TurnEnd, Some(status), None, turn_end) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        let summary = TurnSummary {
            ordinal: self.turn,
            user_text: self.turn_user_text.clone(),
            assistant_text: self
                .turn_assistant_text
                .chars()
                .take(ASSISTANT_HEAD)
                .collect(),
            tool_calls: self.turn_tool_calls,
            files_edited: self.turn_files_edited,
            duration_ms,
            stop_reason: stop_reason.clone(),
            changes,
        };
        if let Err(e) = self
            .store
            .record_turn(&self.signer, &self.cfg.session_id, &summary)
        {
            self.line(RED, "✖", &format!("record_turn: {e}"));
        }
        if let Err(e) = self.set_state(SessionState::Idle) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        let who = std::env::var("AXEL_AGENT_NAME").unwrap_or_else(|_| "agent".into());
        let head: String = self.turn_assistant_text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect();
        self.notify(&match stop_reason.as_str() {
            "error" => format!("{who} hit an error"),
            "cancelled" => format!("{who} stopped"),
            _ if head.is_empty() => format!("{who} finished"),
            _ => format!("{who} finished: {head}"),
        });
        // Finished: a result the owner hasn't seen (done, which Rex marks until
        // they look); stopped by them: at rest (idle); failed: error.
        let turn_state = match stop_reason.as_str() {
            "error" => StatusState::Error,
            "cancelled" => StatusState::Idle,
            _ => StatusState::Done,
        };
        let _ = self.store.record_status(
            &self.signer,
            &self.cfg.session_id,
            &StatusReport {
                state: turn_state,
                title: Some(format!("turn {} {}", self.turn, stop_reason)),
                msg: (turn_state == StatusState::Done && !head.is_empty()).then(|| head.clone()),
                ..Default::default()
            },
        );
        // Another agent's message started this turn: its answer goes back.
        if stop_reason == "end_turn" {
            self.return_answer();
        }
        // A finished turn is a result to look at (done); a cancelled one is at
        // rest (idle); a failed one stopped (error).
        let (osc_state, osc_msg) = match stop_reason.as_str() {
            "error" => (axel_rex::osc::State::Error, "the turn failed".to_string()),
            "cancelled" => (axel_rex::osc::State::Idle, String::new()),
            _ => (axel_rex::osc::State::Done, head.clone()),
        };
        self.program_status(osc_state, None, &osc_msg);
        let secs = duration_ms as f64 / 1000.0;
        let tools = self.turn_tool_calls;
        let turn = self.turn;
        let changed = match summary.changes.as_ref().filter(|c| !c.files.is_empty()) {
            Some(c) => {
                let (a, r) = c.totals();
                format!(" · {} file{} +{a} −{r}", c.files.len(), if c.files.len() == 1 { "" } else { "s" })
            }
            None => String::new(),
        };
        match (&res, status) {
            (Ok(_), ItemStatus::Cancelled) => self.line(
                YELLOW,
                "■",
                &format!("turn {turn} cancelled · {tools} tools · {secs:.1}s{changed}"),
            ),
            (Ok(_), _) => self.line(
                GREEN,
                "✓",
                &format!(
                    "turn {turn} done · {tools} tools · {secs:.1}s{changed}{}",
                    if stop_reason == "end_turn" {
                        String::new()
                    } else {
                        format!(" · {stop_reason}")
                    }
                ),
            ),
            (Err(e), _) => self.line(RED, "✖", &format!("turn {turn} failed: {e}")),
        }
    }

    // --- session/update projection ---

    fn on_update(&mut self, update: acp::SessionUpdate) -> Result<()> {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                self.append_text(ItemKind::AssistantMessage, &block_text(&chunk.content))
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                self.append_text(ItemKind::Thought, &block_text(&chunk.content))
            }
            acp::SessionUpdate::UserMessageChunk(_) => Ok(()), // we pushed the UserMessage ourselves
            acp::SessionUpdate::ToolCall(tc) => {
                self.finalize_text();
                let kind = ToolKind::parse(&enum_str(&tc.kind));
                let status = tool_status(&enum_str(&tc.status));
                let raw_input = tc.raw_input.clone().unwrap_or(Value::Null);
                let summary = input_summary(&raw_input);
                self.turn_tool_calls += 1;
                if matches!(kind, ToolKind::Edit | ToolKind::Delete | ToolKind::Move) {
                    self.turn_files_edited += 1;
                    self.note_authored(&tc.locations, &raw_input);
                }
                let content = json!({
                    "title": tc.title,
                    "kind": kind.as_str(),
                    "raw_input": truncate_json(&raw_input, RAW_LIMIT),
                    "locations": locations_json(&tc.locations),
                });
                let ref_id = tc.tool_call_id.to_string();
                if let Some(existing) = self.store.item_by_ref(&self.cfg.session_id, &ref_id)?
                    && existing.turn == self.turn
                {
                    // A permission request may have created it first.
                    self.store
                        .update_item(&existing.id, Some(status), Some(content))?;
                } else {
                    self.push(ItemKind::ToolCall, Some(status), Some(ref_id), content)?;
                }
                self.line(
                    YELLOW,
                    "⚙",
                    &format!(
                        "{}{}",
                        tc.title,
                        if summary.is_empty() {
                            String::new()
                        } else {
                            format!("  {DIM}{summary}{RESET}")
                        }
                    ),
                );
                Ok(())
            }
            acp::SessionUpdate::ToolCallUpdate(upd) => {
                let ref_id = upd.tool_call_id.to_string();
                let f = &upd.fields;
                let status = f.status.map(|s| tool_status(&enum_str(&s)));
                let mut patch = serde_json::Map::new();
                if let Some(t) = &f.title {
                    patch.insert("title".into(), json!(t));
                }
                if let Some(k) = &f.kind {
                    patch.insert("kind".into(), json!(ToolKind::parse(&enum_str(k)).as_str()));
                }
                if let Some(v) = &f.raw_input {
                    patch.insert("raw_input".into(), truncate_json(v, RAW_LIMIT));
                }
                if let Some(v) = &f.raw_output {
                    patch.insert("raw_output".into(), truncate_json(v, RAW_LIMIT));
                }
                if let Some(l) = &f.locations {
                    patch.insert("locations".into(), locations_json(l));
                    let edits = match &f.kind {
                        Some(k) => matches!(ToolKind::parse(&enum_str(k)), ToolKind::Edit | ToolKind::Delete | ToolKind::Move),
                        None => self
                            .store
                            .item_by_ref(&self.cfg.session_id, &ref_id)?
                            .and_then(|it| it.content.get("kind").and_then(Value::as_str).map(|k| matches!(k, "edit" | "delete" | "move")))
                            .unwrap_or(false),
                    };
                    if edits {
                        self.note_authored(l, f.raw_input.as_ref().unwrap_or(&Value::Null));
                    }
                }
                if let Some(c) = &f.content {
                    let v = serde_json::to_value(c).unwrap_or(Value::Null);
                    patch.insert("content".into(), truncate_json(&v, RAW_LIMIT));
                }
                let title = match self.store.item_by_ref(&self.cfg.session_id, &ref_id)? {
                    Some(item) => {
                        self.store
                            .update_item(&item.id, status, Some(Value::Object(patch)))?;
                        f.title
                            .clone()
                            .or_else(|| item_title(&item))
                            .unwrap_or(ref_id)
                    }
                    None => {
                        // An update for a call we never saw: materialise it.
                        self.finalize_text();
                        self.turn_tool_calls += 1;
                        let title = f.title.clone().unwrap_or_else(|| ref_id.clone());
                        patch.entry("title").or_insert(json!(title));
                        patch
                            .entry("kind")
                            .or_insert(json!(ToolKind::Other.as_str()));
                        self.push(
                            ItemKind::ToolCall,
                            Some(status.unwrap_or(ItemStatus::Running)),
                            Some(ref_id),
                            Value::Object(patch),
                        )?;
                        title
                    }
                };
                match status {
                    Some(ItemStatus::Completed) => self.line(GREEN, "✓", &title),
                    Some(ItemStatus::Failed) => self.line(RED, "✗", &title),
                    _ => {}
                }
                Ok(())
            }
            acp::SessionUpdate::CurrentModeUpdate(m) => {
                self.dim(&format!("· agent mode is now {}", m.current_mode_id));
                Ok(())
            }
            acp::SessionUpdate::Plan(plan) => {
                self.finalize_text();
                let mut lines = Vec::new();
                for e in &plan.entries {
                    let glyph = match enum_str(&e.status).as_str() {
                        "completed" => "✓",
                        "in_progress" => "▸",
                        _ => "·",
                    };
                    lines.push(format!("{glyph} {}", e.content));
                }
                let text = lines.join("\n");
                let content = json!({ "text": text, "plan": serde_json::to_value(&plan.entries).unwrap_or(Value::Null) });
                match self.store.item_by_ref(&self.cfg.session_id, "plan")? {
                    Some(item) => self.store.update_item(&item.id, None, Some(content))?,
                    None => {
                        self.push(
                            ItemKind::Thought,
                            Some(ItemStatus::Completed),
                            Some("plan".into()),
                            content,
                        )?;
                    }
                }
                self.line(BLUE, "☰", &format!("plan · {} steps", plan.entries.len()));
                Ok(())
            }
            acp::SessionUpdate::UsageUpdate(u) => {
                if u.size > 0 {
                    self.store.set_session_context(&self.cfg.session_id, u.used, u.size)?;
                }
                if let Some(cost) = u.cost.filter(|c| c.currency.eq_ignore_ascii_case("USD") && c.amount.is_finite() && c.amount >= 0.0) {
                    let amount = (cost.amount - self.session_cost).max(0.0);
                    if amount > 0.0 {
                        self.store.add_usage(&self.signer, &self.cfg.session_id, &axel_store::UsageDelta {
                            cost_usd_est: amount,
                            model: self.cfg.model.clone(),
                            ..Default::default()
                        }, None)?;
                        self.session_cost += amount;
                        self.turn_progress_cost += amount;
                    }
                    self.standard_cost_seen = true;
                    self.turn_progress = true;
                }
                Ok(())
            }
            other => {
                let name = serde_json::to_value(&other)
                    .ok()
                    .and_then(|v| {
                        v.get("sessionUpdate")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "update".into());
                self.dim(&format!("· {name}"));
                Ok(())
            }
        }
    }

    /// Append streamed text to the open AssistantMessage/Thought item, opening one if needed.
    fn append_text(&mut self, kind: ItemKind, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(cur) = &self.cur_text
            && cur.kind != kind
        {
            self.finalize_text();
        }
        if kind == ItemKind::AssistantMessage {
            self.turn_assistant_text.push_str(text);
        }
        match &mut self.cur_text {
            Some(cur) => {
                cur.text.push_str(text);
                let (id, full) = (cur.id.clone(), cur.text.clone());
                self.store
                    .update_item(&id, None, Some(json!({ "text": full })))?;
            }
            None => {
                let item = self.push(
                    kind,
                    Some(ItemStatus::Running),
                    None,
                    json!({ "text": text }),
                )?;
                self.cur_text = Some(TextItem {
                    id: item.id,
                    kind,
                    text: text.to_string(),
                });
                if kind == ItemKind::Thought {
                    self.dim("· thinking…");
                }
            }
        }
        Ok(())
    }

    /// Close the open text item (marks it Completed and prints the prose head).
    fn finalize_text(&mut self) {
        if let Some(cur) = self.cur_text.take() {
            if let Err(e) = self
                .store
                .update_item(&cur.id, Some(ItemStatus::Completed), None)
            {
                self.line(RED, "✖", &format!("store: {e}"));
            }
            if cur.kind == ItemKind::AssistantMessage {
                let shown = head(&cur.text, 160);
                self.line(RESET, "┃", &shown);
            }
        }
    }

    // --- permissions ---

    fn on_permission(
        &mut self,
        req: acp::RequestPermissionRequest,
        reply: oneshot::Sender<acp::RequestPermissionResponse>,
    ) -> Result<()> {
        self.finalize_text();
        let ref_id = req.tool_call.tool_call_id.to_string();
        let perm_ref = format!("permission:{ref_id}");
        let f = &req.tool_call.fields;
        let existing = self.store.item_by_ref(&self.cfg.session_id, &ref_id)?;
        let title = f
            .title
            .clone()
            .or_else(|| existing.as_ref().and_then(item_title))
            .unwrap_or_else(|| ref_id.clone());
        let kind = f
            .kind
            .map(|k| ToolKind::parse(&enum_str(&k)))
            .or_else(|| {
                existing.as_ref().and_then(|i| {
                    i.content
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(ToolKind::parse)
                })
            })
            .unwrap_or_default();
        let raw_input = f
            .raw_input
            .clone()
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|i| i.content.get("raw_input").cloned())
            })
            .unwrap_or(Value::Null);
        let options: Vec<(String, String)> = req
            .options
            .iter()
            .map(|o| (o.option_id.to_string(), enum_str(&o.kind)))
            .collect();
        let pick = |prefix: &str, prefer: &str| -> Option<String> {
            options
                .iter()
                .find(|(_, k)| k == prefer)
                .or_else(|| options.iter().find(|(_, k)| k.starts_with(prefix)))
                .map(|(id, _)| id.clone())
        };
        let summary = input_summary(&raw_input);
        let label = if summary.is_empty() {
            title.clone()
        } else {
            format!("{title}  {DIM}{summary}{RESET}")
        };

        // A question for the owner (its choices are `answer:` options) is never
        // a permission: no policy may pick an answer, whatever the mode.
        let question = !req.options.is_empty() && req.options.iter().all(|o| o.option_id.to_string().starts_with(crate::claude::ANSWER_PREFIX));
        let decision = if question {
            Decision::Ask
        } else {
            policy::decide(self.cfg.runtime_mode, kind, &title, &raw_input, &self.cfg.disallowed_tools)
        };
        match decision {
            Decision::Allow => {
                let resp = match pick("allow", "allow_once") {
                    Some(id) => acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(id)),
                        ),
                    ),
                    None => acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    ),
                };
                let _ = reply.send(resp);
                self.line(GREEN, "✓", &format!("allow: {label}"));
            }
            Decision::Deny { reason } => {
                let resp = match pick("reject", "reject_once") {
                    Some(id) => acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(id)),
                        ),
                    ),
                    None => acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    ),
                };
                let _ = reply.send(resp);
                self.push(
                    ItemKind::Permission,
                    Some(ItemStatus::Denied),
                    Some(perm_ref),
                    json!({ "title": title, "kind": kind.as_str(), "raw_input": truncate_json(&raw_input, RAW_LIMIT), "reason": reason, "decided_by": "policy" }),
                )?;
                self.line(RED, "✗", &format!("deny: {label} {DIM}({reason}){RESET}"));
            }
            Decision::Ask => {
                let opts: Vec<Value> = req
                    .options
                    .iter()
                    .map(|o| json!({ "option_id": o.option_id.to_string(), "name": o.name, "kind": enum_str(&o.kind) }))
                    .collect();
                let item = self.push(
                    if question { ItemKind::Question } else { ItemKind::Permission },
                    Some(ItemStatus::Pending),
                    Some(perm_ref),
                    json!({ "title": title, "kind": kind.as_str(), "raw_input": truncate_json(&raw_input, RAW_LIMIT), "options": opts }),
                )?;
                let who = std::env::var("AXEL_AGENT_NAME").unwrap_or_else(|_| "agent".into());
                self.notify(&if question { format!("{who} asks: {title}") } else { format!("{who} needs permission: {title}") });
                                self.state = SessionState::Blocked;
                self.store.update_session(
                    &self.signer,
                    &self.cfg.session_id,
                    &SessionUpdate {
                        state: Some(SessionState::Blocked),
                        ..Default::default()
                    },
                )?;
                let (osc_kind, blocked_kind) = if question {
                    (axel_rex::osc::Kind::Question, BlockedKind::Question)
                } else {
                    (axel_rex::osc::Kind::Permission, BlockedKind::Permission)
                };
                self.program_status(axel_rex::osc::State::Blocked, Some(osc_kind), &title);
                self.store.record_status(
                    &self.signer,
                    &self.cfg.session_id,
                    &StatusReport {
                        state: StatusState::Blocked,
                        kind: Some(blocked_kind),
                        title: Some(title.clone()),
                        msg: if summary.is_empty() {
                            None
                        } else {
                            Some(summary.clone())
                        },
                        progress: None,
                    },
                )?;
                self.pending.insert(
                    item.id,
                    PendingPermission {
                        reply,
                        title,
                        options,
                    },
                );
                self.line(MAGENTA, "?", &format!("permission: {label}"));
            }
        }
        Ok(())
    }

    fn answer(&mut self, item_id: &str, option_id: &str) -> Result<()> {
        let Some(p) = self.pending.get(item_id) else {
            anyhow::bail!("no pending permission {item_id}");
        };
        let Some((_, kind)) = p.options.iter().find(|(id, _)| id == option_id) else {
            anyhow::bail!("option {option_id} is not offered on {item_id}");
        };
        let allowed = kind.starts_with("allow");
        let p = self.pending.remove(item_id).expect("checked above");
        let _ = p.reply.send(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(option_id.to_string()),
            )),
        ));
        let question = option_id.starts_with(crate::claude::ANSWER_PREFIX);
        let status = match (question, allowed) {
            (true, _) => ItemStatus::Answered,
            (false, true) => ItemStatus::Allowed,
            (false, false) => ItemStatus::Denied,
        };
        self.store
            .update_item(item_id, Some(status), Some(json!({ "answer": option_id })))?;
        if self.pending.is_empty() {
            self.set_state(SessionState::Running)?;
            let prompt = head(&self.turn_user_text, 200);
            self.program_status(axel_rex::osc::State::Working, None, &prompt);
            self.store.record_status(
                &self.signer,
                &self.cfg.session_id,
                &StatusReport {
                    state: StatusState::Working,
                    title: Some(format!("turn {}", self.turn)),
                    ..Default::default()
                },
            )?;
        }
        let (colour, glyph, verb) = if allowed {
            (GREEN, "✓", "allowed")
        } else {
            (RED, "✗", "denied")
        };
        self.line(colour, glyph, &format!("{verb}: {}", p.title));
        Ok(())
    }

    /// Reject every pending permission (dropping the reply answers `cancelled`).
    /// Tool calls of this turn that never got a result (the turn was cancelled
    /// mid-call, or the harness never reported one) can't finish any more:
    /// close them, or the thread would show them running for ever.
    fn close_open_tool_calls(&mut self) {
        let open = match self.store.open_tool_calls(&self.cfg.session_id, self.turn) {
            Ok(open) => open,
            Err(e) => return self.line(RED, "✖", &format!("store: {e}")),
        };
        for item in open {
            if let Err(e) = self.store.update_item(
                &item.id,
                Some(ItemStatus::Cancelled),
                Some(json!({ "reason": "the turn ended before it finished" })),
            ) {
                self.line(RED, "✖", &format!("store: {e}"));
            }
        }
    }

    fn cancel_pending(&mut self, why: &str) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for (item_id, p) in pending {
            let _ = p.reply.send(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ));
            if let Err(e) = self.store.update_item(
                &item_id,
                Some(ItemStatus::Cancelled),
                Some(json!({ "reason": why })),
            ) {
                self.line(RED, "✖", &format!("store: {e}"));
            }
            self.line(
                YELLOW,
                "■",
                &format!("permission cancelled: {} {DIM}({why}){RESET}", p.title),
            );
        }
    }

    // --- store helpers ---

    /// Snapshot the workspace for this turn (`before` or `after`), kept under
    /// its checkpoint ref.  `None` when there is nothing to checkpoint, or it
    /// failed or took too long: the turn goes on without.
    fn checkpoint(&mut self, which: checkpoint::Which) -> Option<checkpoint::Snapshot> {
        let name = checkpoint::ref_name(&self.cfg.thread_id, &self.cfg.session_id, self.turn, which);
        let started = Instant::now();
        // `after` of an unchanged tree is `before` itself; `before` of an
        // untouched tree is the previous turn's `after`.
        let same_as = match which {
            checkpoint::Which::Before => self.last_after.as_ref(),
            checkpoint::Which::After => self.turn_before.as_ref(),
        };
        let snap = match checkpoint::capture(&self.cfg.cwd, &name, checkpoint::BUDGET, same_as) {
            Ok(s) => s,
            Err(e) => {
                self.line(YELLOW, "!", &format!("turn {} checkpoint skipped: {e:#}", self.turn));
                return None;
            }
        };
        if started.elapsed() > checkpoint::SLOW && !self.checkpoint_slow_told {
            self.checkpoint_slow_told = true;
            self.line(
                YELLOW,
                "!",
                &format!(
                    "checkpoint took {:.1}s: a large file that isn't ignored? add it to .gitignore",
                    started.elapsed().as_secs_f64()
                ),
            );
        }
        snap
    }

    /// The turn's `after` checkpoint and what differs from its `before`.
    fn turn_changes(&mut self) -> Option<TurnChanges> {
        self.turn_before.as_ref()?;
        // `after` first: it reads `turn_before` to reuse it for an unchanged tree.
        let after = self.checkpoint(checkpoint::Which::After);
        let before = self.turn_before.take()?;
        let after = after?;
        let mut c = match checkpoint::changes(&self.cfg.cwd, &before, &after, &self.turn_authored) {
            Ok(c) => c,
            Err(e) => {
                self.line(YELLOW, "!", &format!("turn {} changes unknown: {e:#}", self.turn));
                return None;
            }
        };
        c.between_turns = self.last_after.as_ref().is_some_and(|l| l.tree != before.tree);
        self.last_after = Some(after);
        Some(c)
    }

    /// Remember the files an edit tool call names: its locations, else the
    /// path in its input.  Relative paths are taken from the workspace.
    fn note_authored(&mut self, locations: &[acp::ToolCallLocation], raw_input: &Value) {
        let mut paths: Vec<PathBuf> = locations.iter().map(|l| l.path.clone()).collect();
        if paths.is_empty()
            && let Some(p) = ["file_path", "path", "filePath", "notebook_path"].iter().find_map(|k| raw_input.get(k).and_then(Value::as_str))
        {
            paths.push(PathBuf::from(p));
        }
        for p in paths {
            let p = resolve(&self.cfg.cwd, &p);
            if !self.turn_authored.contains(&p) {
                self.turn_authored.push(p);
            }
        }
    }

    fn push(
        &self,
        kind: ItemKind,
        status: Option<ItemStatus>,
        ref_id: Option<String>,
        content: Value,
    ) -> Result<ConversationItem> {
        Ok(self.store.push_item(&NewItem {
            session_id: self.cfg.session_id.clone(),
            thread_id: self.cfg.thread_id.clone(),
            turn: self.turn,
            kind,
            status,
            ref_id,
            content,
        })?)
    }

    fn set_state(&mut self, state: SessionState) -> Result<()> {
        self.state = state;
        self.store.update_session(
            &self.signer,
            &self.cfg.session_id,
            &SessionUpdate {
                state: Some(state),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    /// End the session in the store (once) and say goodbye.
    fn finish(&mut self, reason: &str) {
        if self.ended {
            return;
        }
        self.ended = true;
        // The session is over: its record goes.
        self.program_status(axel_rex::osc::State::Clear, None, "");
        self.finalize_text();
        self.cancel_pending(reason);
        // Queued prompts never ran: keep them visible as cancelled.
        for q in std::mem::take(&mut self.queue) {
            if let Err(e) = self
                .store
                .update_item(&q.item_id, Some(ItemStatus::Cancelled), None)
            {
                self.line(RED, "✖", &format!("store: {e}"));
            }
        }
        if let Err(e) = self.store.set_session_socket(&self.cfg.session_id, None) {
            self.line(RED, "✖", &format!("store: {e}"));
        }
        if let Err(e) = self
            .store
            .end_session(&self.signer, &self.cfg.session_id, Some(reason))
        {
            self.line(RED, "✖", &format!("end_session: {e}"));
        }
        self.state = SessionState::Ended;
        self.line(CYAN, "○", &format!("session ended: {reason}"));
    }
}

// --- socket server -------------------------------------------------------------

async fn serve_socket(listener: UnixListener, tx: mpsc::UnboundedSender<Event>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let reply = match serde_json::from_str::<Command>(&line) {
                    Err(e) => Reply::Error {
                        message: format!("bad command: {e}"),
                    },
                    Ok(cmd) => {
                        let (rtx, rrx) = oneshot::channel();
                        if tx.send(Event::Command { cmd, reply: rtx }).is_err() {
                            Reply::Error {
                                message: "host is shutting down".into(),
                            }
                        } else {
                            rrx.await.unwrap_or(Reply::Error {
                                message: "host dropped the command".into(),
                            })
                        }
                    }
                };
                let mut s =
                    serde_json::to_string(&reply).unwrap_or_else(|_| r#"{"reply":"ok"}"#.into());
                s.push('\n');
                if w.write_all(s.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
}

// --- fs client methods ---------------------------------------------------------

fn resolve(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

async fn fs_read(
    cwd: &Path,
    req: acp::ReadTextFileRequest,
    responder: Responder<acp::ReadTextFileResponse>,
) -> Result<(), acp::Error> {
    let path = resolve(cwd, &req.path);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            let content = slice_lines(&text, req.line, req.limit);
            responder.respond(acp::ReadTextFileResponse::new(content))
        }
        Err(e) => responder.respond_with_error(
            acp::Error::resource_not_found(Some(path.display().to_string()))
                .data(json!({ "error": e.to_string() })),
        ),
    }
}

async fn fs_write(
    cwd: &Path,
    mode: RuntimeMode,
    req: acp::WriteTextFileRequest,
    responder: Responder<acp::WriteTextFileResponse>,
) -> Result<(), acp::Error> {
    if mode == RuntimeMode::Plan {
        return responder.respond_with_error(
            acp::Error::new(-32000, "file writes are denied in plan mode")
                .data(json!({ "path": req.path.display().to_string() })),
        );
    }
    let path = resolve(cwd, &req.path);
    if let Some(parent) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return responder.respond_with_error(
            acp::Error::internal_error().data(json!({ "error": e.to_string() })),
        );
    }
    match tokio::fs::write(&path, req.content).await {
        Ok(()) => responder.respond(acp::WriteTextFileResponse::new()),
        Err(e) => responder.respond_with_error(
            acp::Error::internal_error()
                .data(json!({ "path": path.display().to_string(), "error": e.to_string() })),
        ),
    }
}

/// `line` is 1-based; `limit` is a line count.
fn slice_lines(text: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
    let take = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let mut out: String = text
        .lines()
        .skip(skip)
        .take(take)
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

// --- small helpers -------------------------------------------------------------

/// The snake_case wire name of a unit-variant enum (via serde).
fn enum_str<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn tool_status(s: &str) -> ItemStatus {
    match s {
        "completed" => ItemStatus::Completed,
        "failed" => ItemStatus::Failed,
        _ => ItemStatus::Running,
    }
}

fn block_text(b: &acp::ContentBlock) -> String {
    match b {
        acp::ContentBlock::Text(t) => t.text.clone(),
        acp::ContentBlock::Image(_) => "[image]".into(),
        acp::ContentBlock::Audio(_) => "[audio]".into(),
        acp::ContentBlock::ResourceLink(_) => "[resource link]".into(),
        acp::ContentBlock::Resource(_) => "[resource]".into(),
        _ => "[content]".into(),
    }
}

fn locations_json(locs: &[acp::ToolCallLocation]) -> Value {
    Value::Array(
        locs.iter()
            .map(|l| json!({ "path": l.path.display().to_string(), "line": l.line }))
            .collect(),
    )
}

fn item_title(item: &ConversationItem) -> Option<String> {
    item.content
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Keep `v` if it serialises within `limit` bytes; otherwise its serialised head as a string.
/// `v`, cut to about `limit` bytes. Text stays text: a string, or text
/// blocks (`[{type: text, text}]`), is cut as text and stays readable; only
/// other JSON is cut as its serialization (stored as a string).
fn truncate_json(v: &Value, limit: usize) -> Value {
    if v.to_string().len() <= limit {
        return v.clone();
    }
    let cut = |s: &str| {
        let mut end = limit.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    };
    match v {
        Value::String(s) => Value::String(cut(s)),
        Value::Array(blocks) if !blocks.is_empty() && blocks.iter().all(|b| b.get("text").and_then(Value::as_str).is_some()) => {
            let text = blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
            Value::String(cut(&text))
        }
        other => Value::String(cut(&other.to_string())),
    }
}

/// First line of `s`, at most `n` chars.
fn head(s: &str, n: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(n).collect();
    if first.chars().count() > n || s.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// A one-line gist of a tool's input: its command, path, pattern, url or query.
fn input_summary(v: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return head(s, 100);
        }
    }
    if let Some(arr) = v.get("command").and_then(Value::as_array) {
        let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        return head(&parts.join(" "), 100);
    }
    String::new()
}

/// A turn's spend as the store keeps it: the harness's cost when it gave
/// one, else an estimate from the model's list prices.
fn usage_delta(u: &TurnUsage) -> axel_store::UsageDelta {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let (input, output, cache_read, cache_write) = (n(u.input), n(u.output), n(u.cache_read), n(u.cache_write));
    axel_store::UsageDelta {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: n(u.reasoning),
        turns: 1,
        cost_usd_est: u
            .cost_usd
            .unwrap_or_else(|| axel_store::pricing::estimate(u.model.as_deref(), input, output, cache_read, cache_write)),
        model: u.model.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_as_text_not_as_its_json() {
        let long = format!("line one\n{}", "x".repeat(100));
        let cut = truncate_json(&Value::String(long.clone()), 20);
        assert_eq!(cut, Value::String(format!("{}…", &long[..20])), "no quotes, no \\n escapes");
        let blocks = serde_json::json!([{ "type": "text", "text": "first\nsecond" }, { "type": "text", "text": "y".repeat(100) }]);
        assert_eq!(truncate_json(&blocks, 15), Value::String("first\nsecond\nyy…".into()), "text blocks become their text");
        let small = serde_json::json!({ "a": 1 });
        assert_eq!(truncate_json(&small, 100), small, "under the limit: as it was");
        assert_eq!(truncate_json(&Value::String("é".repeat(20)), 5), Value::String("éé…".into()), "cut on a character boundary");
    }

    #[test]
    fn truncation_and_heads() {
        let small = json!({"a": 1});
        assert_eq!(truncate_json(&small, 100), small);
        let big = Value::String("x".repeat(5000));
        let t = truncate_json(&big, 10);
        assert!(t.as_str().unwrap().ends_with('…'));
        assert!(t.as_str().unwrap().len() <= 14);
        assert_eq!(head("one\ntwo", 10), "one…");
        assert_eq!(head("short", 10), "short");
        assert_eq!(head("abcdefghij", 3), "abc…");
    }

    #[test]
    fn line_slicing() {
        let t = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(t, None, None), t);
        assert_eq!(slice_lines(t, Some(2), Some(2)), "b\nc\n");
        assert_eq!(slice_lines(t, Some(4), None), "d\n");
        assert_eq!(slice_lines(t, Some(9), None), "");
    }

    #[test]
    fn summaries() {
        assert_eq!(
            input_summary(&json!({"command": "cargo test"})),
            "cargo test"
        );
        assert_eq!(input_summary(&json!({"command": ["ls", "-la"]})), "ls -la");
        assert_eq!(input_summary(&json!({"file_path": "/a/b.rs"})), "/a/b.rs");
        assert_eq!(input_summary(&json!({})), "");
    }
}

