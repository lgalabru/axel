//! Row and value types shared across crate boundaries (see docs/CONTRACTS.md).
//!
//! Pubkeys are base58 strings; timestamps are Unix seconds.

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($(#[$vmeta])* $variant),+ }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The snake_case name stored in the database and in envelopes.
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                    $($s => Ok($name::$variant),)+
                    other => Err(Error::Corrupt(format!(concat!("unknown ", stringify!($name), " {:?}"), other))),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.pad(self.as_str())
            }
        }
    };
}

/// Lenient serde helpers for enum fields read back from the log: text this
/// build does not know maps to the enum's default (or `None`) instead of
/// rejecting the whole envelope.
pub(crate) mod lenient {
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer};

    pub fn or_default<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr + Default,
    {
        let s = String::deserialize(d)?;
        Ok(s.parse().unwrap_or_default())
    }

    pub fn or_none<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
    {
        let s = Option::<String>::deserialize(d)?;
        Ok(s.and_then(|s| s.parse().ok()))
    }
}

str_enum! {
    /// Coarse thread state, set by a human or by policy.  Live state is derived
    /// from the thread's sessions instead.  `Done` is "settled": finished but
    /// still listed.  `Deleted` is a tombstone: the log is append-only, so a
    /// deleted thread stays in it but is hidden everywhere.
    ThreadState { Open => "open", Done => "done", Archived => "archived", Deleted => "deleted" }
}

str_enum! {
    /// Lifecycle of one agent instance on a thread.
    SessionState { Starting => "starting", Running => "running", Blocked => "blocked", Idle => "idle", Paused => "paused", Ended => "ended" }
}

str_enum! {
    /// How a thread works on a project: in its directory, in a dedicated
    /// worktree, or in a scratch directory with no project at all.
    WorkspaceKind { InPlace => "in_place", Worktree => "worktree", Scratch => "scratch" }
}

str_enum! {
    /// What a participant may do on a thread.  Owners add participants.
    ParticipantRole { Owner => "owner", Member => "member", Agent => "agent" }
}

str_enum! {
    /// How a session is run: the native CLI in a Rex pane, or driven by Axel
    /// over ACP with the conversation owned by Axel.
    #[derive(Default)]
    SessionRuntime { #[default] Pane => "pane", Managed => "managed" }
}

str_enum! {
    /// What a conversation item is.
    ItemKind {
        UserMessage => "user_message",
        AssistantMessage => "assistant_message",
        Thought => "thought",
        ToolCall => "tool_call",
        Permission => "permission",
        Question => "question",
        Answer => "answer",
        Error => "error",
        TurnEnd => "turn_end",
    }
}

str_enum! {
    /// Where a conversation item stands.
    ItemStatus {
        Pending => "pending",
        Running => "running",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
        Allowed => "allowed",
        Denied => "denied",
        Answered => "answered",
    }
}

str_enum! {
    /// How much an agent may do unattended.  `autopilot` (the default) never
    /// asks; `edits` auto-accepts file edits; `supervised` asks for everything;
    /// `plan` is read-only.
    #[derive(Default)]
    RuntimeMode { #[default] Autopilot => "autopilot", Edits => "edits", Supervised => "supervised", Plan => "plan" }
}

str_enum! {
    /// What a session says it is doing (the OSC 7501 states minus `clear`).
    #[derive(Default)]
    StatusState { #[default] Idle => "idle", Working => "working", Done => "done", Blocked => "blocked", Error => "error" }
}

impl StatusState {
    /// Rank for "most urgent": blocked > error > working > done > idle.
    pub fn urgency(self) -> u8 {
        match self {
            StatusState::Blocked => 5,
            StatusState::Error => 4,
            StatusState::Working => 3,
            StatusState::Done => 2,
            StatusState::Idle => 1,
        }
    }
}

str_enum! {
    /// What a `blocked` session is waiting for.
    BlockedKind { Permission => "permission", Question => "question", Auth => "auth" }
}

str_enum! {
    /// How a child thread relates to its parent.
    Relationship { Fork => "fork", Subagent => "subagent", Delegation => "delegation" }
}

str_enum! {
    /// The agent colours the UI knows how to draw, in the new-agent form's cycle order.
    AgentColor { Purple => "purple", Yellow => "yellow", Red => "red", Green => "green", Blue => "blue", Gray => "gray", Orange => "orange" }
}

/// The CLI an agent runs in: one Axel knows how to drive, or any other command.
///
/// Stored and serialised as plain text: the builtin name, or the custom
/// command itself.  Parsing never fails, so there is no `ALL`; see
/// [`Harness::BUILTIN`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    Opencode,
    Goose,
    Cursor,
    Pi,
    /// Oh My Pi.
    Omp,
    Devin,
    Grok,
    Kimi,
    Amp,
    Hermes,
    Openclaw,
    /// Any other program, by command.
    Custom(String),
}

impl Harness {
    /// The harnesses Axel knows how to drive, in display order.
    pub const BUILTIN: [Harness; 13] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Opencode,
        Harness::Goose,
        Harness::Cursor,
        Harness::Pi,
        Harness::Omp,
        Harness::Devin,
        Harness::Grok,
        Harness::Kimi,
        Harness::Amp,
        Harness::Hermes,
        Harness::Openclaw,
    ];

    /// The builtin name, or the custom command.
    pub fn as_str(&self) -> &str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Opencode => "opencode",
            Harness::Goose => "goose",
            Harness::Cursor => "cursor",
            Harness::Pi => "pi",
            Harness::Omp => "omp",
            Harness::Devin => "devin",
            Harness::Grok => "grok",
            Harness::Kimi => "kimi",
            Harness::Amp => "amp",
            Harness::Hermes => "hermes",
            Harness::Openclaw => "openclaw",
            Harness::Custom(cmd) => cmd,
        }
    }

    /// A builtin by name (case-insensitive), anything else as [`Harness::Custom`].
    /// A builtin's own CLI name works too (`cursor-agent` is [`Harness::Cursor`]).
    pub fn parse(s: &str) -> Harness {
        let t = s.trim();
        let lower = t.to_ascii_lowercase();
        Harness::BUILTIN
            .into_iter()
            .find(|h| h.as_str() == lower || h.program() == lower)
            .unwrap_or_else(|| Harness::Custom(t.to_string()))
    }

    pub fn is_builtin(&self) -> bool {
        !matches!(self, Harness::Custom(_))
    }

    /// The name people know it by.
    pub fn label(&self) -> &str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Opencode => "OpenCode",
            Harness::Goose => "Goose",
            Harness::Cursor => "Cursor",
            Harness::Pi => "Pi",
            Harness::Omp => "Oh My Pi",
            Harness::Devin => "Devin",
            Harness::Grok => "Grok Build",
            Harness::Kimi => "Kimi Code",
            Harness::Amp => "Amp",
            Harness::Hermes => "Hermes Agent",
            Harness::Openclaw => "OpenClaw",
            Harness::Custom(cmd) => cmd,
        }
    }

    /// The harness's own CLI: what you install, and what runs in a pane.
    /// A custom harness's first word.
    pub fn program(&self) -> &str {
        match self {
            Harness::Cursor => "cursor-agent",
            Harness::Custom(cmd) => cmd.split_whitespace().next().unwrap_or(""),
            other => other.as_str(),
        }
    }

    /// The command line of the ACP adapter that lets Axel drive it, for the
    /// builtins that don't have one inside `axel` (Claude's and Codex's do):
    /// most CLIs speak ACP themselves behind a subcommand; Pi and Amp need a
    /// separate adapter.
    pub fn acp_command(&self) -> Option<&'static [&'static str]> {
        Some(match self {
            Harness::Claude | Harness::Codex | Harness::Custom(_) => return None,
            Harness::Opencode => &["opencode", "acp"],
            Harness::Goose => &["goose", "acp"],
            Harness::Cursor => &["cursor-agent", "acp"],
            Harness::Pi => &["pi-acp"],
            Harness::Omp => &["omp", "acp"],
            Harness::Devin => &["devin", "acp"],
            Harness::Grok => &["grok", "agent", "stdio"],
            Harness::Kimi => &["kimi", "acp"],
            Harness::Amp => &["amp-acp"],
            Harness::Hermes => &["hermes-acp"],
            Harness::Openclaw => &["openclaw", "acp"],
        })
    }

    /// How to install it, in one line.
    pub fn install(&self) -> &str {
        match self {
            Harness::Claude => "curl -fsSL https://claude.ai/install.sh | bash",
            Harness::Codex => "npm i -g @openai/codex",
            Harness::Opencode => "npm i -g opencode-ai",
            Harness::Goose => "curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh | CONFIGURE=false bash",
            Harness::Cursor => "curl https://cursor.com/install -fsS | bash",
            Harness::Pi => "npm i -g @earendil-works/pi-coding-agent pi-acp",
            Harness::Omp => "see https://omp.sh",
            Harness::Devin => "see https://docs.devin.ai/cli",
            Harness::Grok => "see https://docs.x.ai/build/overview",
            Harness::Kimi => "see https://kimi.ai/download",
            Harness::Amp => "npm i -g @sourcegraph/amp amp-acp",
            Harness::Hermes => "see https://hermes-agent.nousresearch.com",
            Harness::Openclaw => "see https://docs.openclaw.ai/start/getting-started",
            Harness::Custom(_) => "",
        }
    }
}

/// An agent's harness and, optionally, its model: `claude`, `claude/opus`,
/// `codex/gpt-5.5`.  The model is everything after the first `/`, so model
/// ids with slashes of their own (`opencode/anthropic/claude-opus`) keep them.
/// A custom command never splits (`/usr/local/bin/agent` is a command).
pub fn parse_harness_model(spec: &str) -> (Harness, Option<String>) {
    let spec = spec.trim();
    if let Some((h, model)) = spec.split_once('/')
        && !h.is_empty()
    {
        let harness = Harness::parse(h);
        if harness.is_builtin() {
            let model = model.trim();
            return (harness, (!model.is_empty()).then(|| model.to_string()));
        }
    }
    (Harness::parse(spec), None)
}

impl FromStr for Harness {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Harness::parse(s))
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

impl Serialize for Harness {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Harness::parse(&String::deserialize(d)?))
    }
}

/// Outcome of [`crate::Store::append`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appended {
    /// False when the envelope was already in the log.
    pub inserted: bool,
    /// True when the envelope is reflected in the projections.
    pub projected: bool,
    /// Why it is not projected, when it is not.
    pub rejected_reason: Option<String>,
}

/// A machine running Axel.  Exactly one row per database is flagged local.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Environment {
    pub id: String,
    pub label: String,
    pub rex_host_id: Option<String>,
    pub is_local: bool,
    pub created_at: i64,
    /// For a remote host: how to reach it (`ssh` destination, e.g. `mini`).
    #[serde(default)]
    pub ssh_target: Option<String>,
    /// For a remote host: its Axel owner key, which agents delegate to.
    #[serde(default)]
    pub host_pubkey: Option<String>,
}

/// A directory on an environment, usually a git root.  Local-only.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub environment_id: String,
    pub path: PathBuf,
    pub name: String,
    /// Identifies the repository across environments (e.g. a normalised remote URL).
    pub repo_key: Option<String>,
    pub has_axel_md: bool,
    pub last_opened_at: i64,
}

/// A named identity plus its harness profile.  Local-only.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Agent {
    pub pubkey: String,
    pub name: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub color: Option<AgentColor>,
    pub runtime_mode: RuntimeMode,
    pub auth_tag: Option<Vec<String>>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub archived_at: Option<i64>,
}

/// Input for [`crate::Store::insert_agent`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewAgent {
    pub pubkey: String,
    pub name: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
    pub color: Option<AgentColor>,
    pub runtime_mode: RuntimeMode,
    pub auth_tag: Option<Vec<String>>,
}

/// A thread: an intent with a title, projected from its event log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Thread {
    /// The id of the `thread.create` envelope.
    pub id: String,
    pub title: String,
    pub intent: String,
    pub state: ThreadState,
    /// Signer of the root envelope; the first owner.
    pub created_by: String,
    pub parent_thread_id: Option<String>,
    /// Set when `parent_thread_id` is (and its text is a relationship this build knows).
    pub relationship: Option<Relationship>,
    pub created_at: i64,
    pub updated_at: i64,
    pub done_at: Option<i64>,
}

/// How a thread works on one project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadWorkspace {
    pub kind: WorkspaceKind,
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
}

/// A project attached to a thread, as other replicas see it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadProject {
    pub thread_id: String,
    pub repo_key: Option<String>,
    pub path_hint: PathBuf,
    pub workspace: ThreadWorkspace,
    /// The matching `projects.id` on this replica (by `repo_key`, else by path), if any.
    pub local_project_id: Option<String>,
    pub added_at: i64,
}

/// Someone allowed to act on a thread.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Participant {
    pub thread_id: String,
    pub pubkey: String,
    pub role: ParticipantRole,
    pub added_by: String,
    pub added_at: i64,
}

/// One agent instance on a thread: agent × environment × harness process.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    /// ULID minted by the replica that started it.
    pub id: String,
    pub thread_id: String,
    pub agent_pubkey: String,
    pub environment_id: String,
    pub environment_label: String,
    pub project_repo_key: Option<String>,
    pub harness: Harness,
    pub runtime_mode: RuntimeMode,
    /// Claude `session_id` / Codex thread id, for `--resume`.
    pub harness_session_id: Option<String>,
    pub transcript_path: Option<String>,
    /// Local-only: how far into the transcript the usage reader has consumed.
    pub transcript_offset: i64,
    pub resumed_from: Option<String>,
    pub runtime: SessionRuntime,
    /// Local-only: the `axel session host` control socket, while it runs.
    pub socket_path: Option<PathBuf>,
    pub rex_session_id: Option<String>,
    pub rex_window_id: Option<String>,
    pub rex_block_id: Option<String>,
    pub state: SessionState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub ended_reason: Option<String>,
}

/// Input for [`crate::Store::start_session`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewSession {
    pub thread_id: String,
    pub agent_pubkey: String,
    pub environment_id: String,
    pub environment_label: String,
    pub project_repo_key: Option<String>,
    pub harness: Harness,
    pub runtime_mode: RuntimeMode,
    pub resumed_from: Option<String>,
    pub runtime: SessionRuntime,
}

/// One entry of a managed session's conversation.  Local-only: never an envelope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationItem {
    pub id: String,
    pub session_id: String,
    pub thread_id: String,
    /// Position within the session, from 1.
    pub seq: i64,
    pub turn: i64,
    /// Unix milliseconds.
    pub at: i64,
    pub kind: ItemKind,
    pub status: Option<ItemStatus>,
    /// ACP `toolCallId` / permission request id, for later updates.
    pub ref_id: Option<String>,
    pub content: serde_json::Value,
}

/// Input for [`crate::Store::push_item`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewItem {
    pub session_id: String,
    pub thread_id: String,
    pub turn: i64,
    pub kind: ItemKind,
    pub status: Option<ItemStatus>,
    pub ref_id: Option<String>,
    pub content: serde_json::Value,
}

/// What a completed turn looks like to other participants; the JSON content
/// of a kind-40 `turn` envelope.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnSummary {
    pub ordinal: i64,
    #[serde(default)]
    pub user_text: String,
    /// Head of the assistant's prose, at most 2000 characters.
    #[serde(default)]
    pub assistant_text: String,
    #[serde(default)]
    pub tool_calls: i64,
    #[serde(default)]
    pub files_edited: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub stop_reason: String,
    /// What the turn changed in the workspace, from its checkpoints (`None`
    /// when the workspace isn't a git repository, or the build predates them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<TurnChanges>,
}

/// The tree before and after one turn, and what differs: the shareable part
/// of a turn's checkpoints.  The diff itself stays in the repository that ran
/// the turn (`refs/axel/checkpoints/<thread>/<session>/<ordinal>/{before,after}`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnChanges {
    /// Commit of the tree when the turn started.
    pub before: String,
    /// Commit of the tree when it ended; equal to `before` when nothing changed.
    pub after: String,
    /// Changed files, at most [`TurnChanges::MAX_FILES`].
    #[serde(default)]
    pub files: Vec<FileChange>,
    /// More files changed than `files` lists.
    #[serde(default)]
    pub truncated: bool,
    /// The tree at the start didn't match the end of the session's previous
    /// turn: `files` includes edits made between the two.
    #[serde(default)]
    pub between_turns: bool,
}

impl TurnChanges {
    pub const MAX_FILES: usize = 200;

    /// Lines added and removed over every listed file (binary files count for none).
    pub fn totals(&self) -> (i64, i64) {
        self.files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)))
    }

    /// Files this session's own edit tools touched.
    pub fn authored(&self) -> usize {
        self.files.iter().filter(|f| f.authored).count()
    }
}

/// One file a turn changed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the workspace root.
    pub path: String,
    /// Where it was, for a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    /// Lines added; `None` for a binary file.
    pub added: Option<i64>,
    pub removed: Option<i64>,
    /// One of this session's edit tools touched it during the turn (a file
    /// a shell command changed, or another agent did, is not authored).
    #[serde(default)]
    pub authored: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    #[default]
    Modified,
    Deleted,
    Renamed,
}

impl ChangeStatus {
    /// git's one-letter status.
    pub fn letter(self) -> char {
        match self {
            ChangeStatus::Added => 'A',
            ChangeStatus::Modified => 'M',
            ChangeStatus::Deleted => 'D',
            ChangeStatus::Renamed => 'R',
        }
    }

    /// From git's `--name-status` letter (`R100` → `Renamed`); anything else
    /// (copies, type changes) reads as modified.
    pub fn from_letter(s: &str) -> ChangeStatus {
        match s.chars().next() {
            Some('A') => ChangeStatus::Added,
            Some('D') => ChangeStatus::Deleted,
            Some('R') => ChangeStatus::Renamed,
            _ => ChangeStatus::Modified,
        }
    }
}

/// Where a pane session's turn stands, for hooks that run once per event
/// (local-only).  `open` while a turn runs: `before_*` is its checkpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnMark {
    pub session_id: String,
    /// The current (open) or last turn.
    pub ordinal: i64,
    pub open: bool,
    pub before_commit: Option<String>,
    pub before_tree: Option<String>,
    /// The tree the previous turn ended with.
    pub last_after_tree: Option<String>,
    pub user_text: String,
    /// Files this turn's edit tools named (absolute paths).
    pub authored: Vec<String>,
    pub tool_calls: i64,
    /// Unix milliseconds.
    pub started_at: i64,
}

/// A projected `turn` envelope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    /// The envelope id.
    pub id: String,
    pub thread_id: String,
    pub session_id: String,
    pub from_pubkey: String,
    pub created_at: u64,
    pub summary: TurnSummary,
}

/// Input for [`crate::Store::update_session`].  Every `None` leaves the field alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionUpdate {
    pub state: Option<SessionState>,
    pub harness_session_id: Option<String>,
    pub transcript_path: Option<String>,
    /// `(rex_session_id, rex_window_id, rex_block_id)`; sets all three.
    pub rex: Option<(Option<String>, Option<String>, Option<String>)>,
    /// The mode was switched mid-session (Shift+Tab in the thread view).
    pub runtime_mode: Option<RuntimeMode>,
}

/// Input for [`crate::Store::record_status`]; also the JSON content of a kind-20 envelope.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusReport {
    /// Unknown text in an old envelope reads as [`StatusState::Idle`].
    #[serde(default, deserialize_with = "lenient::or_default")]
    pub state: StatusState,
    /// Unknown text in an old envelope reads as `None`.
    #[serde(
        default,
        deserialize_with = "lenient::or_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub kind: Option<BlockedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
}

/// Live status of a session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub session_id: String,
    pub thread_id: String,
    pub state: StatusState,
    pub kind: Option<BlockedKind>,
    pub title: Option<String>,
    pub msg: Option<String>,
    pub progress: Option<u8>,
    pub updated_at: i64,
}

impl Status {
    /// Rank for "most urgent": blocked > error > working > done > idle.
    pub fn urgency(&self) -> u8 {
        self.state.urgency()
    }
}

/// Accumulated token usage of one session (or, from
/// [`crate::Store::thread_usage`], of a whole thread with `session_id = ""`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub session_id: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub turns: i64,
    pub cost_usd_est: f64,
    pub updated_at: i64,
}

/// What one [`crate::Store::add_usage`] call adds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageDelta {
    /// Input not read from the prompt cache (cache reads and writes are
    /// their own counts, never included here).
    pub input_tokens: i64,
    /// Output, reasoning included.
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// The part of `output_tokens` spent reasoning.
    pub reasoning_tokens: i64,
    pub turns: i64,
    /// The harness's own figure when it reports one, else an estimate.
    pub cost_usd_est: f64,
    /// The model that did most of the work.
    pub model: Option<String>,
}

/// Token counts and cost summed over some window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub turns: i64,
    pub cost_usd: f64,
}

impl UsageTotals {
    /// Every token the model processed: input (cached or not) and output.
    pub fn tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// What one agent (or one thread) spent: in the last hour, the last 24
/// hours, and ever.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageWindows {
    /// The agent's public key, or the thread id.
    pub key: String,
    pub last_hour: UsageTotals,
    pub last_day: UsageTotals,
    pub total: UsageTotals,
    /// When it last spent anything (Unix seconds).
    pub last_at: i64,
}

str_enum! {
    /// One of a plan's usage windows, as harnesses report them.
    LimitWindow {
        /// The rolling five hours (Claude's and Codex's session limit).
        FiveHour => "five_hour",
        Week => "week",
        /// Free and lower plans' monthly allowance.
        Month => "month",
    }
}

impl LimitWindow {
    /// The window lasting `mins` minutes.
    pub fn from_minutes(mins: u64) -> LimitWindow {
        if mins >= 28 * 24 * 60 {
            LimitWindow::Month
        } else if mins >= 7 * 24 * 60 {
            LimitWindow::Week
        } else {
            LimitWindow::FiveHour
        }
    }

    /// As few characters as say it: `5h`, `wk`, `mo`.
    pub fn short(self) -> &'static str {
        match self {
            LimitWindow::FiveHour => "5h",
            LimitWindow::Week => "wk",
            LimitWindow::Month => "mo",
        }
    }
}

/// How much of a plan window a harness's account has used, as it last said.
/// Local-only: it describes the account signed in on this machine.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlanLimit {
    pub harness: Harness,
    pub window: LimitWindow,
    /// 0–100.
    pub used_pct: f64,
    /// When the window starts over (Unix seconds).
    pub resets_at: Option<i64>,
    pub updated_at: i64,
}

/// How full a session's context window is, as its harness last said.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionContext {
    pub session_id: String,
    pub used: i64,
    pub size: i64,
    pub updated_at: i64,
}

/// One usage envelope, kept for 24 hours.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageSample {
    pub session_id: String,
    pub at: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Everything the dashboard shows about one thread.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ThreadSummary {
    pub thread: Thread,
    pub projects: Vec<ThreadProject>,
    pub participants: Vec<Participant>,
    pub sessions_total: i64,
    /// Sessions not yet ended, newest first.
    pub sessions_live: Vec<Session>,
    /// Distinct agent pubkeys across all sessions, sorted.
    pub agents: Vec<String>,
    /// Distinct environment labels across all sessions, sorted.
    pub environments: Vec<String>,
    /// Sum over all sessions (`session_id = ""`).
    pub usage: Usage,
    /// Most urgent status among live sessions: blocked > error > working > done > idle.
    pub live: Option<Status>,
}

/// Filter for [`crate::Store::list_threads`].  Every `None` means "any".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadFilter {
    pub states: Option<Vec<ThreadState>>,
    /// Threads with at least one session by this agent.
    pub agent_pubkey: Option<String>,
    /// Threads this pubkey participates in.
    pub participant: Option<String>,
    /// Threads with this repository attached.
    pub repo_key: Option<String>,
    pub limit: Option<usize>,
}

/// Filter for [`crate::Store::list_sessions`].  Every `None` means "any".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub thread_id: Option<String>,
    pub agent_pubkey: Option<String>,
    pub environment_id: Option<String>,
    pub states: Option<Vec<SessionState>>,
    pub limit: Option<usize>,
}
