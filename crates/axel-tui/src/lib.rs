//! axel-tui — the ratatui dashboard, launcher and inbox for Axel.
//! See docs/DESIGN.md ("Dashboard", "The launch flow") and docs/CONTRACTS.md.
//!
//! Layout of this crate:
//! - `dashboard`, `launcher`, `inbox`: pure state machines fed with `KeyEvent`s.
//! - `view::*`: rendering of those states onto a ratatui `Frame`.
//! - `glue`: the one module that reads/writes `axel_store`, `axel_identity`, `axel_inbox`.
//! - `app`: terminal ownership and the event loop.

use std::path::PathBuf;

mod app;
pub mod attach;
mod compose;
mod stream;
pub mod dashboard;
mod glue;
pub mod inbox;
pub mod input;
pub mod launcher;
pub mod model;
pub mod picker;
pub mod theme;
pub mod thread;
pub mod util;
pub mod view;

pub use compose::{run_answer, run_compose};

/// How tall the launcher's prompt pane should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSize {
    /// The prompt: this many rows, its text and the rows around it.
    Rows(u16),
    /// The full launch form, which takes most of the layer.
    Form,
}
pub use stream::run_stream;
pub use thread::{SendCommand, SendReply};
pub use util::{relative_time, slugify};

/// Which screen the UI opens on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Launcher,
    Inbox,
    /// The conversation view of one thread.
    Thread(String),
    /// Recent threads: the pane under the launcher's prompt.
    Threads,
}

/// A completed launcher form, ready for the CLI to turn into a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub title: String,
    /// 0..n projects to attach; empty = scratch thread.
    pub projects: Vec<PathBuf>,
    pub agent_pubkey: String,
    pub workspace: axel_store::WorkspaceKind,
    pub branch: Option<String>,
    /// `Some` = spawn another session for `agent_pubkey` onto this existing thread.
    pub thread_id: Option<String>,
    /// Files dropped onto the prompt field.
    pub attachments: Vec<PathBuf>,
}

/// Why the UI exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Launch(LaunchRequest),
    /// Focus the Rex window hosting this session.
    FocusSession(String),
    /// Focus the raw log pane (the session host) of this session.
    OpenLog(String),
    Quit,
}

/// What the recent-threads pane can do to a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadOp {
    /// Finished: still listed, under the open threads.
    Settle,
    /// Back to open.
    Reopen,
    /// Stop its sessions and hide it.
    Archive,
    /// Stop its sessions and hide it everywhere for good.
    Delete,
}

/// `manage_thread(thread_id, op)`.
pub type ManageThreadFn<'a> = dyn Fn(&str, ThreadOp) -> anyhow::Result<()> + 'a;

/// `open_thread(thread_id)`: show a thread outside this TUI.
pub type OpenThreadFn<'a> = dyn Fn(&str) -> anyhow::Result<()> + 'a;

/// `summon(thread_id, agent_name, text, attachments) -> session_id`.
pub type SummonFn<'a> = dyn Fn(&str, &str, &str, &[PathBuf]) -> anyhow::Result<String> + 'a;

/// Everything the UI needs from the outside world.
pub struct Deps<'a> {
    pub store: &'a axel_store::Store,
    pub keystore: &'a axel_identity::Keystore,
    pub cwd: PathBuf,
    /// Harness names offered by the new-agent form, e.g. `["claude", "codex", "opencode"]`.
    pub harnesses: Vec<String>,
    /// Send a command to a managed session's host socket.
    pub send: &'a dyn Fn(&str, SendCommand) -> anyhow::Result<SendReply>,
    /// Start a new managed session for `agent name` on `thread_id` with an initial
    /// prompt and attachments; returns the new session id.
    pub summon: &'a SummonFn<'a>,
    /// Human label of the runtime a harness would get, shown on the launcher's
    /// summary, e.g. `"managed (claude-code-acp)"` or `"pane (claude-code-acp not found on PATH)"`.
    pub runtime_label: &'a dyn Fn(&axel_store::Harness) -> String,
    /// Open a thread outside this TUI (inside Rex: a stream pane plus a
    /// composer pane).  `None`, or an error, opens the in-TUI thread view.
    pub open_thread: Option<&'a OpenThreadFn<'a>>,
    /// Move keyboard focus to the paired pane (prompt ⇄ recent threads), when
    /// there is one.
    pub focus_peer: Option<&'a dyn Fn()>,
    /// Size the prompt pane (the recent threads take the rest of the layer).
    pub fit_prompt: Option<&'a dyn Fn(PromptSize)>,
    /// Settle, reopen, archive or delete a thread.  `None` hides those keys.
    pub manage_thread: Option<&'a ManageThreadFn<'a>>,
}

/// Run the TUI until the user launches a thread, focuses one, or quits.
///
/// Owns the terminal: enables raw mode and the alternate screen, installs a
/// panic hook that restores both, and restores them on return.
pub fn run(deps: Deps, initial: Screen) -> anyhow::Result<Outcome> {
    app::run(deps, initial)
}
