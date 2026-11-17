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
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use anyhow::{Context, Result};
use axel_i18n::t;
use axel_identity::Keypair;
use axel_store::{
    BlockedKind, ConversationItem, Harness, ItemKind, ItemStatus, NewItem, RuntimeMode,
    SessionState, SessionUpdate, StatusReport, StatusState, Store, TurnChanges, TurnSummary,
};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use crate::adapter::{adapter_command, runs_builtin_adapter};
use crate::agent_rpc::LineRead;
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
    /// The agent's reasoning effort, applied once the session exists (or, for
    /// Axel's own adapters, as it is created). `None` sends nothing.
    pub effort: Option<axel_store::Effort>,
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

/// The reasoning effort the host asked one of Axel's own adapters for, in
/// the `_meta.axel.effort` of `session/new` / `session/load` params.
pub(crate) fn meta_effort(params: &Value) -> Option<axel_store::Effort> {
    params.pointer("/_meta/axel/effort")?.as_str()?.parse().ok()
}

/// `meta` with the effort for one of Axel's own adapters, which take it when
/// the session is created rather than as a config option, and what to tell
/// the owner when the harness has no such level. `own_adapter` is whether
/// the session runs under one ([`runs_builtin_adapter`]): any other adapter,
/// an `AXEL_ACP_CLAUDE` or `AXEL_ACP_CODEX` override included, does not read
/// this and is asked through its config options instead.
fn meta_with_effort(
    harness: &Harness,
    own_adapter: bool,
    meta: Option<serde_json::Map<String, Value>>,
    effort: Option<axel_store::Effort>,
) -> (Option<serde_json::Map<String, Value>>, Option<String>) {
    let Some(effort) = effort.filter(|_| own_adapter) else { return (meta, None) };
    match harness.native_effort(effort) {
        Some(level) => {
            let mut meta = meta.unwrap_or_default();
            let axel = meta.entry("axel").or_insert_with(|| json!({}));
            if !axel.is_object() {
                *axel = json!({});
            }
            axel["effort"] = json!(level);
            (Some(meta), None)
        }
        None => (meta, Some(t!("could not set effort {effort}: {harness} has no such level; it runs at its default", effort = effort, harness = harness.label()))),
    }
}

/// The select option that sets how hard the model thinks: the one in ACP's
/// `thought_level` category, else one whose id or name says so. Among those
/// named ones, a selector of something else about the reasoning (its
/// summary) is passed over, and one that offers effort levels is preferred.
fn effort_selector(options: &[Value]) -> Option<&Value> {
    let selects = || options.iter().filter(|o| o.get("type").and_then(Value::as_str) == Some("select"));
    let category = |o: &Value| o.get("category").and_then(Value::as_str).map(str::to_owned);
    let says_effort = |o: &Value, key: &str| {
        let text = o.get(key).and_then(Value::as_str).unwrap_or("").to_lowercase();
        ["effort", "reasoning", "thinking", "thought"].iter().any(|word| text.contains(word))
    };
    let says_summary = |o: &Value| ["id", "name"].iter().any(|key| o.get(*key).and_then(Value::as_str).unwrap_or("").to_lowercase().contains("summary"));
    let offers_levels = |o: &Value| {
        select_values(o.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))
            .iter()
            .any(|(value, name)| axel_store::Effort::ALL.iter().any(|e| same_level(value, *e) || same_level(name, *e)))
    };
    selects().find(|o| category(o).as_deref() == Some("thought_level")).or_else(|| {
        let named = || {
            selects().filter(|o| {
                !matches!(category(o).as_deref(), Some("model" | "mode")) && (says_effort(o, "id") || says_effort(o, "name")) && !says_summary(o)
            })
        };
        named().find(|o| offers_levels(o)).or_else(|| named().next())
    })
}

/// A select option's `(value, name)` pairs, flat or grouped. ACP groups them
/// one level deep, and that is as far as this reads: a group inside a group
/// is passed over, so what an adapter sends cannot make it recurse.
fn select_values(options: &[Value]) -> Vec<(String, String)> {
    fn group(entry: &Value) -> Option<&Vec<Value>> {
        entry.get("options").and_then(Value::as_array)
    }
    fn pair(entry: &Value) -> Option<(String, String)> {
        let value = entry.get("value").and_then(Value::as_str)?;
        Some((value.to_string(), entry.get("name").and_then(Value::as_str).unwrap_or(value).to_string()))
    }
    options
        .iter()
        .flat_map(|entry| match group(entry) {
            Some(members) => members.iter().filter(|member| group(member).is_none()).filter_map(pair).collect(),
            None => Vec::from_iter(pair(entry)),
        })
        .collect()
}

/// The level a session stays at when the host could not change its effort:
/// what the harness has it at, which the selector's `currentValue` names
/// when it reports one. That is often not the default. `None` when it
/// reports none, and the warning says the effort is unchanged.
fn effort_left_at(selector: &Value) -> Option<&str> {
    selector.get("currentValue").and_then(Value::as_str).filter(|current| !current.is_empty())
}

/// Whether an advertised value or name is `effort`: `high`, `High`, `x-high`.
fn same_level(advertised: &str, effort: axel_store::Effort) -> bool {
    let plain: String = advertised.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
    plain == effort.as_str()
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

/// Stored tool input and output stop at this many bytes (a read file, a
/// command's output): enough to see it, not the store's whole size.
const RAW_LIMIT_BYTES: usize = 16 * 1024;
/// `TurnSummary.assistant_text` keeps this many chars.
const ASSISTANT_HEAD_CHARS: usize = 2000;

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
    // Unbounded on purpose: the ACP connection's handlers must never wait on
    // the loop (it may be awaiting them), and what goes in is bounded by its
    // sources: one adapter's frames, socket commands (at most
    // `CONNECTIONS_MAX` in flight) and the pane's EOF.
    let (tx, rx) = mpsc::unbounded_channel::<Event>();

    // Adapter.
    let cmd = adapter_command(&cfg.harness);
    let (stderr_tx, stderr_rx) = mpsc::unbounded_channel::<String>();
    let (child, transport_) = transport::spawn(&cmd, &cfg.cwd, &cfg.env, log.clone(), stderr_tx)?;
    forward_stderr(stderr_rx, tx.clone());

    let listener = bind_socket(&cfg.socket_path)?;
    let socket_task = tokio::spawn(serve_socket(listener, tx.clone()));
    // The pane closing shows up as EOF on our stdin — only meaningful when stdin is a terminal.
    if std::io::stdin().is_terminal() {
        watch_pane(tx.clone());
    }

    let socket_path = cfg.socket_path.clone();
    let mut host = Host::new(cfg, store, signer, out, cmd);
    host.line(CYAN, "●", &t!("adapter: {command}", command = host.adapter.join(" ")));
    let reason = serve_agent(&mut host, transport_, rx, &tx).await;

    socket_task.abort();
    host.finish(&reason);
    if let Err(e) = std::fs::remove_file(&socket_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        host.line(RED, "✖", &t!("removing {path}: {error}", path = socket_path.display(), error = e));
    }
    end_process_group(&child);
    drop(child); // kill_on_drop
    Ok(())
}

/// Connect to the adapter over `transport` and run the host until it stops;
/// the reason it stopped.  The agent's notifications and requests become
/// events on `tx`, but for file reads and writes, answered here.
async fn serve_agent<W: Write + Send>(
    host: &mut Host<W>,
    transport: impl ConnectTo<Client> + 'static,
    rx: mpsc::UnboundedReceiver<Event>,
    tx: &mpsc::UnboundedSender<Event>,
) -> String {
    let (tx_notif, tx_ext, tx_perm) = (tx.clone(), tx.clone(), tx.clone());
    let cwd = host.cfg.cwd.clone();
    let cwd2 = cwd.clone();
    // Shared with the fs handler so a mid-session mode switch reaches writes.
    let mode_cell = host.mode_cell.clone();
    let connected = Client
        .builder()
        .name("axel")
        .on_receive_notification(
            async move |n: acp::SessionNotification, _cx: ConnectionTo<Agent>| {
                // The loop is gone only when the host is shutting down.
                let _ = tx_notif.send(Event::Update(n));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: agent_client_protocol::UntypedMessage, _cx: ConnectionTo<Agent>| {
                if n.method().starts_with("_axel/") {
                    // The loop is gone only when the host is shutting down.
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
                // No loop to ask, or it dropped the question: cancelled.
                let (reply_tx, reply_rx) = oneshot::channel();
                let resp = match tx_perm.send(Event::Permission { req, reply: reply_tx }) {
                    Ok(()) => reply_rx.await.unwrap_or_else(|_| selected_or_cancelled(None)),
                    Err(_) => selected_or_cancelled(None),
                };
                responder.respond(resp)
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
                let mode = *mode_cell.lock().expect("mode lock: never held across a panic");
                fs_write(&cwd2, mode, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx: ConnectionTo<Agent>| {
            let reason = host.run(&cx, rx).await;
            Ok::<String, acp::Error>(reason)
        })
        .await;
    match connected {
        Ok(reason) => reason,
        Err(e) => format!("connection error: {e}"),
    }
}

/// The adapter's stderr lines, as events for the pane.
fn forward_stderr(mut stderr_rx: mpsc::UnboundedReceiver<String>, tx: mpsc::UnboundedSender<Event>) {
    tokio::spawn(async move {
        while let Some(line) = stderr_rx.recv().await {
            if tx.send(Event::Stderr(line)).is_err() {
                break;
            }
        }
    });
}

/// The command socket at `path`, in place of any left by a host that died.
fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // A socket left by a host that died; none is the usual case, and a real
    // problem with the path shows in `bind` below.
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

/// Send `StdinClosed` when the host's pane closes (its stdin ends).
fn watch_pane(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        // Nothing is typed into the host's pane: whatever is, is thrown
        // away, so this holds no memory, and EOF and a read error both
        // mean the pane is gone.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        // The loop may have ended already; then there is nobody to stop.
        let _ = tx.send(Event::StdinClosed);
    });
}

/// The adapter and everything it started (Claude, `npx` wrappers) share a
/// process group: end the group, so no harness outlives its session.
fn end_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: signalling a process group we created (`process_group(0)`).
        // It may be gone already (ESRCH): then there is nothing left to end.
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
        }
    }
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

/// How a turn ended: what the thread, the status and the pane say about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnOutcome {
    /// The agent finished (`end_turn`).
    Finished,
    /// The agent stopped short of finishing (`max_tokens`,
    /// `max_turn_requests`, `refusal`): the turn is done, and says why.
    StoppedShort,
    /// The owner cancelled it (`cancelled`).
    Cancelled,
    /// The prompt request failed: no stop reason at all.
    Failed,
}

impl TurnOutcome {
    fn of(stop_reason: acp::StopReason) -> TurnOutcome {
        match stop_reason {
            acp::StopReason::EndTurn => TurnOutcome::Finished,
            acp::StopReason::Cancelled => TurnOutcome::Cancelled,
            _ => TurnOutcome::StoppedShort,
        }
    }

    /// The status of the turn's `TurnEnd` item.
    fn item_status(self) -> ItemStatus {
        match self {
            TurnOutcome::Finished | TurnOutcome::StoppedShort => ItemStatus::Completed,
            TurnOutcome::Cancelled => ItemStatus::Cancelled,
            TurnOutcome::Failed => ItemStatus::Failed,
        }
    }
}

/// A reply that refuses a command, saying why.
fn error_reply(message: impl Into<String>) -> Reply {
    Reply::Error { message: message.into() }
}

/// ` · 3 files +10 −2` for a turn's line in the pane, when it changed files.
fn changes_note(changes: Option<&TurnChanges>) -> String {
    let Some(c) = changes.filter(|c| !c.files.is_empty()) else { return String::new() };
    let (added, removed) = c.totals();
    let count = c.files.len();
    let files = if count == 1 {
        t!("1 file +{added} −{removed}", added = added, removed = removed)
    } else {
        t!("{count} files +{added} −{removed}", count = count, added = added, removed = removed)
    };
    format!(" · {files}")
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
    /// `(option_id, kind)` of each option offered.
    options: Vec<(String, acp::PermissionOptionKind)>,
    multiple: Option<(usize,usize)>,
}

/// The tool call a permission request is about.
struct PermissionCall {
    /// The ACP `toolCallId`.
    ref_id: String,
    title: String,
    kind: ToolKind,
    raw_input: Value,
}

impl PermissionCall {
    /// The `ref_id` of its `Permission` item: never the tool call's own, so
    /// tool call updates never touch it.
    fn permission_ref(&self) -> String {
        format!("permission:{}", self.ref_id)
    }

    /// The title, with the gist of the input, for the pane.
    fn label(&self) -> String {
        let summary = input_summary(&self.raw_input);
        if summary.is_empty() {
            self.title.clone()
        } else {
            format!("{}  {DIM}{summary}{RESET}", self.title)
        }
    }
}

fn is_allow(kind: acp::PermissionOptionKind) -> bool {
    matches!(kind, acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways)
}

fn is_reject(kind: acp::PermissionOptionKind) -> bool {
    matches!(kind, acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways)
}

/// The option of kind `prefer`, else the first of its family.
fn pick_option(options: &[(String, acp::PermissionOptionKind)], prefer: acp::PermissionOptionKind, family: fn(acp::PermissionOptionKind) -> bool) -> Option<String> {
    debug_assert!(family(prefer), "the preferred kind belongs to its family");
    options
        .iter()
        .find(|(_, kind)| *kind == prefer)
        .or_else(|| options.iter().find(|(_, kind)| family(*kind)))
        .map(|(id, _)| id.clone())
}

/// The answer that picks `option_id`, or cancels when there is none to pick.
fn selected_or_cancelled(option_id: Option<String>) -> acp::RequestPermissionResponse {
    let outcome = match option_id {
        Some(id) => acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(id))),
        None => acp::RequestPermissionOutcome::Cancelled,
    };
    acp::RequestPermissionResponse::new(outcome)
}

/// How many choices a multi-select question takes, `(min, max)`; `None`
/// for a question with one answer.  Bounds the choices can't meet are an error.
fn selection_bounds(raw_input: &Value, options: &[(String, acp::PermissionOptionKind)]) -> Result<Option<(usize, usize)>> {
    if raw_input.get("multiSelect").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let count = options.iter().filter(|(_, kind)| is_allow(*kind)).count();
    // Saturating: an absurd bound fails the check below rather than wrapping.
    let bound = |key: &str| raw_input.get(key).and_then(Value::as_u64).map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let min = bound("minSelections").unwrap_or(1);
    let max = bound("maxSelections").unwrap_or(count);
    anyhow::ensure!(min <= max && max <= count, "invalid question selection bounds");
    Ok(Some((min, max)))
}

/// Composite answers are an Axel extension for explicit multi-select questions,
/// never a way to select unoffered options or combine permission grants.
fn validate_answer(options: &[(String, acp::PermissionOptionKind)], multiple: Option<(usize,usize)>, answer: &str) -> Result<bool> {
    if let Some(encoded)=answer.strip_prefix(crate::claude::MULTI_ANSWER_PREFIX) {
        let Some((min,max))=multiple else { anyhow::bail!("this question does not allow multiple selections") };
        let ids:Vec<String>=serde_json::from_str(encoded)?;
        anyhow::ensure!(ids.len()>=min && ids.len()<=max,"select {min}–{max} options");
        let unique:std::collections::HashSet<_>=ids.iter().collect();
        anyhow::ensure!(unique.len()==ids.len(),"duplicate selections");
        for id in ids {
            anyhow::ensure!(options.iter().any(|(offered,kind)|offered==&id && is_allow(*kind)),"option {id} is not an offered selection");
        }
        return Ok(true)
    }
    let Some((_,kind))=options.iter().find(|(id,_)|id==answer) else { anyhow::bail!("option {answer} is not offered") };
    let allowed=is_allow(*kind);
    if allowed && let Some((min,max))=multiple {anyhow::ensure!(min<=1 && max>=1,"select {min}–{max} options");}
    Ok(allowed)
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

/// The most items read back to find a turn's last message: far more than
/// any session holds, so the newest are always in it.
const SESSION_ITEMS_MAX: usize = 100_000;

/// Prompts that may wait at once, queued or sent into the turn.  A person
/// queues a handful; past this, something (a script, two agents answering
/// each other) is looping, and its prompts are refused.
const QUEUED_MAX: usize = 64;

/// How long a stopping host waits for the socket's last reply to go out.
const STOP_DRAIN: Duration = Duration::from_millis(50);

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
    /// The message or thought being streamed, until another kind of
    /// update closes it.
    open_text: Option<TextItem>,
    /// Pending `Permission` items by item id.  Permission items carry
    /// `ref_id = "permission:<toolCallId>"` so tool-call updates never touch them.
    pending: HashMap<String, PendingPermission>,
    /// Prompts waiting for the running turn to end, next first.
    queue: VecDeque<Queued>,
    /// The adapter can take prompts into a running turn (`_axel/steer`).
    can_steer: bool,
    /// The adapter takes the skills a prompt names (`_meta.axel.skills`) and
    /// makes them its harness's own; otherwise the agent is pointed at their files.
    can_take_skills: bool,
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
            open_text: None,
            pending: HashMap::new(),
            queue: VecDeque::new(),
            can_steer: false,
            can_take_skills: false,
            steering: VecDeque::new(),
            mode_cell,
            native_modes: None,
            turn_from: None,
            ended: false,
            seen_through: None,
        }
    }

    // --- output ---

    /// Write to the host's pane.  The pane only shows what happens: a write
    /// that fails (it was closed) must not stop the session, whose record is
    /// the store, so the error is dropped here, once for every writer.
    fn emit(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = self.out.write_fmt(args);
        let _ = self.out.flush();
    }

    fn line(&mut self, colour: &str, glyph: &str, text: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.emit(format_args!("{DIM}{ts}{RESET} {colour}{glyph}{RESET} {text}\n"));
    }

    /// A store write that failed: said in the pane, the session goes on.
    fn store_failed(&mut self, error: &dyn std::fmt::Display) {
        self.line(RED, "✖", &t!("store: {error}", error = error));
    }

    /// A desktop notification (OSC 9) for the terminal the host runs in, when
    /// `$AXEL_NOTIFY` is set: remote jobs run in a Rex pane on their host, and
    /// Rex shows it on every client attached there.
    fn notify(&mut self, text: &str) {
        if std::env::var_os("AXEL_NOTIFY").is_none() {
            return;
        }
        let clean: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
        self.emit(format_args!("\x1b]9;{clean}\x07"));
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
        let encoded = axel_rex::osc::encode(&report);
        self.emit(format_args!("{encoded}"));
    }

    fn dim(&mut self, text: &str) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.emit(format_args!("{DIM}{ts}   {text}{RESET}\n"));
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
                let error = format!("{e:#}");
                self.line(RED, "✖", &t!("setup failed: {error}", error = error));
                // On the thread too: otherwise the agent just shows as done.
                let msg = setup_failure_message(&self.cfg.harness, &error);
                self.record_error(&msg);
                // The session's stored end reason: kept in English, like the others.
                return format!("setup failed: {error}");
            }
        }
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { return "host channel closed".into() };
                    let action = self.handle_event(cx, ev, in_flight.is_some());
                    if let Some(reason) = self.act(cx, action, &mut in_flight).await {
                        return reason;
                    }
                    self.start_queued(cx, &mut in_flight);
                }
                res = async { in_flight.as_mut().expect("polled only while a prompt is in flight").await }, if in_flight.is_some() => {
                    in_flight = None;
                    let deferred = self.drain_turn_events(cx, &mut rx).await;
                    self.end_turn(res);
                    for ev in deferred {
                        let action = self.handle_event(cx, ev, in_flight.is_some());
                        if let Some(reason) = self.act(cx, action, &mut in_flight).await {
                            return reason;
                        }
                    }
                    self.start_queued(cx, &mut in_flight);
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

    /// Carry out what handling an event asked for.  `Some` with the reason
    /// when the host is to stop.
    async fn act(&mut self, cx: &ConnectionTo<Agent>, action: Action, in_flight: &mut Option<PromptFuture>) -> Option<String> {
        match action {
            Action::None => {}
            Action::StartPrompt(text, files, from) => {
                assert!(in_flight.is_none(), "a prompt starts only between turns");
                *in_flight = Some(self.start_turn(cx, text, files, from));
            }
            Action::SetMode(mode) => self.set_mode(cx, mode).await,
            Action::Steer(item_id) => self.send_steer(cx, &item_id).await,
            Action::Withdraw { item_id, edit, reply } => {
                let r = self.withdraw(cx, &item_id, edit).await;
                // The client may have given up waiting; the withdrawal stands.
                let _ = reply.send(r);
            }
            Action::Stop => {
                // Let the socket reply drain before the listener is torn down.
                tokio::time::sleep(STOP_DRAIN).await;
                return Some("stopped".into());
            }
        }
        None
    }

    /// A prompt the turn couldn't take (it ended meanwhile) waits in the
    /// queue: start it once no turn is running.
    fn start_queued(&mut self, cx: &ConnectionTo<Agent>, in_flight: &mut Option<PromptFuture>) {
        if in_flight.is_none()
            && let Some((text, files, from)) = self.take_queued(None)
        {
            *in_flight = Some(self.start_turn(cx, text, files, from));
        }
    }

    /// The agent's last `session/update`s precede its response on the wire
    /// but reach us through the channel: project them before the turn is
    /// closed.  Commands and the like wait for after; they are returned.
    async fn drain_turn_events(&mut self, cx: &ConnectionTo<Agent>, rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        tokio::task::yield_now().await;
        let mut deferred = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Update(_) | Event::Permission { .. } | Event::Stderr(_) | Event::Ext { .. } => {
                    let action = self.handle_event(cx, ev, true);
                    debug_assert!(matches!(action, Action::None), "only commands ask for an action");
                }
                other => deferred.push(other),
            }
        }
        deferred
    }

    /// initialize → session/new (or session/load); returns the initial prompt to send.
    async fn setup(&mut self, cx: &ConnectionTo<Agent>) -> Result<Option<(String, Vec<PathBuf>)>> {
        let can_load = self.initialize(cx).await?;
        let (sid, modes, raw) = self.open_session(cx, can_load).await?;
        self.apply_native_mode(cx, &sid, modes.as_ref()).await;
        let options = self.apply_model(cx, &sid, &raw).await;
        self.apply_effort(cx, &sid, options.as_ref().or(raw.get("configOptions"))).await;
        self.line(CYAN, "●", &t!("session {session} · cwd {cwd}", session = sid, cwd = self.cfg.cwd.display()));
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

    /// `initialize`: what the agent takes in prompts, whether it takes
    /// prompts into a running turn; whether it can load a session.
    async fn initialize(&mut self, cx: &ConnectionTo<Agent>) -> Result<bool> {
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
        let axel_cap = |name: &str| {
            init.agent_capabilities.meta.as_ref().and_then(|m| m.get("axel")).and_then(|a| a.get(name)).and_then(Value::as_bool).unwrap_or(false)
        };
        self.can_steer = axel_cap("steer");
        self.can_take_skills = axel_cap("skills");
        let agent_name = init
            .agent_info
            .as_ref()
            .map(|i| format!("{} {}", i.name, i.version))
            .unwrap_or_else(|| self.cfg.harness.to_string());
        let protocol = format!("{:?}", init.protocol_version);
        self.line(CYAN, "●", &t!("initialized: {agent} (protocol v{protocol})", agent = agent_name, protocol = protocol));
        Ok(init.agent_capabilities.load_session)
    }

    /// `session/load` the native session to resume, when there is one and
    /// the agent can; else `session/new`.  The session id, its modes, and
    /// the raw reply (the typed one drops fields adapters add).
    async fn open_session(&mut self, cx: &ConnectionTo<Agent>, can_load: bool) -> Result<(acp::SessionId, Option<acp::SessionModeState>, Value)> {
        let session_meta = self.session_meta();
        let mcp_servers: Vec<acp::McpServer> = self.cfg.mcp_servers.iter().map(McpStdio::to_acp).collect();
        match (self.cfg.resume_native_id.clone(), can_load) {
            (Some(native), true) => {
                let params = acp::LoadSessionRequest::new(native.clone(), self.cfg.cwd.clone())
                    .mcp_servers(mcp_servers)
                    .meta(session_meta);
                let raw = self.raw_request(cx, "session/load", params).await?;
                let resp: acp::LoadSessionResponse = serde_json::from_value(raw.clone()).context("session/load reply")?;
                Ok((acp::SessionId::new(native), resp.modes, raw))
            }
            (resume, _) => {
                if resume.is_some() {
                    self.dim(t!("agent cannot load sessions; starting a new one"));
                }
                let params = acp::NewSessionRequest::new(self.cfg.cwd.clone())
                    .mcp_servers(mcp_servers)
                    .meta(session_meta);
                let raw = self.raw_request(cx, "session/new", params).await?;
                let resp: acp::NewSessionResponse = serde_json::from_value(raw.clone()).context("session/new reply")?;
                Ok((resp.session_id, resp.modes, raw))
            }
        }
    }

    /// A request by method name, returning the raw reply: the typed schema
    /// drops fields adapters add (Claude's `models`).
    async fn raw_request(&self, cx: &ConnectionTo<Agent>, method: &str, params: impl serde::Serialize) -> Result<Value> {
        let msg = agent_client_protocol::UntypedMessage::new(method, params).map_err(|e| anyhow::anyhow!("{method}: {e}"))?;
        cx.send_request(msg).block_task().await.map_err(|e| anyhow::anyhow!("{method}: {e}"))
    }

    /// Switch to the agent's model, when it has one.  The adapter picks its
    /// own default right after `session/new`, so this has to come after.
    /// Returns the config options the harness answered with, when it did:
    /// what a model offers (its effort levels, say) can change with the model.
    async fn apply_model(&mut self, cx: &ConnectionTo<Agent>, sid: &acp::SessionId, session_reply: &Value) -> Option<Value> {
        let want = self.cfg.model.clone().filter(|m| !m.trim().is_empty())?;
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
            self.line(CYAN, "●", &t!("model {model}", model = id));
            return None;
        }
        let (method, params) = match selector {
            Some(option) => ("session/set_config_option", json!({ "sessionId": sid.to_string(), "configId": option["id"], "value": id })),
            None => ("session/set_model", json!({ "sessionId": sid.to_string(), "modelId": id })),
        };
        match self.raw_request(cx, method, params).await {
            Ok(reply) => {
                self.line(CYAN, "●", &t!("model {model}", model = id));
                reply.get("configOptions").filter(|o| o.as_array().is_some_and(|a| !a.is_empty())).cloned()
            }
            Err(e) => {
                let listed: Vec<&str> = available.iter().filter_map(|m| m.get("modelId")?.as_str()).collect();
                let msg = if current.is_empty() {
                    t!("could not switch the agent to model {model} ({error}); it runs its default", model = want, error = e)
                } else {
                    t!("could not switch the agent to model {model} ({error}); it runs {current}", model = want, error = e, current = current)
                };
                let msg = if listed.is_empty() { msg } else { t!("{message} · available: {models}", message = msg, models = listed.join(", ")) };
                self.warn(&msg);
                None
            }
        }
    }

    /// A problem the owner should see that does not stop the session: in
    /// the host's log and as an item on the thread.
    fn warn(&mut self, msg: &str) {
        self.line(YELLOW, "!", msg);
        self.record_error(msg);
    }

    /// An `Error` item on the thread, so viewers see what went wrong.
    fn record_error(&mut self, msg: &str) {
        if let Err(e) = self.push(ItemKind::Error, Some(ItemStatus::Failed), None, json!({ "message": msg })) {
            self.store_failed(&e);
        }
    }

    /// `_meta` for `session/new` / `session/load`. Axel's own adapters take
    /// the agent's effort here and start the harness with it; a level the
    /// harness does not have is left out and reported.
    fn session_meta(&mut self) -> Option<serde_json::Map<String, Value>> {
        let own_adapter = runs_builtin_adapter(&self.cfg.harness);
        let (meta, warning) = meta_with_effort(&self.cfg.harness, own_adapter, self.cfg.session_meta.clone(), self.cfg.effort);
        if let Some(warning) = warning {
            self.warn(&warning);
        }
        meta
    }

    /// Put a harness that does not run under Axel's own adapter at the
    /// agent's effort, through the effort selector in its ACP
    /// `configOptions`, and only with a value it advertises. Anything else
    /// is said, and the session keeps the level the harness has it at (the
    /// selector's `currentValue`, named when reported), not its default.
    async fn apply_effort(&mut self, cx: &ConnectionTo<Agent>, sid: &acp::SessionId, options: Option<&Value>) {
        let Some(effort) = self.cfg.effort else { return };
        if runs_builtin_adapter(&self.cfg.harness) {
            return;
        }
        let harness = self.cfg.harness.label().to_string();
        let options = options.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let Some(selector) = effort_selector(options) else {
            self.warn(&t!("could not set effort {effort}: {harness} offers no effort option; its effort is unchanged", effort = effort, harness = harness));
            return;
        };
        let offered = select_values(selector.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
        let Some((value, _)) = offered.iter().find(|(value, name)| same_level(value, effort) || same_level(name, effort)) else {
            let levels = offered.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>().join(", ");
            self.warn(&match effort_left_at(selector) {
                Some(current) => t!(
                    "could not set effort {effort}: {harness} offers {levels}; it stays at {current}",
                    effort = effort, harness = harness, levels = levels, current = current,
                ),
                None => t!("could not set effort {effort}: {harness} offers {levels}; its effort is unchanged", effort = effort, harness = harness, levels = levels),
            });
            return;
        };
        if selector.get("currentValue").and_then(Value::as_str) == Some(value) {
            self.line(CYAN, "●", &t!("effort {level}", level = value));
            return;
        }
        let params = json!({ "sessionId": sid.to_string(), "configId": selector["id"], "value": value });
        match self.raw_request(cx, "session/set_config_option", params).await {
            Ok(_) => self.line(CYAN, "●", &t!("effort {level}", level = value)),
            Err(e) => self.warn(&match effort_left_at(selector) {
                Some(current) => t!(
                    "could not set effort {effort} on {harness} ({error}); it stays at {current}",
                    effort = effort, harness = harness, error = e, current = current,
                ),
                None => t!("could not set effort {effort} on {harness} ({error}); its effort is unchanged", effort = effort, harness = harness, error = e),
            }),
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
            self.dim(&t!("agent has no {mode} mode; staying in {current}", mode = want, current = modes.current_mode_id));
            return;
        }
        let res = cx
            .send_request(acp::SetSessionModeRequest::new(sid.clone(), want.to_string()))
            .block_task()
            .await;
        match res {
            Ok(_) => {
                self.line(CYAN, "●", &t!("mode {mode} ({runtime_mode})", mode = want, runtime_mode = self.cfg.runtime_mode));
                if let Some(m) = self.native_modes.as_mut() {
                    m.current_mode_id = acp::SessionModeId::new(want.to_string());
                }
            }
            Err(e) => {
                let msg = t!(
                    "could not switch the agent to {mode} for {runtime_mode} ({error}); it stays in {current}, so background sub-agents may be refused",
                    mode = want, runtime_mode = self.cfg.runtime_mode, error = e, current = modes.current_mode_id,
                );
                self.warn(&msg);
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
            self.store_failed(&e);
        }
        self.line(CYAN, "●", &t!("runtime mode → {mode}", mode = mode));
        if let Some(sid) = self.native_session_id.clone() {
            let modes = self.native_modes.clone();
            self.apply_native_mode(cx, &sid, modes.as_ref()).await;
        }
    }

    fn handle_event(&mut self, cx: &ConnectionTo<Agent>, ev: Event, in_turn: bool) -> Action {
        match ev {
            Event::Update(n) => {
                if let Err(e) = self.on_update(n.update) {
                    self.store_failed(&e);
                }
                Action::None
            }
            Event::Permission { req, reply } => {
                if let Err(e) = self.on_permission(req, reply) {
                    self.line(RED, "✖", &t!("permission: {error}", error = e));
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
                // The client may have hung up; what the command did stands.
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
            Command::Prompt { text, attachments, steer, from } => self.on_prompt(cx, text, attachments, steer, from, in_turn),
            Command::Note { text, from } => (self.on_note(&text, &from), Action::None),
            Command::Steer { item_id } => self.on_steer(cx, &item_id, in_turn),
            Command::Dequeue { item_id } => (self.on_dequeue(&item_id), Action::None),
            Command::Edit { item_id, text } => (self.on_edit(&item_id, text), Action::None),
            Command::SetMode { mode } => (Reply::Ok, Action::SetMode(mode)),
            Command::Cancel => {
                self.cancel_pending("cancelled");
                if in_turn && let Err(e) = self.cancel_turn(cx) {
                    return (error_reply(e), Action::None);
                }
                (Reply::Ok, Action::None)
            }
            Command::Answer { item_id, option_id } => match self.answer(&item_id, &option_id) {
                Ok(()) => (Reply::Ok, Action::None),
                Err(e) => (error_reply(e.to_string()), Action::None),
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

    /// A prompt: it starts a turn, goes into the running one, or waits.
    fn on_prompt(
        &mut self,
        cx: &ConnectionTo<Agent>,
        text: String,
        attachments: Vec<PathBuf>,
        steer: bool,
        from: Option<Sender>,
        in_turn: bool,
    ) -> (Reply, Action) {
        if text.trim().is_empty() && attachments.is_empty() {
            return (error_reply("empty prompt"), Action::None);
        }
        if !in_turn {
            return (Reply::Ok, Action::StartPrompt(text, attachments, from));
        }
        // The owner's message goes into the running turn. Another
        // agent's waits for its own turn, so its answer goes back to it.
        if !steer && self.can_steer && from.is_none() {
            return match self.record_steer(text, attachments) {
                Ok(item_id) => (Reply::Steering { item_id: item_id.clone() }, Action::Steer(item_id)),
                Err(e) => (error_reply(format!("store: {e}")), Action::None),
            };
        }
        match self.enqueue(text, attachments, steer, from) {
            Ok(item_id) => {
                if steer && let Err(e) = self.cancel_turn(cx) {
                    return (error_reply(e), Action::None);
                }
                (Reply::Queued { item_id }, Action::None)
            }
            Err(e) => (error_reply(format!("queue: {e}")), Action::None),
        }
    }

    /// A note from another agent: on the thread, no turn.
    fn on_note(&mut self, text: &str, from: &Sender) -> Reply {
        if text.trim().is_empty() {
            return error_reply("empty note");
        }
        let content = json!({ "text": text, "from": from.pubkey, "fyi": true });
        match self.push(ItemKind::UserMessage, Some(ItemStatus::Completed), None, content) {
            Ok(_) => {
                self.line(DIM, "✎", &t!("note from {name}  {text}", name = from.name, text = head(text, 100)));
                Reply::Ok
            }
            Err(e) => error_reply(format!("store: {e}")),
        }
    }

    /// Send a queued prompt now: it starts a turn, goes into the running
    /// one, or goes first and cancels it.
    fn on_steer(&mut self, cx: &ConnectionTo<Agent>, item_id: &str, in_turn: bool) -> (Reply, Action) {
        let Some(pos) = self.queue.iter().position(|q| q.item_id == item_id) else {
            return (error_reply("that message is no longer queued"), Action::None);
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
            Err(e) => (error_reply(e), Action::None),
        }
    }

    /// Withdraw a queued prompt.
    fn on_dequeue(&mut self, item_id: &str) -> Reply {
        let Some(pos) = self.queue.iter().position(|q| q.item_id == item_id) else {
            return error_reply("that message is no longer queued");
        };
        self.queue.remove(pos);
        if let Err(e) = self.store.delete_item(item_id) {
            self.store_failed(&e);
        }
        self.dim(&t!("dequeued {item}", item = item_id));
        Reply::Ok
    }

    /// Change the text of a queued prompt.
    fn on_edit(&mut self, item_id: &str, text: String) -> Reply {
        if text.trim().is_empty() {
            return error_reply("empty message: remove it instead");
        }
        let Some(q) = self.queue.iter_mut().find(|q| q.item_id == item_id) else {
            return error_reply("the agent has read that message already");
        };
        q.text = text.clone();
        self.patch_item(item_id, json!({ "text": text }));
        self.dim(&t!("edited {item}", item = item_id));
        Reply::Ok
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
        self.line(YELLOW, "■", &t!("cancelling turn {turn}", turn = self.turn));
        Ok(())
    }

    /// Persist a prompt sent mid-turn as a pending `UserMessage` and queue it
    /// (first when steering).  Returns the item id.
    fn enqueue(&mut self, text: String, files: Vec<PathBuf>, steer: bool, from: Option<Sender>) -> Result<String> {
        self.ensure_room_to_queue()?;
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
        let text = format!("{DIM}{}{RESET}", head(&q.text, 100));
        self.line(BLUE, "…", &if steer { t!("steering  {text}", text = text) } else { t!("queued  {text}", text = text) });
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
        self.ensure_room_to_queue()?;
        let paths = attach::attachment_paths(&files);
        let content = json!({ "text": text, "attachments": paths, "queued": true, "steering": true });
        let item = self.push(ItemKind::UserMessage, Some(ItemStatus::Pending), None, content)?;
        self.line(BLUE, "↳", &t!("into the turn  {text}", text = format!("{DIM}{}{RESET}", head(&text, 100))));
        self.steering.push_back(Queued { item_id: item.id.clone(), text, files, from: None });
        Ok(item.id)
    }

    /// Refuse another waiting prompt past [`QUEUED_MAX`].
    fn ensure_room_to_queue(&self) -> Result<()> {
        let waiting = self.queue.len() + self.steering.len();
        // Every path that adds a waiting prompt comes through here, so the
        // limit holds; a prompt going back from `steering` only moves.
        debug_assert!(waiting <= QUEUED_MAX);
        anyhow::ensure!(waiting < QUEUED_MAX, "{waiting} messages are already waiting; send this once the agent has read some");
        Ok(())
    }

    /// Hand a prompt in `steering` to the adapter for the running turn. If it
    /// can't take it (the turn just ended), it waits in the queue instead.
    async fn send_steer(&mut self, cx: &ConnectionTo<Agent>, item_id: &str) {
        let Some(q) = self.steering.iter().find(|q| q.item_id == item_id) else { return };
        let (text, files) = (q.text.clone(), q.files.clone());
        let (wire, meta) = self.with_skills(&text, text.clone());
        let blocks = self.prompt_blocks(&wire, &files);
        let Some(sid) = self.native_session_id.clone() else { return };
        let mut params = json!({ "sessionId": sid.to_string(), "messageId": item_id, "prompt": blocks });
        if let Some(meta) = meta {
            params["_meta"] = Value::Object(meta);
        }
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
        debug_assert!(!self.queue.iter().chain(&self.steering).any(|w| w.item_id == item_id), "a prompt waits in one place only");
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
                self.dim(&t!("edited {item}", item = item_id));
                self.send_steer(cx, item_id).await;
            }
            None => {
                self.steering.retain(|q| q.item_id != item_id);
                if let Err(e) = self.store.delete_item(item_id) {
                    self.store_failed(&e);
                }
                self.dim(&t!("withdrew {item}", item = item_id));
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
                    self.store_failed(&e);
                }
                self.line(BLUE, "↳", &t!("read  {text}", text = format!("{DIM}{}{RESET}", head(&q.text, 100))));
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
                    self.line(RED, "✖", &t!("usage: {error}", error = e));
                } else {
                    self.session_cost += delta.cost_usd_est;
                }
            }
            "_axel/limits" => {
                for l in crate::usage::limits_from_params(params) {
                    if let Err(e) = self.store.set_plan_limit(&self.cfg.harness, l.window, l.used_pct, l.resets_at) {
                        self.store_failed(&e);
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
            self.store_failed(&e);
        }
    }

    /// Pop a queued prompt (the first, or the one at `pos`) and drop its
    /// placeholder item: `start_turn` records the message again at its real
    /// place in the timeline.
    fn take_queued(&mut self, pos: Option<usize>) -> Option<(String, Vec<PathBuf>, Option<Sender>)> {
        let q = self.queue.remove(pos.unwrap_or(0))?;
        if let Err(e) = self.store.delete_item(&q.item_id) {
            self.store_failed(&e);
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
            .items(&self.cfg.session_id, 0, SESSION_ITEMS_MAX)
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
            Ok(o) if o.status.success() => self.line(DIM, "↩", &t!("answer returned to {name}", name = from.name)),
            Ok(o) => {
                let error = String::from_utf8_lossy(&o.stderr).trim().to_string();
                self.line(RED, "✖", &t!("returning the answer to {name}: {error}", name = from.name, error = error));
            }
            Err(e) => self.line(RED, "✖", &t!("returning the answer to {name}: {error}", name = from.name, error = e)),
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
            self.store_failed(&e);
        }
        // The thread shows the message as written; the agent also reads who
        // wrote it, and first what it missed on the thread.
        let framed = from.as_ref().map_or_else(|| text.clone(), |f| f.frame(&text));
        let catch_up = self.catch_up();
        self.seen_through = Some(chrono::Utc::now().timestamp_millis());
        let wire_text = crate::catchup::prepend(catch_up.as_deref(), &framed);
        self.reset_turn_tallies();
        self.announce_turn_start(&text);
        // Still before session/prompt, so the agent's first edit cannot enter
        // the before snapshot. Only message visibility moves ahead of this I/O.
        self.turn_before = self.checkpoint(checkpoint::Which::Before);
        let shown = format!("{DIM}{}{RESET}", head(&text, 100));
        let line = if files.is_empty() {
            t!("prompt {turn}  {text}", turn = self.turn, text = shown)
        } else {
            let attached = format!("{DIM}{}{RESET}", t!("+{count} file(s)", count = files.len()));
            t!("prompt {turn}  {text} {files}", turn = self.turn, text = shown, files = attached)
        };
        self.line(BOLD, "▶", &line);
        let (wire_text, meta) = self.with_skills(&text, wire_text);
        let blocks = self.prompt_blocks(&wire_text, &files);
        let sid = self
            .native_session_id
            .clone()
            .expect("session created before prompting");
        let fut = cx
            .send_request(acp::PromptRequest::new(sid, blocks).meta(meta))
            .block_task();
        Box::pin(fut)
    }

    /// What a turn counts starts at zero.
    fn reset_turn_tallies(&mut self) {
        self.turn_assistant_text.clear();
        self.turn_tool_calls = 0;
        self.turn_files_edited = 0;
        self.turn_authored.clear();
        self.turn_progress_cost = 0.0;
        self.turn_progress = false;
        self.open_text = None;
    }

    /// The session is working on `text`: its state, its status, and the
    /// pane's program status.
    fn announce_turn_start(&mut self, text: &str) {
        if let Err(e) = self.set_state(SessionState::Running) {
            self.store_failed(&e);
        }
        if let Err(e) = self.store.record_status(
            &self.signer,
            &self.cfg.session_id,
            &StatusReport {
                state: StatusState::Working,
                title: Some(format!("turn {}", self.turn)),
                msg: Some(head(text, 120)),
                ..Default::default()
            },
        ) {
            self.line(RED, "✖", &t!("status: {error}", error = e));
        }
        self.program_status(axel_rex::osc::State::Working, None, &head(text, 200));
    }

    /// The skills `written` (the message as its author wrote it) names, for
    /// the agent: as `_meta.axel.skills` when its adapter takes them (it makes
    /// them its harness's own), or else as a note added to `wire`, the text
    /// it reads, naming each skill's file. `wire` as it goes out, and the
    /// prompt's `_meta`.
    fn with_skills(&mut self, written: &str, wire: String) -> (String, Option<acp::Meta>) {
        if !written.contains('/') {
            return (wire, None);
        }
        let Some(home) = dirs::home_dir() else { return (wire, None) };
        let global = axel_config::default_global_skills_dir();
        let known = axel_config::skill_hints(&axel_config::all_skill_dirs(std::slice::from_ref(&self.cfg.cwd), &home, global.as_deref()));
        let named = axel_config::named_skills(written, &known);
        if named.is_empty() {
            return (wire, None);
        }
        let names: Vec<&str> = named.iter().map(|s| s.name.as_str()).collect();
        self.line(DIM, "·", &t!("skills: {names}", names = names.join(", ")));
        if self.can_take_skills {
            (wire, Some(crate::skills::prompt_meta(&named)))
        } else {
            let files: Vec<(&str, &std::path::Path)> = named.iter().map(|s| (s.name.as_str(), s.path.as_path())).collect();
            (format!("{wire}\n\n{}", axel_config::skill_pointers(&files)), None)
        }
    }

    /// A prompt's content blocks, with a warning in the pane for each
    /// attachment that couldn't go in as it is.
    fn prompt_blocks(&mut self, text: &str, files: &[PathBuf]) -> Vec<acp::ContentBlock> {
        let mut warnings = Vec::new();
        let blocks = attach::build_prompt_blocks(text, files, self.prompt_caps, &mut |w| warnings.push(w));
        for w in warnings {
            self.line(YELLOW, "!", &w);
        }
        blocks
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
                self.line(RED, "✖", &t!("usage: {error}", error = e));
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
            self.line(RED, "✖", &t!("usage: {error}", error = e));
        } else {
            self.session_cost += delta.cost_usd_est;
        }
    }

    fn end_turn(&mut self, res: Result<acp::PromptResponse, acp::Error>) {
        self.close_turn_items();
        let duration_ms = i64::try_from(self.turn_started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let (outcome, stop_reason) = match &res {
            Ok(resp) => (TurnOutcome::of(resp.stop_reason), enum_str(&resp.stop_reason)),
            Err(e) => {
                self.record_error(&e.to_string());
                (TurnOutcome::Failed, "error".to_string())
            }
        };
        if let Ok(resp) = &res {
            self.record_usage(resp);
        }
        let changes = self.turn_changes();
        let changed = changes_note(changes.as_ref());
        self.record_turn_end(outcome, &stop_reason, duration_ms, changes);
        if let Err(e) = self.set_state(SessionState::Idle) {
            self.store_failed(&e);
        }
        let head: String = self.turn_assistant_text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect();
        self.announce_turn_end(outcome, &stop_reason, &head);
        // Another agent's message started this turn: its answer goes back.
        if outcome == TurnOutcome::Finished {
            self.return_answer();
        }
        // A finished turn is a result to look at (done); a cancelled one is at
        // rest (idle); a failed one stopped (error).
        let (osc_state, osc_msg) = match outcome {
            TurnOutcome::Failed => (axel_rex::osc::State::Error, t!("the turn failed").to_string()),
            TurnOutcome::Cancelled => (axel_rex::osc::State::Idle, String::new()),
            TurnOutcome::Finished | TurnOutcome::StoppedShort => (axel_rex::osc::State::Done, head),
        };
        self.program_status(osc_state, None, &osc_msg);
        let seconds = format!("{:.1}", duration_ms as f64 / 1000.0);
        let (tools, turn) = (self.turn_tool_calls, self.turn);
        match (outcome, &res) {
            (_, Err(e)) => self.line(RED, "✖", &t!("turn {turn} failed: {error}", turn = turn, error = e)),
            (TurnOutcome::Cancelled, Ok(_)) => {
                let line = t!("turn {turn} cancelled · {tools} tools · {seconds}s{changes}", turn = turn, tools = tools, seconds = seconds, changes = changed);
                self.line(YELLOW, "■", &line);
            }
            (TurnOutcome::StoppedShort, Ok(_)) => {
                let line = t!(
                    "turn {turn} done · {tools} tools · {seconds}s{changes} · {stop_reason}",
                    turn = turn, tools = tools, seconds = seconds, changes = changed, stop_reason = stop_reason,
                );
                self.line(GREEN, "✓", &line);
            }
            (TurnOutcome::Finished, Ok(_)) => {
                let line = t!("turn {turn} done · {tools} tools · {seconds}s{changes}", turn = turn, tools = tools, seconds = seconds, changes = changed);
                self.line(GREEN, "✓", &line);
            }
            (TurnOutcome::Failed, Ok(_)) => unreachable!("a turn fails only when its prompt request does"),
        }
    }

    /// What a turn leaves open when it ends: its streamed text, pending
    /// permissions, tool calls without a result, and prompts sent into it
    /// and never taken in (they wait for the next turn).
    fn close_turn_items(&mut self) {
        self.finalize_text();
        self.cancel_pending("turn ended");
        self.close_open_tool_calls();
        let unread: Vec<String> = self.steering.iter().map(|q| q.item_id.clone()).collect();
        for id in unread.iter().rev() {
            self.unsteer_to_queue(id, true);
        }
        debug_assert!(self.steering.is_empty() && self.pending.is_empty() && self.open_text.is_none());
    }

    /// The turn's `TurnEnd` item on the thread and its signed `turn` record.
    fn record_turn_end(&mut self, outcome: TurnOutcome, stop_reason: &str, duration_ms: i64, changes: Option<TurnChanges>) {
        let mut turn_end = json!({
            "stop_reason": stop_reason,
            "tool_calls": self.turn_tool_calls,
            "files_edited": self.turn_files_edited,
            "duration_ms": duration_ms,
        });
        if let Some(c) = &changes {
            turn_end["changes"] = serde_json::to_value(c).unwrap_or(Value::Null);
        }
        if let Err(e) = self.push(ItemKind::TurnEnd, Some(outcome.item_status()), None, turn_end) {
            self.store_failed(&e);
        }
        let summary = TurnSummary {
            ordinal: self.turn,
            user_text: self.turn_user_text.clone(),
            assistant_text: self.turn_assistant_text.chars().take(ASSISTANT_HEAD_CHARS).collect(),
            tool_calls: self.turn_tool_calls,
            files_edited: self.turn_files_edited,
            duration_ms,
            stop_reason: stop_reason.to_string(),
            changes,
        };
        if let Err(e) = self.store.record_turn(&self.signer, &self.cfg.session_id, &summary) {
            self.line(RED, "✖", &format!("record_turn: {e}"));
        }
    }

    /// Tell the owner the turn ended: a desktop notification, and the
    /// session's status.  `head` is the first line of the agent's answer.
    fn announce_turn_end(&mut self, outcome: TurnOutcome, stop_reason: &str, head: &str) {
        let who = std::env::var("AXEL_AGENT_NAME").unwrap_or_else(|_| "agent".into());
        self.notify(&match outcome {
            TurnOutcome::Failed => t!("{who} hit an error", who = who),
            TurnOutcome::Cancelled => t!("{who} stopped", who = who),
            TurnOutcome::Finished | TurnOutcome::StoppedShort if head.is_empty() => t!("{who} finished", who = who),
            TurnOutcome::Finished | TurnOutcome::StoppedShort => t!("{who} finished: {answer}", who = who, answer = head),
        });
        // Finished: a result the owner hasn't seen (done, which Rex marks until
        // they look); stopped by them: at rest (idle); failed: error.
        let turn_state = match outcome {
            TurnOutcome::Failed => StatusState::Error,
            TurnOutcome::Cancelled => StatusState::Idle,
            TurnOutcome::Finished | TurnOutcome::StoppedShort => StatusState::Done,
        };
        if let Err(e) = self.store.record_status(
            &self.signer,
            &self.cfg.session_id,
            &StatusReport {
                state: turn_state,
                title: Some(format!("turn {} {}", self.turn, stop_reason)),
                msg: (turn_state == StatusState::Done && !head.is_empty()).then(|| head.to_string()),
                ..Default::default()
            },
        ) {
            self.line(RED, "✖", &t!("status: {error}", error = e));
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
            acp::SessionUpdate::ToolCall(tc) => self.on_tool_call(&tc),
            acp::SessionUpdate::ToolCallUpdate(upd) => self.on_tool_call_update(&upd),
            acp::SessionUpdate::CurrentModeUpdate(m) => {
                self.dim(&t!("· agent mode is now {mode}", mode = m.current_mode_id));
                Ok(())
            }
            acp::SessionUpdate::Plan(plan) => self.on_plan(&plan),
            acp::SessionUpdate::UsageUpdate(u) => self.on_usage_update(&u),
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

    /// A new tool call: a `ToolCall` item (or the one a permission request
    /// made for it first), counted in the turn, and a line in the pane.
    fn on_tool_call(&mut self, tc: &acp::ToolCall) -> Result<()> {
        self.finalize_text();
        let kind = ToolKind::parse(&enum_str(&tc.kind));
        let status = tool_status(tc.status);
        let raw_input = tc.raw_input.clone().unwrap_or(Value::Null);
        let summary = input_summary(&raw_input);
        self.turn_tool_calls += 1;
        if edits_files(kind) {
            self.turn_files_edited += 1;
            self.note_authored(&tc.locations, &raw_input);
        }
        let content = json!({
            "title": tc.title,
            "kind": kind.as_str(),
            "raw_input": truncate_json(&raw_input, RAW_LIMIT_BYTES),
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
        let shown_summary = if summary.is_empty() { String::new() } else { format!("  {DIM}{summary}{RESET}") };
        self.line(YELLOW, "⚙", &format!("{}{shown_summary}", tc.title));
        Ok(())
    }

    /// An update to a tool call: merged into its item (made now if the call
    /// was never seen), and its result shown when it has one.
    fn on_tool_call_update(&mut self, upd: &acp::ToolCallUpdate) -> Result<()> {
        let ref_id = upd.tool_call_id.to_string();
        let f = &upd.fields;
        let status = f.status.map(tool_status);
        let mut patch = self.tool_call_patch(&ref_id, f)?;
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

    /// The fields a tool call update changes, as an item content patch.
    /// Locations an edit names count as authored by the turn.
    fn tool_call_patch(&mut self, ref_id: &str, f: &acp::ToolCallUpdateFields) -> Result<serde_json::Map<String, Value>> {
        let mut patch = serde_json::Map::new();
        if let Some(t) = &f.title {
            patch.insert("title".into(), json!(t));
        }
        if let Some(k) = &f.kind {
            patch.insert("kind".into(), json!(ToolKind::parse(&enum_str(k)).as_str()));
        }
        if let Some(v) = &f.raw_input {
            patch.insert("raw_input".into(), truncate_json(v, RAW_LIMIT_BYTES));
        }
        if let Some(v) = &f.raw_output {
            patch.insert("raw_output".into(), truncate_json(v, RAW_LIMIT_BYTES));
        }
        if let Some(l) = &f.locations {
            patch.insert("locations".into(), locations_json(l));
            let edits = match &f.kind {
                Some(k) => edits_files(ToolKind::parse(&enum_str(k))),
                None => self
                    .store
                    .item_by_ref(&self.cfg.session_id, ref_id)?
                    .and_then(|it| it.content.get("kind").and_then(Value::as_str).map(|k| edits_files(ToolKind::parse(k))))
                    .unwrap_or(false),
            };
            if edits {
                self.note_authored(l, f.raw_input.as_ref().unwrap_or(&Value::Null));
            }
        }
        if let Some(c) = &f.content {
            let v = serde_json::to_value(c).unwrap_or(Value::Null);
            patch.insert("content".into(), truncate_json(&v, RAW_LIMIT_BYTES));
        }
        Ok(patch)
    }

    /// The agent's plan: one `Thought` item, kept up to date.
    fn on_plan(&mut self, plan: &acp::Plan) -> Result<()> {
        self.finalize_text();
        let text = plan
            .entries
            .iter()
            .map(|e| {
                let glyph = match e.status {
                    acp::PlanEntryStatus::Completed => "✓",
                    acp::PlanEntryStatus::InProgress => "▸",
                    _ => "·",
                };
                format!("{glyph} {}", e.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
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
        self.line(BLUE, "☰", &t!("plan · {count} steps", count = plan.entries.len()));
        Ok(())
    }

    /// How full the context is, and the session's running cost in USD,
    /// when the harness reports them in ACP's own terms.
    fn on_usage_update(&mut self, u: &acp::UsageUpdate) -> Result<()> {
        if u.size > 0 {
            self.store.set_session_context(&self.cfg.session_id, u.used, u.size)?;
        }
        if let Some(cost) = u.cost.as_ref().filter(|c| c.currency.eq_ignore_ascii_case("USD") && c.amount.is_finite() && c.amount >= 0.0) {
            // A running total: only what it grew by is new.
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

    /// Append streamed text to the open AssistantMessage/Thought item, opening one if needed.
    fn append_text(&mut self, kind: ItemKind, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(cur) = &self.open_text
            && cur.kind != kind
        {
            self.finalize_text();
        }
        if kind == ItemKind::AssistantMessage {
            self.turn_assistant_text.push_str(text);
        }
        match &mut self.open_text {
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
                self.open_text = Some(TextItem {
                    id: item.id,
                    kind,
                    text: text.to_string(),
                });
                if kind == ItemKind::Thought {
                    self.dim(t!("· thinking…"));
                }
            }
        }
        Ok(())
    }

    /// Close the open text item (marks it Completed and prints the prose head).
    fn finalize_text(&mut self) {
        if let Some(cur) = self.open_text.take() {
            if let Err(e) = self
                .store
                .update_item(&cur.id, Some(ItemStatus::Completed), None)
            {
                self.store_failed(&e);
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
        let call = self.permission_call(&req.tool_call)?;
        let options: Vec<(String, acp::PermissionOptionKind)> = req
            .options
            .iter()
            .map(|o| (o.option_id.to_string(), o.kind))
            .collect();
        // A question for the owner (its choices are `answer:` options) is never
        // a permission: no policy may pick an answer, whatever the mode.
        let question = !req.options.is_empty() && req.options.iter().all(|o| o.option_id.to_string().starts_with(crate::claude::ANSWER_PREFIX));
        if question {
            let multiple = selection_bounds(&call.raw_input, &options)?;
            return self.ask_owner(call, &req.options, options, multiple, true, reply);
        }
        match policy::decide(self.cfg.runtime_mode, call.kind, &call.title, &call.raw_input, &self.cfg.disallowed_tools) {
            Decision::Allow => {
                let picked = pick_option(&options, acp::PermissionOptionKind::AllowOnce, is_allow);
                // The adapter may have withdrawn the request; nothing to undo.
                let _ = reply.send(selected_or_cancelled(picked));
                self.line(GREEN, "✓", &t!("allow: {tool}", tool = call.label()));
            }
            Decision::Deny { reason } => {
                let picked = pick_option(&options, acp::PermissionOptionKind::RejectOnce, is_reject);
                // The adapter may have withdrawn the request; nothing to undo.
                let _ = reply.send(selected_or_cancelled(picked));
                self.push(
                    ItemKind::Permission,
                    Some(ItemStatus::Denied),
                    Some(call.permission_ref()),
                    json!({ "title": call.title, "kind": call.kind.as_str(), "raw_input": truncate_json(&call.raw_input, RAW_LIMIT_BYTES), "reason": reason, "decided_by": "policy" }),
                )?;
                let reason = format!("{DIM}({reason}){RESET}");
                self.line(RED, "✗", &t!("deny: {tool} {reason}", tool = call.label(), reason = reason));
            }
            Decision::Ask => self.ask_owner(call, &req.options, options, None, false, reply)?,
        }
        Ok(())
    }

    /// What a permission request is about: its tool call's title, kind and
    /// input, from the request, else from the tool call item already stored.
    fn permission_call(&self, tool_call: &acp::ToolCallUpdate) -> Result<PermissionCall> {
        let ref_id = tool_call.tool_call_id.to_string();
        let f = &tool_call.fields;
        let existing = self.store.item_by_ref(&self.cfg.session_id, &ref_id)?;
        let title = f
            .title
            .clone()
            .or_else(|| existing.as_ref().and_then(item_title))
            .unwrap_or_else(|| ref_id.clone());
        let kind = f
            .kind
            .map(|k| ToolKind::parse(&enum_str(&k)))
            .or_else(|| existing.as_ref().and_then(|i| i.content.get("kind").and_then(Value::as_str).map(ToolKind::parse)))
            .unwrap_or_default();
        let raw_input = f
            .raw_input
            .clone()
            .or_else(|| existing.as_ref().and_then(|i| i.content.get("raw_input").cloned()))
            .unwrap_or(Value::Null);
        Ok(PermissionCall { ref_id, title, kind, raw_input })
    }

    /// Park a permission request (or a question) for the owner: a pending
    /// item on the thread, the session blocked, and a notification.
    fn ask_owner(
        &mut self,
        call: PermissionCall,
        offered: &[acp::PermissionOption],
        options: Vec<(String, acp::PermissionOptionKind)>,
        multiple: Option<(usize, usize)>,
        question: bool,
        reply: oneshot::Sender<acp::RequestPermissionResponse>,
    ) -> Result<()> {
        debug_assert!(question || multiple.is_none(), "only a question has several selections");
        let opts: Vec<Value> = offered
            .iter()
            .map(|o| json!({ "option_id": o.option_id.to_string(), "name": o.name, "kind": enum_str(&o.kind) }))
            .collect();
        let item = self.push(
            if question { ItemKind::Question } else { ItemKind::Permission },
            Some(ItemStatus::Pending),
            Some(call.permission_ref()),
            json!({ "title": call.title, "kind": call.kind.as_str(), "raw_input": truncate_json(&call.raw_input, RAW_LIMIT_BYTES), "options": opts, "multi_select": multiple.map(|(min,max)|json!({"min":min,"max":max})) }),
        )?;
        let who = std::env::var("AXEL_AGENT_NAME").unwrap_or_else(|_| "agent".into());
        let title = &call.title;
        self.notify(&if question { t!("{who} asks: {title}", who = who, title = title) } else { t!("{who} needs permission: {title}", who = who, title = title) });
        self.set_state(SessionState::Blocked)?;
        let (osc_kind, blocked_kind) = if question {
            (axel_rex::osc::Kind::Question, BlockedKind::Question)
        } else {
            (axel_rex::osc::Kind::Permission, BlockedKind::Permission)
        };
        self.program_status(axel_rex::osc::State::Blocked, Some(osc_kind), title);
        let summary = input_summary(&call.raw_input);
        self.store.record_status(
            &self.signer,
            &self.cfg.session_id,
            &StatusReport {
                state: StatusState::Blocked,
                kind: Some(blocked_kind),
                title: Some(title.clone()),
                msg: (!summary.is_empty()).then_some(summary),
                progress: None,
            },
        )?;
        let label = call.label();
        self.pending.insert(item.id, PendingPermission { reply, title: call.title, options, multiple });
        self.line(MAGENTA, "?", &t!("permission: {tool}", tool = label));
        Ok(())
    }

    fn answer(&mut self, item_id: &str, option_id: &str) -> Result<()> {
        let Some(p) = self.pending.get(item_id) else {
            anyhow::bail!("no pending permission {item_id}");
        };
        let allowed = validate_answer(&p.options,p.multiple,option_id)?;
        let p = self.pending.remove(item_id).expect("checked above");
        // The adapter may have withdrawn the request; the item still records the answer.
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
        if allowed {
            self.line(GREEN, "✓", &t!("allowed: {tool}", tool = p.title));
        } else {
            self.line(RED, "✗", &t!("denied: {tool}", tool = p.title));
        }
        Ok(())
    }

    /// Reject every pending permission (dropping the reply answers `cancelled`).
    /// Tool calls of this turn that never got a result (the turn was cancelled
    /// mid-call, or the harness never reported one) can't finish any more:
    /// close them, or the thread would show them running for ever.
    fn close_open_tool_calls(&mut self) {
        let open = match self.store.open_tool_calls(&self.cfg.session_id, self.turn) {
            Ok(open) => open,
            Err(e) => return self.store_failed(&e),
        };
        for item in open {
            if let Err(e) = self.store.update_item(
                &item.id,
                Some(ItemStatus::Cancelled),
                Some(json!({ "reason": "the turn ended before it finished" })),
            ) {
                self.store_failed(&e);
            }
        }
    }

    fn cancel_pending(&mut self, why: &str) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for (item_id, p) in pending {
            // The adapter may have withdrawn the request already.
            let _ = p.reply.send(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ));
            if let Err(e) = self.store.update_item(
                &item_id,
                Some(ItemStatus::Cancelled),
                Some(json!({ "reason": why })),
            ) {
                self.store_failed(&e);
            }
            // `why` is stored as it is (an end reason, in English); the pane frames it.
            let reason = format!("{DIM}({why}){RESET}");
            self.line(YELLOW, "■", &t!("permission cancelled: {tool} {reason}", tool = p.title, reason = reason));
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
                self.line(YELLOW, "!", &t!("turn {turn} checkpoint skipped: {error}", turn = self.turn, error = format!("{e:#}")));
                return None;
            }
        };
        if started.elapsed() > checkpoint::SLOW && !self.checkpoint_slow_told {
            self.checkpoint_slow_told = true;
            let seconds = format!("{:.1}", started.elapsed().as_secs_f64());
            self.line(YELLOW, "!", &t!("checkpoint took {seconds}s: a large file that isn't ignored? add it to .gitignore", seconds = seconds));
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
                self.line(YELLOW, "!", &t!("turn {turn} changes unknown: {error}", turn = self.turn, error = format!("{e:#}")));
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
                self.store_failed(&e);
            }
        }
        if let Err(e) = self.store.set_session_socket(&self.cfg.session_id, None) {
            self.store_failed(&e);
        }
        if let Err(e) = self
            .store
            .end_session(&self.signer, &self.cfg.session_id, Some(reason))
        {
            self.line(RED, "✖", &format!("end_session: {e}"));
        }
        self.state = SessionState::Ended;
        self.line(CYAN, "○", &t!("session ended: {reason}", reason = reason));
    }
}

// --- socket server -------------------------------------------------------------

/// After a failed `accept` (out of file descriptors, say), wait this long
/// before the next: the retries go on until the host aborts the task, at a
/// rate that can't spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The longest command line a client may send.  Commands carry a prompt's
/// text and attachment paths, never file contents: 16 MiB is far more than
/// anyone types or pastes, and one line can't take all memory.
const COMMAND_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Connections served at once.  Clients (`axel send`, the thread view) send
/// one command per connection and hang up; more than this at once is a
/// runaway script, which then waits for a turn instead of piling up tasks.
const CONNECTIONS_MAX: usize = 64;

async fn serve_socket(listener: UnixListener, tx: mpsc::UnboundedSender<Event>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(CONNECTIONS_MAX));
    loop {
        let slot = Arc::clone(&slots).acquire_owned().await.expect("the semaphore is never closed");
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            serve_connection(stream, tx).await;
            drop(slot);
        });
    }
}

/// One client: a reply line for every command line, until it hangs up.
async fn serve_connection(stream: tokio::net::UnixStream, tx: mpsc::UnboundedSender<Event>) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    while let Ok(Some(read)) = transport::read_line_bounded(&mut reader, COMMAND_LIMIT_BYTES).await {
        let reply = match read {
            LineRead::Line(bytes) => {
                let line = String::from_utf8_lossy(&bytes);
                if line.trim().is_empty() {
                    continue;
                }
                command_reply(&line, &tx).await
            }
            LineRead::TooLong { length_bytes } => Reply::Error {
                message: format!("command too long: {length_bytes} bytes, over the {COMMAND_LIMIT_BYTES}-byte limit"),
            },
        };
        let mut s = serde_json::to_string(&reply).expect("a Reply has only strings and numbers: it always serializes");
        s.push('\n');
        if w.write_all(s.as_bytes()).await.is_err() {
            break;
        }
    }
}

/// Hand one command line to the host's loop and wait for its reply.
async fn command_reply(line: &str, tx: &mpsc::UnboundedSender<Event>) -> Reply {
    let cmd = match serde_json::from_str::<Command>(line) {
        Ok(cmd) => cmd,
        Err(e) => return Reply::Error { message: format!("bad command: {e}") },
    };
    let (rtx, rrx) = oneshot::channel();
    if tx.send(Event::Command { cmd, reply: rtx }).is_err() {
        return Reply::Error { message: "host is shutting down".into() };
    }
    rrx.await.unwrap_or(Reply::Error { message: "host dropped the command".into() })
}

// --- fs client methods ---------------------------------------------------------

fn resolve(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// The largest file an agent may read through `fs/read_text_file`.  Source
/// files are kilobytes and logs megabytes; past 64 MiB it isn't text an
/// agent can read in one go, and reading it would hold it all in memory.
const FS_READ_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

async fn fs_read(
    cwd: &Path,
    req: acp::ReadTextFileRequest,
    responder: Responder<acp::ReadTextFileResponse>,
) -> Result<(), acp::Error> {
    let path = resolve(cwd, &req.path);
    // Refused before it is read: the whole file is held to slice its lines.
    if let Ok(meta) = tokio::fs::metadata(&path).await
        && meta.len() > FS_READ_LIMIT_BYTES
    {
        return responder.respond_with_error(acp::Error::invalid_params().data(json!({
            "path": path.display().to_string(),
            "error": format!("file is {} bytes, over the {FS_READ_LIMIT_BYTES}-byte limit for a text read", meta.len()),
        })));
    }
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
    let as_count = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
    let skip = line.map_or(0, |l| as_count(l.saturating_sub(1)));
    let take = limit.map_or(usize::MAX, as_count);
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

fn tool_status(status: acp::ToolCallStatus) -> ItemStatus {
    match status {
        acp::ToolCallStatus::Completed => ItemStatus::Completed,
        acp::ToolCallStatus::Failed => ItemStatus::Failed,
        // Pending, in progress, and kinds a newer protocol adds.
        _ => ItemStatus::Running,
    }
}

/// Kinds of tool that change files: their calls count as the turn's edits.
fn edits_files(kind: ToolKind) -> bool {
    matches!(kind, ToolKind::Edit | ToolKind::Delete | ToolKind::Move)
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

/// `v`, cut to about `limit_bytes`: kept whole when its serialization fits.
/// Text stays text: a string, or text blocks (`[{type: text, text}]`), is
/// cut as text and stays readable; only other JSON is cut as its
/// serialization (stored as a string).
fn truncate_json(v: &Value, limit_bytes: usize) -> Value {
    if v.to_string().len() <= limit_bytes {
        return v.clone();
    }
    let cut = |s: &str| {
        let mut end = limit_bytes.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let out = format!("{}…", &s[..end]);
        debug_assert!(out.len() <= limit_bytes + '…'.len_utf8());
        out
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

/// First line of `s`, at most `max_chars` chars, with an ellipsis when
/// anything was left out.
fn head(s: &str, max_chars: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(max_chars).collect();
    if first.chars().count() > max_chars || s.lines().count() > 1 {
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

/// What the thread says when the agent couldn't start.  ACP's "auth
/// required" (a harness never logged in) gets the way to log it in; anything
/// else its first line, the full error staying in the session log.
fn setup_failure_message(harness: &Harness, err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("authentication required") || lower.contains("auth_required") || lower.contains("not logged in") {
        // `login` is the store's English instruction (a command to run).
        return t!(
            "{harness} isn't logged in, so the agent couldn't start. To log in, {login}; then send your message again.",
            harness = harness.label(), login = harness.login(),
        );
    }
    let first = err.lines().next().unwrap_or(err).trim();
    let first: String = if first.chars().count() > 300 { format!("{}…", first.chars().take(300).collect::<String>()) } else { first.to_string() };
    t!("{harness} couldn't start: {reason}", harness = harness.label(), reason = first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_answers_are_bounded_unique_and_never_permission_grants() {
        let options=vec![("answer:0".into(),acp::PermissionOptionKind::AllowOnce),("answer:1".into(),acp::PermissionOptionKind::AllowOnce),("answer:none".into(),acp::PermissionOptionKind::RejectOnce)];
        let both=r#"answer:multi:["answer:0","answer:1"]"#;
        assert!(validate_answer(&options,Some((1,2)),both).unwrap());
        assert!(validate_answer(&options,None,both).is_err());
        assert!(validate_answer(&options,Some((1,1)),both).is_err());
        assert!(validate_answer(&options,Some((2,2)),"answer:0").is_err());
        for invalid in [r#"answer:multi:["answer:0","answer:0"]"#,r#"answer:multi:["answer:none"]"#,r#"answer:multi:["answer:99"]"#,"answer:multi:[]"] {
            assert!(validate_answer(&options,Some((1,2)),invalid).is_err(),"{invalid}");
        }
        assert!(validate_answer(&options,Some((0,2)),"answer:multi:[]").unwrap());
        assert!(!validate_answer(&options,Some((1,2)),"answer:none").unwrap());
    }

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
    fn axels_own_adapters_get_the_effort_in_the_session_meta() {
        use axel_store::Effort;
        let base = json!({ "systemPrompt": { "append": "You are atlas." }, "claudeCode": { "options": {} } }).as_object().cloned();
        let (meta, warning) = meta_with_effort(&Harness::Claude, true, base.clone(), Some(Effort::Max));
        let meta = Value::Object(meta.unwrap());
        assert_eq!(meta["axel"]["effort"], "max");
        assert_eq!(meta["systemPrompt"]["append"], "You are atlas.", "the rest of the meta stays");
        assert_eq!(warning, None);
        let (meta, _) = meta_with_effort(&Harness::Codex, true, None, Some(Effort::Xhigh));
        assert_eq!(Value::Object(meta.unwrap()), json!({ "axel": { "effort": "xhigh" } }));

        // Codex stops at xhigh: nothing is sent, and the owner is told.
        let (meta, warning) = meta_with_effort(&Harness::Codex, true, base.clone(), Some(Effort::Max));
        assert_eq!(meta, base);
        assert!(warning.unwrap().contains("could not set effort max: Codex has no such level"));

        // No effort, or a harness asked through its config options: the meta is untouched.
        assert_eq!(meta_with_effort(&Harness::Claude, true, base.clone(), None), (base.clone(), None));
        assert_eq!(meta_with_effort(&Harness::Claude, true, None, None), (None, None));
        assert_eq!(meta_with_effort(&Harness::Goose, false, None, Some(Effort::High)), (None, None));
        // So is that of a Claude or Codex put behind another adapter, even at a level it has no word for.
        assert_eq!(meta_with_effort(&Harness::Claude, false, base.clone(), Some(Effort::High)), (base.clone(), None));
        assert_eq!(meta_with_effort(&Harness::Codex, false, None, Some(Effort::Max)), (None, None));
    }

    #[test]
    fn the_effort_selector_is_found_by_category_then_by_what_it_is_called() {
        let model = json!({ "id": "model", "name": "Model (reasoning models first)", "category": "model", "type": "select" });
        let toggle = json!({ "id": "thinking", "name": "Thinking", "type": "boolean" });
        let named = json!({ "id": "depth", "name": "Reasoning Effort", "type": "select" });
        let by_category = json!({ "id": "budget", "name": "Budget", "category": "thought_level", "type": "select" });
        assert_eq!(effort_selector(&[model.clone(), toggle.clone(), named.clone(), by_category.clone()]).unwrap()["id"], "budget");
        assert_eq!(effort_selector(&[model.clone(), toggle.clone(), named]).unwrap()["id"], "depth");
        assert!(effort_selector(&[model.clone(), toggle.clone()]).is_none(), "a model picker or a switch is not an effort selector");
        // Something else about the reasoning, listed first, is not the effort selector.
        let levels = json!([{ "value": "low", "name": "Low" }, { "value": "high", "name": "High" }]);
        let summary = json!({ "id": "reasoning_summary", "name": "Reasoning Summary", "type": "select", "options": [{ "value": "auto", "name": "Auto" }, { "value": "none", "name": "None" }] });
        let display = json!({ "id": "thinking_display", "name": "Thinking", "type": "select", "options": [{ "value": "shown", "name": "Shown" }] });
        let effort = json!({ "id": "reasoning_effort", "name": "Reasoning Effort", "type": "select", "options": levels });
        assert_eq!(effort_selector(&[summary.clone(), display.clone(), effort]).unwrap()["id"], "reasoning_effort");
        assert!(effort_selector(&[model, toggle, summary.clone()]).is_none(), "a summary selector alone is no effort option");
        // With no levels anywhere, the first plausible one is kept, so the warning can list what it offers.
        assert_eq!(effort_selector(&[summary, display]).unwrap()["id"], "thinking_display");
        assert!(same_level("X-High", axel_store::Effort::Xhigh) && !same_level("highest", axel_store::Effort::High));
    }

    #[test]
    fn a_selects_values_are_read_flat_or_one_group_down_and_no_deeper() {
        // Far deeper than a stack could follow: `max` under 100,000 groups.
        let mut deep = json!({ "value": "max", "name": "Max" });
        for _ in 0..100_000 {
            deep = Value::Object(serde_json::Map::from_iter([("options".to_string(), Value::Array(vec![deep]))]));
        }
        let options = vec![json!({ "value": "low", "name": "Low" }), json!({ "group": "more", "name": "More", "options": [{ "value": "high" }] }), deep];
        let pair = |value: &str, name: &str| (value.to_string(), name.to_string());
        assert_eq!(select_values(&options), [pair("low", "Low"), pair("high", "high")]);
        // Dropping a value recurses as deep as it nests, so this one is not dropped.
        std::mem::forget(options);
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

    #[test]
    fn a_harness_that_is_not_logged_in_says_how_to_log_in() {
        let err = "session/new: Authentication required: Configure an API key or log in with an OAuth provider.: {\n  \"authMethods\": []\n}";
        let msg = super::setup_failure_message(&Harness::Pi, err);
        assert!(msg.starts_with("Pi isn't logged in"), "{msg}");
        assert!(msg.contains("pi auth login"), "{msg}");
        let other = super::setup_failure_message(&Harness::Goose, "initialize: connection closed\nmore detail");
        assert_eq!(other, "Goose couldn't start: initialize: connection closed");
    }
}

