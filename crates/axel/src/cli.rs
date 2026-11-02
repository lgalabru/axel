use axel_store::{AgentColor, RuntimeMode};
use clap::{Parser, Subcommand, ValueEnum};

/// Axel: agent orchestration, designed for Rex.
///
/// With no subcommand, `axel` opens the launcher. Inside Rex it opens as a
/// floating centered pane; elsewhere it runs inline.
#[derive(Parser, Debug)]
#[command(name = "axel", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the TUI in the current terminal (what the floating pane runs).
    Ui {
        /// Screen to open on. Defaults to inbox when something needs you, else launcher.
        #[arg(long, value_enum)]
        screen: Option<ScreenArg>,
        /// Thread to open (with --screen thread).
        #[arg(long)]
        thread: Option<String>,
    },
    /// Sessions: one agent instance on a thread.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Tail a managed session's raw harness log.
    Log {
        session: String,
        #[arg(long, short)]
        follow: bool,
    },
    /// Agents: named Ed25519 identities with a harness profile and an inbox.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Threads: units of work (project + agent + workspace + intent).
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Ingest a harness hook event from stdin (used by generated hook configs).
    Hook {
        /// Harness that produced the event.
        #[arg(value_enum)]
        harness: HarnessArg,
        /// Only run the PreToolUse guard (managed sessions: the host owns status).
        #[arg(long)]
        guard_only: bool,
    },
    /// Report a status for the current thread (reads $AXEL_THREAD).
    Status {
        /// idle | working | done | blocked | error | clear
        state: String,
        /// One-line message.
        msg: Option<String>,
        /// For blocked: permission | question | auth
        #[arg(long)]
        kind: Option<String>,
    },
    /// Send a signed message to agents by `@name` (as the owner, or as $AXEL_AGENT).
    Send {
        /// Message text; `@name` mentions are resolved to recipients.
        text: Vec<String>,
    },
    /// Print an inbox.
    Inbox {
        /// Agent name; defaults to the owner's inbox.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Get this machine ready and create an agent (runs by itself the first time).
    Setup,
    /// The built-in Claude Code ACP agent (what managed Claude sessions run).
    #[command(name = "claude-acp", hide = true)]
    ClaudeAcp,
    /// The built-in Codex ACP agent (what managed Codex sessions run).
    #[command(name = "codex-acp", hide = true)]
    CodexAcp,
    /// Check Rex, keystore and database health.
    Doctor,
    /// Start the MCP server on stdio (for MCP client integration).
    Mcp,
    /// Remote machines that run agents for this one.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Exchange threads, transcripts and job branches with remote hosts.
    Sync {
        /// Host label; all hosts when omitted.
        host: Option<String>,
        /// Keep syncing while the host runs sessions for us.
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HostCommand {
    /// Install Axel on a machine reachable over SSH and register it.
    Add {
        /// Name for the host (also its Rex host label, if Rex knows one).
        label: String,
        /// SSH destination; defaults to the label.
        #[arg(long)]
        ssh: Option<String>,
        /// Install what's missing there (Node, Claude Code, the ACP adapter) with Homebrew and npm.
        #[arg(long)]
        install: bool,
    },
    /// List hosts.
    #[command(alias = "list")]
    Ls,
    /// Forget a host.
    #[command(alias = "remove")]
    Rm { label: String },
    /// Serve one request from another machine (run over SSH).
    #[command(hide = true)]
    Rpc,
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Create an agent: generates a key, stores it, attests it, records it.
    New {
        name: String,
        #[arg(long, default_value = "claude")]
        harness: String,
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated skill names, or `*`.
        #[arg(long, default_value = "*")]
        skills: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, value_enum)]
        color: Option<AgentColorArg>,
        /// autopilot never asks; edits auto-accepts file edits; supervised asks; plan is read-only.
        #[arg(long, value_enum, default_value_t = RuntimeModeArg::Autopilot)]
        runtime_mode: RuntimeModeArg,
        /// Run it on this host (see `axel host`) unless told otherwise.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// Change an agent's permissions, budget, runtime mode or default host.
    Edit {
        name: String,
        #[arg(long, value_enum)]
        runtime_mode: Option<RuntimeModeArg>,
        /// Run it on this host unless told otherwise; `local` runs it here again.
        #[arg(long, value_name = "HOST|local")]
        host: Option<String>,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// List agents.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Show one agent, including its attestation.
    Show { name: String },
    /// Archive an agent (keeps its key and history).
    Archive { name: String },
}

/// Who an agent may reach and what it may spend (`agent new` / `agent edit`).
///
/// Agents reach nobody by default: an agent can message or summon another
/// only when allowed.  Loops (summoning an agent already in the chain) are
/// always refused, and chains stop at `--max-depth`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct PolicyArgs {
    /// Let this agent message and summon these agents (names, comma-separated, repeatable).
    #[arg(long, value_delimiter = ',', value_name = "AGENT")]
    pub allow: Vec<String>,
    /// Take these agents off the list.
    #[arg(long, value_delimiter = ',', value_name = "AGENT")]
    pub disallow: Vec<String>,
    /// Replace the list: nobody, or any agent.
    #[arg(long, value_enum)]
    pub reach: Option<ReachArg>,
    /// Daily spend cap in estimated USD, or `unlimited` (the default).
    #[arg(long, value_name = "USD|unlimited")]
    pub daily_usd: Option<String>,
    /// How deep a summon chain this agent may extend (default 3).
    #[arg(long, value_name = "N")]
    pub max_depth: Option<u32>,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ReachArg {
    Nobody,
    Anyone,
}

#[derive(Subcommand, Debug)]
pub enum ThreadCommand {
    /// Create a thread and spin up its first agent session.
    New {
        /// What we're building (the intent; also the first prompt).
        title: Vec<String>,
        /// Agent name.
        #[arg(long)]
        agent: String,
        /// Project directories (repeatable). None = scratch thread. Default: current directory.
        #[arg(long = "project", value_name = "DIR")]
        projects: Vec<std::path::PathBuf>,
        /// Start with no project at all (scratch).
        #[arg(long)]
        scratch: bool,
        /// Work in a new git worktree on this branch (default: axel/<slug>).
        #[arg(long, value_name = "BRANCH", num_args = 0..=1, default_missing_value = "")]
        worktree: Option<String>,
        /// Record the thread without starting the harness.
        #[arg(long)]
        no_launch: bool,
        /// managed (default when the ACP adapter is installed) or pane.
        #[arg(long, value_enum)]
        runtime: Option<RuntimeArg>,
        /// Run the agent on this host (default: the agent's own; `local` for here).
        /// Remote agents always work on their own branch.
        #[arg(long, value_name = "HOST|local")]
        host: Option<String>,
    },
    /// Spin up another agent onto an existing thread.
    Spawn {
        thread: String,
        #[arg(long)]
        agent: String,
        /// First message for the new agent (defaults to the thread intent).
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, value_enum)]
        runtime: Option<RuntimeArg>,
        /// Run the agent on this host (default: the agent's own; `local` for here).
        #[arg(long, value_name = "HOST|local")]
        host: Option<String>,
    },
    /// Open a thread's view in this terminal.
    Open { thread: String },
    /// Open a thread as a Rex window of two panes: the conversation streaming
    /// as plain terminal output (native scrollback) above, the composer below.
    Split { thread: String },
    /// The conversation as plain terminal output, following new items (the top pane of `split`).
    Stream { thread: String },
    /// Only the composer: prompt, queue, approvals, mode (the bottom pane of `split`).
    Compose { thread: String },
    /// Continue a thread: a new session resuming its last harness conversation.
    Resume {
        thread: String,
        /// Agent to resume as; defaults to the agent of the last session.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Attach a project to a thread.
    Attach {
        thread: String,
        #[arg(long)]
        project: std::path::PathBuf,
        #[arg(long, value_name = "BRANCH", num_args = 0..=1, default_missing_value = "")]
        worktree: Option<String>,
    },
    /// List threads.
    Ls {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        all: bool,
    },
    /// Show a thread: projects, participants, sessions, usage, messages.
    Show { thread: String },
    /// End every live session on a thread (closes their panes).
    Stop { thread: String },
    /// Settle a thread: finished, still listed (under "settled").
    Done { thread: String },
    /// Reopen a settled or archived thread.
    Reopen { thread: String },
    /// Archive a thread: stops its sessions and hides it.
    Archive { thread: String },
    /// Delete a thread: stops its sessions and hides it everywhere for good.
    Delete { thread: String },
}

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// Host a managed (ACP) session: runs in its own pane and prints the raw log.
    Host { session: String },
    /// End a session (and close its pane).
    Stop { session: String },
    /// Move a running Claude session to a host, or back (`--to local`): it
    /// stops, and a new session on the same thread resumes its conversation
    /// there, with its code (uncommitted changes too).
    Move {
        session: String,
        #[arg(long, value_name = "HOST|local")]
        to: String,
    },
    /// List sessions.
    Ls {
        #[arg(long)]
        all: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ScreenArg {
    Dashboard,
    Launch,
    Inbox,
    Thread,
    /// Recent threads (the pane under the prompt).
    Threads,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum RuntimeArg {
    /// Axel drives the harness over ACP; you work in the thread view.
    Managed,
    /// The harness's own CLI runs in a Rex pane.
    Pane,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum HarnessArg {
    Claude,
    Codex,
}

/// `--runtime-mode`; mirrors [`RuntimeMode`] so clap can list and validate it.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum RuntimeModeArg {
    Autopilot,
    Edits,
    Supervised,
    Plan,
}

impl From<RuntimeModeArg> for RuntimeMode {
    fn from(m: RuntimeModeArg) -> Self {
        match m {
            RuntimeModeArg::Autopilot => RuntimeMode::Autopilot,
            RuntimeModeArg::Edits => RuntimeMode::Edits,
            RuntimeModeArg::Supervised => RuntimeMode::Supervised,
            RuntimeModeArg::Plan => RuntimeMode::Plan,
        }
    }
}

/// `--color`; mirrors [`AgentColor`].
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum AgentColorArg {
    Purple,
    Yellow,
    Red,
    Green,
    Blue,
    Gray,
    Orange,
}

impl From<AgentColorArg> for AgentColor {
    fn from(c: AgentColorArg) -> Self {
        match c {
            AgentColorArg::Purple => AgentColor::Purple,
            AgentColorArg::Yellow => AgentColor::Yellow,
            AgentColorArg::Red => AgentColor::Red,
            AgentColorArg::Green => AgentColor::Green,
            AgentColorArg::Blue => AgentColor::Blue,
            AgentColorArg::Gray => AgentColor::Gray,
            AgentColorArg::Orange => AgentColor::Orange,
        }
    }
}
