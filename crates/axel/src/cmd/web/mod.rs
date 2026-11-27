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
mod attachments;
mod controls;
mod guard;
mod server;
mod terminal;
mod theme;
mod view;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_tui::manage::Outcome;
use axel_tui::{SendCommand, SendReply};
use serde_json::{Value, json};

use super::Ctx;
use super::display::{Display, Pane};
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

/// Device pixels per CSS pixel for a pane of `size`: a Retina display, or a
/// phone, draws more pixels per cell, so without this the page is drawn
/// small.  `$AXEL_WEB_SCALE` overrides it.
fn device_pixel_ratio(size: &terminal::PaneSize) -> f64 {
    if let Some(scale) = std::env::var("AXEL_WEB_SCALE").ok().and_then(|v| v.parse::<f64>().ok()) {
        return scale.clamp(DEVICE_PIXEL_RATIO_MIN, DEVICE_PIXEL_RATIO_MAX);
    }
    size.cell_height_px().map_or(DEVICE_PIXEL_RATIO_MIN, ratio_for_cell_height)
}

/// The ratio a terminal cell `cell_height` device pixels tall implies.  A
/// terminal's 13–14 point font makes cells about 17 CSS pixels tall, so the
/// ratio is how many of those fit, to the nearest half.
fn ratio_for_cell_height(cell_height: f64) -> f64 {
    const CSS_CELL_HEIGHT: f64 = 17.0;
    ((cell_height / CSS_CELL_HEIGHT * 2.0).round() / 2.0).clamp(DEVICE_PIXEL_RATIO_MIN, DEVICE_PIXEL_RATIO_MAX)
}


/// No display draws fewer device pixels than CSS pixels, and none we know
/// more than three.
const DEVICE_PIXEL_RATIO_MIN: f64 = 1.0;
const DEVICE_PIXEL_RATIO_MAX: f64 = 3.0;

/// `axel web pane THREAD --view PANE`: this pane shows one of the thread's
/// panes as a web page, or its terminal pane where Ladybird can't draw.
pub fn pane(thread_id: &str, view: Pane) -> Result<()> {
    let backend = ThreadBackend::open(thread_id, view)?;
    let Some(browser) = ladybird() else {
        return fall_back(thread_id, view, t!("Ladybird isn't installed"));
    };
    // After a Rex restart this pane comes back as the same web pane.
    super::resume::come_back_as(&["web", "pane", thread_id, "--view", view.as_str()], std::env::current_dir().ok().as_deref());
    // The composer closes the whole thread view, as the terminal one does.
    // Every view ends its Ladybird when the pane goes.
    let undock = (view == Pane::Composer).then(Undock::find).flatten();
    let on_close = undock.clone();
    super::ui::on_hangup(move || {
        terminal::end_running();
        if let Some(u) = on_close {
            u.run();
        }
    });
    let running = server::start(Arc::new(backend), view, "")?;
    let started = Instant::now();
    // Terminal mode draws in this pane.  A temporary profile keeps each pane
    // apart from the owner's Ladybird (cookies, history) and from each other.
    // Before each start the page takes the pane's colours and text size, as
    // its frames can't be transparent and its text should be the terminal's.
    let mut dock_fitted = false;
    let fit = |size: &terminal::PaneSize, ratio: f64, colors: &terminal::Colors| {
        if !dock_fitted && size.rows > 0 {
            if let Some(dock) = &undock
                && let Some(stream) = &dock.stream
            {
                super::ui::fit_web_dock(&axel_rex::Rex::new(dock.rex.clone()), stream, dock.usage.as_deref(), size.rows);
            }
            dock_fitted = true;
        }
        running.set_theme(&theme::css(colors, theme::font_px(size, ratio)));
        running.url()
    };
    let outcome = terminal::run(&browser, &["--headless=terminal", "--temporary-profile"], device_pixel_ratio, fit);
    // Only a Ladybird that gave up at once, before loading the page, means
    // it can't draw here; a pane that went later just went.
    let early = started.elapsed() < Duration::from_secs(FALLBACK_WINDOW_SECS) && running.served() == 0;
    match &outcome {
        Ok(status) if early && !status.success() => {
            return fall_back(thread_id, view, &t!("Ladybird couldn't draw here ({status})", status = status));
        }
        Err(e) if early => return fall_back(thread_id, view, &t!("Ladybird couldn't draw here ({status})", status = format!("{e:#}"))),
        _ => {}
    }
    outcome?;
    if let Some(u) = undock {
        u.run();
    }
    Ok(())
}

/// What closing a web composer closes with it, found when it starts.
///
/// Rex's address for this pane is kept from then too: once the pane is
/// closing, Rex no longer says which session its block belongs to.
#[derive(Clone, Debug)]
struct Undock {
    rex: axel_rex::RexEnv,
    stream: Option<String>,
    usage: Option<String>,
    origin: Option<String>,
}

impl Undock {
    fn find() -> Option<Undock> {
        let env = axel_rex::detect()?;
        let (stream, usage) = super::ui::composer_neighbours(&axel_rex::Rex::new(env.clone()));
        let origin = std::env::var(super::ui::ORIGIN_BLOCK_ENV).ok().filter(|o| !o.is_empty());
        Some(Undock { rex: env, stream, usage, origin })
    }

    fn run(&self) {
        let rex = axel_rex::Rex::new(self.rex.clone());
        super::ui::undock(&rex, self.stream.as_deref(), self.usage.as_deref(), self.origin.as_deref());
    }
}

/// This pane's terminal counterpart instead, saying why.
fn fall_back(thread_id: &str, view: Pane, why: &str) -> Result<()> {
    eprintln!("axel: {}", t!("{why}; showing it as terminal text", why = why));
    Display::Terminal.panes().run(thread_id, view)
}

/// `axel web serve THREAD --view VIEW`: the pane's server alone, with its address on
/// stdout, until interrupted.  For a desktop browser, and for tests.
pub fn serve(thread_id: &str, view: Pane) -> Result<()> {
    let backend = ThreadBackend::open(thread_id, view)?;
    let running = server::start(Arc::new(backend), view, "")?;
    println!("{}", running.url()?);
    // Flushing makes the address readable by whoever started us; if stdout
    // is gone there is no one to read it either.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // Until interrupted: serving is all this command does.
    loop {
        std::thread::park();
    }
}

/// The thread a pane shows, and what the page may do to it.
struct ThreadBackend {
    thread_id: String,
    view: Pane,
    owner_pk: String,
    /// For loading, which polls.  Actions open a context of their own on
    /// the connection's thread (a `Ctx` holds Rex's runner, which stays on
    /// one thread), so they never wait behind a poll.
    reader: Mutex<axel_store::Store>,
    updates: Option<Mutex<UpdateState>>,
}

struct UpdateState {
    receiver: std::sync::mpsc::Receiver<Option<String>>,
    hint: Option<String>,
}

impl UpdateState {
    fn current(&mut self) -> Option<String> {
        match self.receiver.try_recv() {
            Ok(hint) => self.hint = hint,
            // Optional background checks may not have answered, or may have stopped.
            Err(std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        self.hint.clone()
    }
}

impl ThreadBackend {
    fn open(thread_id: &str, view: Pane) -> Result<ThreadBackend> {
        let ctx = Ctx::open()?;
        let thread = ctx.store.get_thread(thread_id)?.with_context(|| t!("no thread {id}", id = thread_id))?;
        let owner_pk = ctx.owner()?.public().to_base58();
        let reader = axel_store::Store::open_default().context(t!("opening ~/.axel/axel.db"))?;
        let updates = (view == Pane::Composer).then(|| Mutex::new(UpdateState { receiver: axel_tui::start_update_check(), hint: None }));
        Ok(ThreadBackend { thread_id: thread.id, view, owner_pk, reader: Mutex::new(reader), updates })
    }

    fn send(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        let text = body["text"].as_str().map(str::trim).unwrap_or_default();
        let files = attachments::from_message(body)?;
        if text.is_empty() && files.is_empty() {
            bail!("{}", t!("nothing to send"));
        }
        let text = if text.is_empty() { "Please look at the attached files." } else { text };
        if text.len() > MESSAGE_BYTES_MAX {
            bail!("{}", t!("that message is longer than {max} bytes", max = MESSAGE_BYTES_MAX));
        }
        let steer = body["steer"].as_bool().unwrap_or(false);
        let data = axel_tui::load_thread_data(&ctx.store, &self.owner_pk, &self.thread_id)?;
        match axel_tui::thread::dispatch(text, &data) {
            axel_tui::thread::Dispatch::Prompt { session_id, text } => {
                let reply = super::ui::send_to_session(ctx, &session_id, SendCommand::Prompt { text, attachments: files, steer })?;
                reply_json(reply)
            }
            axel_tui::thread::Dispatch::Summon { agent, text } => {
                super::ui::summon(ctx, &self.thread_id, &agent, &text, &files)?;
                Ok(json!({ "note": t!("summoning @{agent}…", agent = agent) }))
            }
            axel_tui::thread::Dispatch::NoTarget => {
                bail!("{}", t!("no agent is on this thread yet: start with @name and what you want done"))
            }
        }
    }

    /// What the controls page picked: asked about first (`confirmed` false),
    /// as the terminal pane asks, then carried out.  Closing, or an action
    /// after which the pane has nothing left to show, closes its layer.
    fn control(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        let data = super::manage::load(&ctx.store, &self.thread_id, None)?;
        let outcome = controls::parse_outcome(&body["outcome"], &data).context(t!("that isn't something the controls offer"))?;
        let ask = !body["confirmed"].as_bool().unwrap_or(false) && !matches!(outcome, Outcome::Close | Outcome::Rename(_));
        if ask {
            return Ok(json!({ "confirm": axel_tui::manage::confirm_question(&outcome), "danger": matches!(outcome, Outcome::HardReset) }));
        }
        let note = super::manage::act(ctx, &self.thread_id, &outcome)?;
        match note {
            Some(note) if !matches!(outcome, Outcome::Archive) => Ok(json!({ "note": note })),
            _ => {
                super::ui::close_own_layer(ctx);
                Ok(json!({ "close": true }))
            }
        }
    }

    fn cycle_mode(&self, ctx: &Ctx, body: &Value) -> Result<Value> {
        use axel_store::RuntimeMode;
        let session_id = self.own_session(ctx, body)?;
        let data = axel_tui::load_thread_data(&ctx.store, &self.owner_pk, &self.thread_id)?;
        let session = data.live_sessions.iter().find(|s| s.id == session_id && s.managed)
            .context("no managed session to switch modes on")?;
        let mode = match session.mode {
            RuntimeMode::Supervised => RuntimeMode::Edits,
            RuntimeMode::Edits => RuntimeMode::Plan,
            RuntimeMode::Plan => RuntimeMode::Autopilot,
            RuntimeMode::Autopilot => RuntimeMode::Supervised,
        };
        reply_json(super::ui::send_to_session(ctx, &session_id, SendCommand::SetMode { mode })?)?;
        Ok(json!({ "mode": mode.as_str() }))
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
        if self.view == Pane::Controls {
            return Ok(controls::controls(&super::manage::load(&store, &self.thread_id, None)?));
        }
        let data = axel_tui::load_thread_data(&store, &self.owner_pk, &self.thread_id)?;
        let usage = axel_tui::load_usage_data(&store, &self.thread_id)?;
        let mut value = view::thread(&data, &usage);
        value["version"] = json!(env!("CARGO_PKG_VERSION"));
        value["update_hint"] = match &self.updates {
            Some(updates) => json!(updates.lock().map_err(|_| anyhow::anyhow!("update state lock poisoned"))?.current()),
            None => Value::Null,
        };
        Ok(value)
    }

    fn marker(&self) -> Option<(i64, i64)> {
        // Can't tell: the poll then loads in full, which is correct, only slower.
        self.reader.lock().ok().and_then(|store| store.change_marker().ok())
    }

    fn act(&self, action: Action, body: &Value) -> Result<Value> {
        let ctx = Ctx::open()?;
        match action {
            Action::Send => self.send(&ctx, body),
            Action::Attach => attachments::attach(body),
            Action::CycleMode => self.cycle_mode(&ctx, body),
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
            Action::Control => self.control(&ctx, body),
            Action::OpenControls => {
                let rex = ctx.rex.as_ref().context(t!("the thread controls need Rex"))?;
                super::ui::open_controls(rex, &self.thread_id);
                Ok(json!({ "ok": true }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retina_pane_draws_at_twice_the_pixels() {
        assert_eq!(ratio_for_cell_height(17.0), 1.0, "a 1x display, a 13-point font");
        assert_eq!(ratio_for_cell_height(34.0), 2.0, "the same on Retina");
        assert_eq!(ratio_for_cell_height(26.0), 1.5, "a 1.5x display");
        assert_eq!(ratio_for_cell_height(3.0), DEVICE_PIXEL_RATIO_MIN, "a terminal reporting nonsense");
        assert_eq!(ratio_for_cell_height(400.0), DEVICE_PIXEL_RATIO_MAX);
    }
}
