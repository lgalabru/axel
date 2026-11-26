//! Web panes (RFD 0007): a thread's view drawn as a web page in its Rex
//! pane by Ladybird's terminal mode (`--headless=terminal`: frames as Kitty
//! graphics, input from the terminal's mouse and key reports), instead of as
//! terminal text.
//!
//! `axel web pane THREAD` is the pane's whole program.  It serves the
//! thread on `127.0.0.1` from this process and runs Ladybird in the same
//! pane; when Ladybird closes, the server goes with it, so nothing outlives
//! the pane.  The page shows the model the terminal view
//! draws (`axel_tui::load_thread_data`) and sends what you type the way the
//! terminal composer does (`axel_tui::thread::dispatch`).  Where Ladybird
//! isn't installed, or can't draw, the pane is the terminal view; `axel
//! panes terminal` makes that the choice everywhere.

mod assets;
mod server;
mod view;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_tui::{SendCommand, SendReply};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Ctx;
use server::Action;

/// Where Ladybird is looked for, after `$AXEL_LADYBIRD`: on PATH (Linux
/// packages), then the macOS app in `/Applications` and `~/Applications`.
const LADYBIRD_ON_PATH: [&str; 2] = ["ladybird", "Ladybird"];
const LADYBIRD_APP: &str = "Ladybird.app/Contents/MacOS/Ladybird";

/// Ladybird's executable, when it is installed.  `$AXEL_LADYBIRD`, when
/// set, is the only place looked at: a path, or anything that isn't one to
/// mean "none" (what the tests use).
pub fn ladybird() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("AXEL_LADYBIRD") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    if let Some(found) = LADYBIRD_ON_PATH.iter().find_map(|name| super::launch::which(name)) {
        return Some(found);
    }
    let homes = [Some(PathBuf::from("/Applications")), dirs::home_dir().map(|h| h.join("Applications"))];
    homes.into_iter().flatten().map(|dir| dir.join(LADYBIRD_APP)).find(|p| p.is_file())
}

/// Ladybird that gives up within this long of starting couldn't draw in
/// this terminal: the pane falls back to the terminal view.  It comes up in
/// well under a second; a refusal comes well before three.
const FALLBACK_WINDOW_SECS: u64 = 3;

/// The most text one message may carry from the page (bytes): the body
/// limit of the server, which also holds the JSON around it.
const MESSAGE_BYTES_MAX: usize = server::BODY_BYTES_MAX;

/// How a thread's panes are drawn.  Web panes are experimental, so they
/// are off until the owner turns them on, and then used only where Ladybird
/// is installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneChoice {
    /// Terminal text: the default.
    #[default]
    Terminal,
    /// A web page wherever Ladybird is installed; terminal text elsewhere.
    Web,
}

impl PaneChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneChoice::Terminal => "terminal",
            PaneChoice::Web => "web",
        }
    }

    pub fn parse(text: &str) -> Option<PaneChoice> {
        [PaneChoice::Terminal, PaneChoice::Web].into_iter().find(|c| c.as_str() == text.trim())
    }
}

/// A saved choice this build doesn't know (a newer build's) reads as the
/// default rather than making the whole settings file unreadable.
impl<'de> Deserialize<'de> for PaneChoice {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<PaneChoice, D::Error> {
        Ok(PaneChoice::parse(&String::deserialize(d)?).unwrap_or_default())
    }
}

/// `$AXEL_HOME/settings.json`: what the owner chose here, local to this
/// machine.  Unknown fields from a newer build are kept.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    panes: PaneChoice,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

fn settings_path() -> PathBuf {
    crate::paths::home().join("settings.json")
}

fn load_settings() -> Settings {
    // A missing or unreadable file is the defaults: setting a choice writes it.
    std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// The pane choice in force: `$AXEL_PANES` when set, else the saved one.
pub fn pane_choice() -> PaneChoice {
    std::env::var("AXEL_PANES").ok().and_then(|v| PaneChoice::parse(&v)).unwrap_or_else(|| load_settings().panes)
}

/// Whether a thread view opens as a web pane: the owner turned web panes
/// on, and Ladybird is here to draw them.
pub fn web_panes() -> bool {
    pane_choice() == PaneChoice::Web && ladybird().is_some()
}

/// What a thread's pane runs when it is a web pane.
pub fn pane_command(thread_id: &str) -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    vec![exe, "web".into(), "pane".into(), thread_id.to_string()]
}

/// `axel panes [CHOICE]`: show or set how thread views are drawn.
pub fn panes(choice: Option<&str>) -> Result<()> {
    let Some(choice) = choice else {
        let current = pane_choice();
        let browser = ladybird().map_or_else(|| t!("not installed").to_string(), |p| p.display().to_string());
        println!("{}", t!("panes: {choice} (Ladybird: {browser})", choice = current.as_str(), browser = browser));
        return Ok(());
    };
    let choice = PaneChoice::parse(choice).with_context(|| t!("panes are web or terminal"))?;
    let mut settings = load_settings();
    settings.panes = choice;
    let path = settings_path();
    std::fs::create_dir_all(path.parent().context("settings live in a directory")?)?;
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("settings live in a directory")?)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), &settings)?;
    tmp.persist(&path)?;
    println!("{}", t!("thread views open as {choice} panes from now on", choice = choice.as_str()));
    if choice == PaneChoice::Web && ladybird().is_none() {
        println!("{}", t!("Ladybird isn't installed, so they stay terminal panes until it is: ladybird.org"));
    }
    Ok(())
}

/// `axel web pane THREAD`: this pane shows the thread as a web page.
pub fn pane(thread_id: &str) -> Result<()> {
    let backend = ThreadBackend::open(thread_id)?;
    let Some(browser) = ladybird() else {
        return fall_back(thread_id, t!("Ladybird isn't installed"));
    };
    // After a Rex restart this pane comes back as a web pane again.
    super::resume::come_back_as(&["web", "pane", thread_id], std::env::current_dir().ok().as_deref());
    let running = server::start(Arc::new(backend))?;
    let started = Instant::now();
    // Terminal mode draws in this pane.  A temporary profile keeps each pane
    // apart from the owner's Ladybird (cookies, history) and from each other.
    let status = std::process::Command::new(&browser)
        .args(["--headless=terminal", "--temporary-profile", &running.url()])
        .status()
        .with_context(|| t!("starting {program}", program = browser.display()))?;
    let gave_up = !status.success() && started.elapsed() < Duration::from_secs(FALLBACK_WINDOW_SECS) && running.served() == 0;
    if gave_up {
        return fall_back(thread_id, &t!("Ladybird couldn't draw here ({status})", status = status));
    }
    Ok(())
}

/// The terminal view in this pane instead, saying why.
fn fall_back(thread_id: &str, why: &str) -> Result<()> {
    eprintln!("axel: {}", t!("{why}; showing the thread as terminal text", why = why));
    super::ui::open_here_as_text(thread_id)
}

/// `axel web serve THREAD`: the pane's server alone, with its address on
/// stdout, until interrupted.  For a desktop browser, and for tests.
pub fn serve(thread_id: &str) -> Result<()> {
    let backend = ThreadBackend::open(thread_id)?;
    let running = server::start(Arc::new(backend))?;
    println!("{}", running.url());
    // Flushing makes the address readable by whoever started us; if stdout
    // is gone there is no one to read it either.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    loop {
        std::thread::park();
    }
}

/// The thread a pane shows, and what the page may do to it.
struct ThreadBackend {
    thread_id: String,
    owner_pk: String,
    /// For loading, which polls.  Actions open a context of their own on
    /// the connection's thread (a `Ctx` holds Rex's runner, which stays on
    /// one thread), so they never wait behind a poll.
    reader: Mutex<axel_store::Store>,
}

impl ThreadBackend {
    fn open(thread_id: &str) -> Result<ThreadBackend> {
        let ctx = Ctx::open()?;
        let thread = ctx.store.get_thread(thread_id)?.with_context(|| t!("no thread {id}", id = thread_id))?;
        let owner_pk = ctx.owner()?.public().to_base58();
        let reader = axel_store::Store::open_default().context(t!("opening ~/.axel/axel.db"))?;
        Ok(ThreadBackend { thread_id: thread.id, owner_pk, reader: Mutex::new(reader) })
    }

    fn send(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        let text = body["text"].as_str().map(str::trim).unwrap_or_default();
        if text.is_empty() {
            bail!("{}", t!("nothing to send"));
        }
        if text.len() > MESSAGE_BYTES_MAX {
            bail!("{}", t!("that message is longer than {max} bytes", max = MESSAGE_BYTES_MAX));
        }
        let steer = body["steer"].as_bool().unwrap_or(false);
        let data = axel_tui::load_thread_data(&ctx.store, &self.owner_pk, &self.thread_id)?;
        match axel_tui::thread::dispatch(text, &data) {
            axel_tui::thread::Dispatch::Prompt { session_id, text } => {
                let reply = super::ui::send_to_session(ctx, &session_id, SendCommand::Prompt { text, attachments: vec![], steer })?;
                reply_json(reply)
            }
            axel_tui::thread::Dispatch::Summon { agent, text } => {
                super::ui::summon(ctx, &self.thread_id, &agent, &text, &[])?;
                Ok(json!({ "note": t!("summoning @{agent}…", agent = agent) }))
            }
            axel_tui::thread::Dispatch::NoTarget => {
                bail!("{}", t!("no agent is on this thread yet: start with @name and what you want done"))
            }
        }
    }

    /// A session of this thread named by the page, which may not reach others.
    fn own_session(&self, ctx: &Ctx, body: &Value) -> Result<String> {
        let id = body["session_id"].as_str().context(t!("which session?"))?;
        let session = ctx.store.get_session(id)?.with_context(|| t!("no session {id}", id = id))?;
        if session.thread_id != self.thread_id {
            bail!("{}", t!("that session is on another thread"));
        }
        Ok(session.id)
    }
}

fn reply_json(reply: SendReply) -> Result<Value> {
    Ok(match reply {
        SendReply::Ok | SendReply::Status { .. } => json!({ "ok": true }),
        SendReply::Queued { .. } => json!({ "note": t!("queued: it goes in when the running turn ends") }),
        SendReply::Steering { .. } => json!({ "note": t!("sent into the running turn: read at its next step") }),
        SendReply::Error { message } => bail!("{message}"),
    })
}

impl server::Backend for ThreadBackend {
    fn load(&self) -> Result<Value> {
        let store = self.reader.lock().map_err(|_| anyhow::anyhow!("the reader was poisoned"))?;
        let data = axel_tui::load_thread_data(&store, &self.owner_pk, &self.thread_id)?;
        Ok(view::thread(&data))
    }

    fn marker(&self) -> Option<(i64, i64)> {
        // Can't tell: the poll then loads in full, which is correct, only slower.
        self.reader.lock().ok().and_then(|store| store.change_marker().ok())
    }

    fn act(&self, action: Action, body: &Value) -> Result<Value> {
        let ctx = Ctx::open()?;
        match action {
            Action::Send => self.send(&ctx, body),
            Action::Answer => {
                let session_id = self.own_session(&ctx, body)?;
                let item_id = body["item_id"].as_str().context(t!("which question?"))?.to_string();
                let option_id = body["option_id"].as_str().context(t!("which answer?"))?.to_string();
                reply_json(super::ui::send_to_session(&ctx, &session_id, SendCommand::Answer { item_id, option_id })?)
            }
            Action::Cancel => {
                let session_id = self.own_session(&ctx, body)?;
                reply_json(super::ui::send_to_session(&ctx, &session_id, SendCommand::Cancel)?)
            }
        }
    }
}
