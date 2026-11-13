//! Pure data rows the screens render. Only the store's closed-set enums are
//! used here (no store access); `glue.rs` converts store records into these.

use std::path::PathBuf;

pub use axel_store::{AgentColor, Harness, RuntimeMode, StatusState};

/// A project the "Where?" step can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    pub path: PathBuf,
}

/// An agent the "Who?" step can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub pubkey: String,
    pub name: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub color: Option<AgentColor>,
    pub runtime_mode: RuntimeMode,
    pub last_seen_at: Option<i64>,
    pub unread: u64,
}

/// What the launcher asks the glue to create when the new-agent form is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAgentSpec {
    pub name: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub color: Option<AgentColor>,
    pub runtime_mode: RuntimeMode,
}

/// Kind of a pending request in the inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Permission,
    Question,
}

impl RequestKind {
    pub fn label(self) -> &'static str {
        match self {
            RequestKind::Permission => "permission",
            RequestKind::Question => "question",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub id: String,
    pub from_pubkey: String,
    pub agent_name: String,
    pub kind: RequestKind,
    pub content: String,
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    pub thread_title: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusRow {
    pub session_id: String,
    pub thread_id: String,
    pub state: StatusState,
    pub agent_name: String,
    pub thread_title: String,
    pub msg: Option<String>,
    pub updated_at: i64,
}

impl StatusRow {
    pub fn glyph(&self) -> &'static str {
        state_glyph(self.state)
    }
}

/// Glyph for a status state.
pub fn state_glyph(state: StatusState) -> &'static str {
    match state {
        StatusState::Working => "●",
        StatusState::Blocked => "◐",
        StatusState::Done => "✓",
        StatusState::Error => "!",
        StatusState::Idle => "○",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRow {
    pub id: String,
    pub from_pubkey: String,
    pub from_name: String,
    pub kind: u32,
    pub kind_label: String,
    pub content: String,
    pub thread_id: Option<String>,
    pub created_at: u64,
}

/// Everything the inbox screen shows, refreshed periodically by the glue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxData {
    pub requests: Vec<RequestRow>,
    pub statuses: Vec<StatusRow>,
    pub messages: Vec<MessageRow>,
    pub unread: u64,
}

/// One row of the dashboard's threads table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: String,
    pub title: String,
    /// Live state derived from the thread's sessions.
    pub state: StatusState,
    /// `(display name, colour)` of agents on the thread; unknown pubkeys get a dim prefix.
    pub agents: Vec<(String, Option<AgentColor>)>,
    /// Project names (file name of `path_hint`); empty = scratch.
    pub projects: Vec<String>,
    pub environments: Vec<String>,
    pub sessions_total: i64,
    pub sessions_live: usize,
    /// input + output tokens across all sessions.
    pub tokens: i64,
    pub spark: Vec<u64>,
    pub updated_at: i64,
    /// Most urgent live session (blocked first, else most recent); what Enter focuses.
    pub focus_session: Option<String>,
    /// Settled (state done): finished but still listed.
    pub settled: bool,
}

/// One row of the dashboard's agents table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DashAgentRow {
    pub pubkey: String,
    pub name: String,
    pub color: Option<AgentColor>,
    pub harness: Harness,
    pub runtime_mode: RuntimeMode,
    /// From the latest session's status.
    pub state: StatusState,
    /// Thread of the agent's latest session (what Enter opens).
    pub thread_id: Option<String>,
    /// Title of that thread.
    pub thread_title: Option<String>,
    pub environment: String,
    /// input + output tokens across the agent's sessions.
    pub tokens: i64,
    pub spark: Vec<u64>,
    pub last_event_at: Option<i64>,
    /// Latest session that has not ended (what Enter focuses).
    pub active_session_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokensPanel {
    /// Per-minute buckets over the last hour, oldest first.
    pub series: Vec<u64>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
}

/// Everything the dashboard shows, refreshed periodically by the glue.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DashboardData {
    pub threads: Vec<ThreadRow>,
    pub agents: Vec<DashAgentRow>,
    pub tokens: TokensPanel,
    pub requests: Vec<RequestRow>,
    pub messages: Vec<MessageRow>,
    pub unread: u64,
}

// ---------------------------------------------------------------- thread view

/// Mirror of `axel_store::ItemKind` so the pure modules compile without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Thought,
    ToolCall,
    Permission,
    Question,
    Answer,
    Error,
    TurnEnd,
}

/// Mirror of `axel_store::ItemStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Allowed,
    Denied,
    Answered,
}

impl ItemStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            ItemStatus::Pending => "○",
            ItemStatus::Running => "●",
            ItemStatus::Completed | ItemStatus::Allowed | ItemStatus::Answered => "✓",
            ItemStatus::Failed | ItemStatus::Denied => "!",
            ItemStatus::Cancelled => "×",
        }
    }
}

/// An option of a pending permission/question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
    /// Declining this question is exclusive of other selections.
    pub exclusive: bool,
}

/// Decoded body of a conversation item (the glue decodes the store's JSON).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemBody {
    /// User / assistant / thought / error text.
    Text(String),
    Tool {
        title: String,
        /// ACP tool kind (`read`, `edit`, `execute`, …) when known.
        kind: Option<String>,
        /// One-line summary of the raw input.
        input: String,
        /// Output lines (head only is rendered).
        output: Vec<String>,
        /// What it does, in words ([`crate::tools`]): "Read lines 1–140 of send.rs".
        summary: String,
        /// The same, as actions, for merging a run of calls into one line.
        actions: Vec<crate::tools::Action>,
    },
    Choice {
        /// Question text, or the tool title asking for permission.
        prompt: String,
        options: Vec<ChoiceOption>,
        /// Minimum and maximum checks, or a single-choice question.
        multiple: Option<(usize, usize)>,
        /// Label of the chosen option once answered.
        chosen: Option<String>,
    },
    /// `Answer`: label of the option picked.
    Answered(String),
    /// `TurnEnd` of a turn that changed files (from its checkpoints): the
    /// change line under the turn.
    Changes {
        files: usize,
        /// Files this session's own edit tools touched; the rest changed
        /// during the turn by other means (a command, another agent).
        authored: usize,
        added: i64,
        removed: i64,
        /// Includes edits made between this turn and the previous one.
        between_turns: bool,
    },
    Empty,
}

/// One timeline item (a `ConversationItem`, or a turn summary from a remote session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadItem {
    pub id: String,
    pub session_id: String,
    /// Agent that produced it (name, colour); `None` for the owner.
    pub agent: Option<(String, Option<AgentColor>)>,
    /// Display name for user messages from someone else (remote participants).
    pub author: Option<String>,
    pub seq: i64,
    pub turn: i64,
    /// Unix milliseconds.
    pub at_ms: i64,
    pub kind: ItemKind,
    pub status: Option<ItemStatus>,
    pub body: ItemBody,
    /// Files attached to a user message (`content.attachments`).
    pub attachments: Vec<PathBuf>,
    /// For a message one agent sent another: the recipient's name.
    pub relay_to: Option<String>,
    /// A prompt sent into the running turn that the agent hasn't read yet
    /// (pending, `content.steering`); a queued one waits for the turn to end.
    pub steering: bool,
}

/// A session of the thread that has not ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSession {
    pub id: String,
    pub agent_name: String,
    pub managed: bool,
    pub started_at: i64,
    /// The session's runtime mode (Shift+Tab cycles it on managed sessions).
    pub mode: RuntimeMode,
    /// The remote host it runs on; `None` when it runs on this machine.
    pub host: Option<String>,
    /// When its agent last replied on the thread (Unix ms).
    pub last_reply_at: Option<i64>,
}

/// Everything the thread view shows, refreshed by the glue.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadData {
    pub thread_id: String,
    pub title: String,
    /// (name, colour, live state) per agent on the thread.
    pub agents: Vec<(String, Option<AgentColor>, StatusState)>,
    pub environments: Vec<String>,
    pub tokens: i64,
    pub cost_usd: f64,
    pub items: Vec<ThreadItem>,
    /// Newest first.
    pub live_sessions: Vec<LiveSession>,
    /// Name of the agent of the most recent session (live or not), for summon fallback.
    pub last_agent_name: Option<String>,
    /// Every local agent (name, colour): what an `@name` in the composer can summon.
    pub known_agents: Vec<(String, Option<AgentColor>)>,
}

/// What the usage pane shows: spend per agent and per thread, over the last
/// hour, the last 24 hours and all time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageData {
    pub agents: Vec<AgentSpend>,
    /// Threads that spent in the last 24 hours, most recent first.
    pub threads: Vec<ThreadSpend>,
    pub last_hour: axel_store::UsageTotals,
    pub last_day: axel_store::UsageTotals,
    pub total: axel_store::UsageTotals,
    /// Each harness's plan windows, by harness.
    pub limits: Vec<PlanRow>,
}

/// One harness's plan windows.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanRow {
    pub harness: String,
    pub windows: Vec<LimitRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LimitRow {
    pub window: axel_store::LimitWindow,
    /// 0–100; `None` once the window has started over since the report.
    pub used_pct: Option<f64>,
    /// Seconds until it starts over.
    pub resets_in: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentSpend {
    pub name: String,
    pub color: Option<AgentColor>,
    pub last_hour: axel_store::UsageTotals,
    pub last_day: axel_store::UsageTotals,
    pub total: axel_store::UsageTotals,
    /// Tokens per minute over the last hour, oldest first.
    pub spark: Vec<i64>,
    /// The fullest context window among its live sessions: `(used, size)`.
    pub context: Option<(i64, i64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadSpend {
    pub title: String,
    pub last_day: axel_store::UsageTotals,
    pub total: axel_store::UsageTotals,
}
