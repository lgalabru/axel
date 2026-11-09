//! The run loop: owns the terminal, polls events, routes them to the active
//! screen's state machine, applies the resulting actions through `glue` and
//! the `Deps` callbacks.

use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;

use crate::dashboard::{DashAction, Dashboard};
use crate::glue;
use crate::inbox::{Inbox, InboxAction};
use crate::launcher::{Action, Launcher, Step, Workspace};
use crate::picker::{PickAction, ThreadPicker};
use crate::thread::{ThreadAction, ThreadView};
use crate::util::now_secs;
use crate::view;
use crate::{Deps, LaunchRequest, Outcome, Screen};

const POLL: Duration = Duration::from_millis(60);
const REFRESH: Duration = Duration::from_secs(1);
const THREAD_REFRESH_LIVE: Duration = Duration::from_millis(300);
const THREAD_REFRESH_IDLE: Duration = Duration::from_secs(2);

pub fn run(deps: Deps, initial: Screen) -> Result<Outcome> {
    // Resolve the owner key before taking over the terminal: the keychain may prompt.
    let owner_pk = glue::owner_pubkey(deps.keystore)?;

    let mut terminal = ratatui::init();
    // Mouse capture: without it the terminal turns the wheel into ↑/↓ on the
    // alternate screen, which the composer reads as prompt history.
    // Focus reports: the recent-threads pane only highlights its row while it has focus.
    let _ = execute!(stdout(), EnableBracketedPaste, EnableMouseCapture, EnableFocusChange);
    // Shift+Enter in the thread view's composer is a line break.
    let keys = crate::util::enhance_keys();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::util::restore_keys(keys);
        let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture, DisableFocusChange);
        ratatui::restore();
        previous_hook(info);
    }));

    let result = event_loop(&mut terminal, &deps, &owner_pk, initial);

    crate::util::restore_keys(keys);
    let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture, DisableFocusChange);
    ratatui::restore();
    let _ = std::panic::take_hook();
    result
}

struct App<'a> {
    deps: &'a Deps<'a>,
    owner_pk: &'a str,
    screen: Screen,
    dashboard: Dashboard,
    launcher: Launcher,
    inbox: Inbox,
    thread: Option<ThreadView>,
    picker: ThreadPicker,
    usage: crate::model::UsageData,
    /// Whether this terminal has focus (from focus reports).  The picker
    /// starts without it: the prompt above is focused first.
    focused: bool,
    /// Opened straight onto the launcher (the floating prompt): leaving it closes Axel.
    prompt_first: bool,
    /// Last size asked of `Deps::fit_prompt`.
    prompt_size: Option<crate::PromptSize>,
    /// Settling the prompt pane on the rows it needs (see [`App::settle_prompt`]).
    prompt_fit: Option<PromptFit>,
    last_refresh: Instant,
}

impl<'a> App<'a> {
    fn new(deps: &'a Deps<'a>, owner_pk: &'a str, initial: Screen) -> Self {
        let now = now_secs();
        let dashboard = Dashboard::new(glue::load_dashboard(deps.store, owner_pk, now).unwrap_or_default());
        let inbox = Inbox::new(glue::load_inbox(deps.store, owner_pk).unwrap_or_default());
        let launcher = Self::fresh_launcher(deps);
        let prompt_first = initial == Screen::Launcher;
        let focused = initial != Screen::Threads;
        let mut app = Self {
            deps,
            owner_pk,
            screen: Screen::Dashboard,
            dashboard,
            launcher,
            inbox,
            thread: None,
            picker: ThreadPicker { can_manage: deps.manage_thread.is_some(), ..Default::default() },
            usage: Default::default(),
            focused,
            prompt_first,
            prompt_size: None,
            prompt_fit: None,
            last_refresh: Instant::now(),
        };
        app.open(initial);
        app
    }

    fn fresh_launcher(deps: &Deps) -> Launcher {
        let projects = glue::load_projects(deps.store).unwrap_or_default();
        let agents = glue::load_agents(deps.store).unwrap_or_default();
        let harnesses = deps.harnesses.iter().map(|h| h.to_string()).collect();
        let mut l = Launcher::new(deps.cwd.clone(), projects, agents, harnesses);
        l.pane_grows = deps.fit_prompt.is_some();
        l
    }

    fn open(&mut self, screen: Screen) {
        match &screen {
            Screen::Launcher if !matches!(self.screen, Screen::Launcher) => {
                // Entering the launcher for a new thread: drop any spawn-mode launcher and
                // re-read projects/agents so the lists are fresh.
                if self.launcher.spawn.is_some() {
                    self.launcher = Self::fresh_launcher(self.deps);
                } else {
                    let projects = glue::load_projects(self.deps.store).unwrap_or_default();
                    let agents = glue::load_agents(self.deps.store).unwrap_or_default();
                    self.launcher.set_projects(projects);
                    self.launcher.set_agents(agents);
                }
            }
            Screen::Thread(tid) => {
                let same = self.thread.as_ref().is_some_and(|t| &t.thread_id == tid);
                if !same {
                    let data = glue::load_thread(self.deps.store, self.owner_pk, tid, now_secs()).unwrap_or_default();
                    self.thread = Some(ThreadView::new(tid, data));
                }
            }
            _ => {}
        }
        self.screen = screen;
        self.refresh(true);
    }

    fn refresh_interval(&self) -> Duration {
        match (&self.screen, &self.thread) {
            (Screen::Thread(_), Some(t)) if t.is_live() => THREAD_REFRESH_LIVE,
            (Screen::Thread(_), _) => THREAD_REFRESH_IDLE,
            _ => REFRESH,
        }
    }

    fn refresh(&mut self, force: bool) {
        if !force && self.last_refresh.elapsed() < self.refresh_interval() {
            return;
        }
        self.last_refresh = Instant::now();
        let now = now_secs();
        match &self.screen {
            Screen::Dashboard => {
                match glue::load_dashboard(self.deps.store, self.owner_pk, now) {
                    Ok(d) => self.dashboard.set_data(d),
                    Err(e) => self.dashboard.set_flash(format!("refresh failed: {e}"), now),
                }
                self.dashboard.tick(now);
            }
            Screen::Inbox => {
                match glue::load_inbox(self.deps.store, self.owner_pk) {
                    Ok(d) => self.inbox.set_data(d),
                    Err(e) => self.inbox.set_flash(format!("refresh failed: {e}"), now),
                }
                self.inbox.tick(now);
            }
            Screen::Thread(tid) => {
                if let Some(t) = self.thread.as_mut() {
                    match glue::load_thread(self.deps.store, self.owner_pk, tid, now) {
                        Ok(d) => t.set_data(d),
                        Err(e) => t.set_status(format!("error: refresh failed: {e}")),
                    }
                }
            }
            Screen::Threads => match glue::load_recent_threads(self.deps.store, self.owner_pk, now) {
                Ok(t) => self.picker.set_threads(t),
                Err(e) => eprintln!("axel: refresh failed: {e}"),
            },
            Screen::Usage => match glue::load_usage(self.deps.store, now) {
                Ok(u) => self.usage = u,
                Err(e) => eprintln!("axel: refresh failed: {e}"),
            },
            Screen::Launcher => {}
        }
    }

    /// The prompt pane grows with the prompt's lines (and for the full
    /// launch form), and shrinks back, so the recent threads below stay visible.
    fn fit_prompt(&mut self) {
        let Some(fit) = self.deps.fit_prompt else { return };
        let size = if matches!(self.screen, Screen::Launcher) && self.launcher.step != Step::Title {
            crate::PromptSize::Form
        } else {
            let width = crossterm::terminal::size().map_or(80, |(w, _)| w);
            crate::PromptSize::Rows(view::launcher_view::prompt_pane_rows(&self.launcher, width))
        };
        if self.prompt_size != Some(size) {
            self.prompt_size = Some(size);
            // What Rex's pane chrome cost last time (rows asked beyond those
            // needed) goes into every later ask, growing or shrinking, so the
            // pane lands on its size at once instead of overshooting.
            let extra = self.prompt_fit.map_or(0, |f| f.asked.saturating_sub(f.want));
            let (size, fit_next) = match size {
                crate::PromptSize::Rows(want) => {
                    let asked = want + extra;
                    (crate::PromptSize::Rows(asked), Some(PromptFit { want, asked, tries: 0, at: Instant::now() }))
                }
                crate::PromptSize::Form => (size, self.prompt_fit.map(|f| PromptFit { tries: 4, ..f })),
            };
            self.prompt_fit = fit_next;
            fit(size);
        }
    }

    /// The pane Rex gave the prompt can come out short of what was asked
    /// (splits are sized by ratio, and pane title bars take rows too): ask
    /// again for what's missing, a few times, so every line shows.
    fn settle_prompt(&mut self, height: u16) {
        let Some(fit) = self.deps.fit_prompt else { return };
        let Some(f) = self.prompt_fit.as_mut() else { return };
        let Some(ask) = f.correction(height) else { return };
        fit(crate::PromptSize::Rows(ask));
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        let now = now_secs();
        match &self.screen {
            Screen::Dashboard => view::dashboard_view::render(frame, &self.dashboard, now),
            Screen::Launcher => view::launcher_view::render(frame, &self.launcher, now),
            Screen::Inbox => view::inbox_view::render(frame, &self.inbox, now),
            Screen::Threads => view::picker_view::render(frame, &self.picker, self.focused, now),
            Screen::Usage => view::usage_view::render(frame, &self.usage, frame.area()),
            Screen::Thread(_) => {
                if let Some(t) = &self.thread {
                    view::thread_view::render(frame, t, now);
                }
            }
        }
    }

    /// Returns `Some(outcome)` when the UI should exit.
    fn handle(&mut self, ev: Event) -> Option<Outcome> {
        crate::util::log_key(&ev);
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => match &self.screen {
                Screen::Dashboard => {
                    let action = self.dashboard.handle_key(key, now_secs());
                    self.on_dashboard(action)
                }
                Screen::Launcher => {
                    let action = self.launcher.handle_key(key);
                    let out = self.on_launcher(action);
                    self.sync_runtime_label();
                    self.fit_prompt();
                    out
                }
                Screen::Inbox => {
                    let action = self.inbox.handle_key(key);
                    self.on_inbox(action)
                }
                Screen::Thread(_) => {
                    let action = self.thread.as_mut().map(|t| t.handle_key(key)).unwrap_or(ThreadAction::Back);
                    self.on_thread(action)
                }
                Screen::Threads => {
                    self.focused = true;
                    let action = self.picker.handle_key(key);
                    self.on_picker(action)
                }
                // Nothing to pick: Esc or q closes it.
                Screen::Usage => matches!(key.code, KeyCode::Esc | KeyCode::Char('q')).then_some(Outcome::Quit),
            },
            Event::FocusGained => {
                self.focused = true;
                None
            }
            Event::FocusLost => {
                self.focused = false;
                None
            }
            Event::Mouse(m) => {
                let delta = match m.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => return None,
                };
                match &self.screen {
                    Screen::Thread(_) => {
                        if let Some(t) = self.thread.as_mut() {
                            t.scroll(delta);
                        }
                        None
                    }
                    // The wheel only moves the cursor here; ↑ on the first row would leave the pane.
                    Screen::Threads => {
                        let code = if delta < 0 { KeyCode::Up } else { KeyCode::Down };
                        if !(code == KeyCode::Up && self.picker.sel == 0) {
                            self.picker.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
                        }
                        None
                    }
                    // Lists elsewhere: the wheel moves the selection, one row per notch.
                    _ => {
                        let code = if delta < 0 { KeyCode::Up } else { KeyCode::Down };
                        self.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
                    }
                }
            }
            Event::Paste(text) => {
                match &self.screen {
                    Screen::Launcher => {
                        self.launcher.handle_paste(&text);
                        self.fit_prompt();
                    }
                    Screen::Thread(_) => {
                        if let Some(t) = self.thread.as_mut() {
                            t.handle_paste(&text);
                        }
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }

    fn on_dashboard(&mut self, action: DashAction) -> Option<Outcome> {
        match action {
            DashAction::None => None,
            DashAction::Quit => Some(Outcome::Quit),
            DashAction::OpenLauncher => {
                self.open(Screen::Launcher);
                None
            }
            DashAction::OpenInbox => {
                self.open(Screen::Inbox);
                None
            }
            DashAction::OpenThread(tid) => {
                let opened = self.deps.open_thread.is_some_and(|f| f(&tid).is_ok());
                if !opened {
                    self.open(Screen::Thread(tid));
                }
                None
            }
            DashAction::FocusSession(sid) => Some(Outcome::FocusSession(sid)),
            DashAction::Spawn { thread_id, title } => {
                let agents = glue::load_agents(self.deps.store).unwrap_or_default();
                let harnesses = self.deps.harnesses.iter().map(|h| h.to_string()).collect();
                self.launcher = Launcher::for_spawn(self.deps.cwd.clone(), agents, harnesses, &thread_id, &title);
                self.screen = Screen::Launcher;
                None
            }
            DashAction::Refresh => {
                self.refresh(true);
                None
            }
        }
    }

    /// Fill the launcher's "runtime: …" line from the chosen agent's harness.
    fn sync_runtime_label(&mut self) {
        if self.launcher.step == crate::launcher::Step::Summary {
            let label = self.launcher.chosen_agent_row().map(|a| (self.deps.runtime_label)(&a.harness));
            self.launcher.runtime_label = label;
        } else {
            self.launcher.runtime_label = None;
        }
    }

    fn on_launcher(&mut self, action: Action) -> Option<Outcome> {
        match action {
            Action::None => None,
            Action::Quit => Some(Outcome::Quit),
            Action::Back => {
                if self.launcher.spawn.is_some() {
                    self.launcher = Self::fresh_launcher(self.deps);
                } else if self.prompt_first {
                    return Some(Outcome::Quit);
                }
                self.open(Screen::Dashboard);
                None
            }
            Action::FocusThreads => {
                if let Some(focus) = self.deps.focus_peer {
                    focus();
                }
                None
            }
            Action::OpenInbox => {
                self.open(Screen::Inbox);
                None
            }
            Action::CreateAgent(spec) => {
                match glue::create_agent(self.deps.store, self.deps.keystore, &spec) {
                    Ok(row) => {
                        let agents = glue::load_agents(self.deps.store).unwrap_or_else(|_| vec![row.clone()]);
                        self.launcher.agent_created(agents, &row.pubkey);
                    }
                    Err(e) => self.launcher.agent_create_failed(format!("{e:#}")),
                }
                None
            }
            Action::Launch(plan) => Some(Outcome::Launch(LaunchRequest {
                title: plan.title,
                projects: plan.projects,
                agent_pubkey: plan.agent_pubkey,
                workspace: match plan.workspace {
                    Workspace::InPlace => axel_store::WorkspaceKind::InPlace,
                    Workspace::Worktree => axel_store::WorkspaceKind::Worktree,
                    Workspace::Scratch => axel_store::WorkspaceKind::Scratch,
                },
                branch: plan.branch,
                thread_id: plan.thread_id,
                attachments: plan.attachments,
            })),
        }
    }

    fn on_picker(&mut self, action: PickAction) -> Option<Outcome> {
        match action {
            PickAction::None => None,
            PickAction::Quit => Some(Outcome::Quit),
            PickAction::FocusPrompt => {
                if let Some(focus) = self.deps.focus_peer {
                    focus();
                    self.focused = false;
                }
                None
            }
            PickAction::Manage(tid, op) => {
                let title = self.picker.threads.iter().find(|t| t.id == tid).map(|t| t.title.clone()).unwrap_or_default();
                if let Some(manage) = self.deps.manage_thread {
                    self.picker.notice = Some(match manage(&tid, op) {
                        Ok(()) => {
                            let done = match op {
                                crate::ThreadOp::Settle => "Settled",
                                crate::ThreadOp::Reopen => "Reopened",
                                crate::ThreadOp::Archive => "Archived",
                                crate::ThreadOp::Delete => "Deleted",
                            };
                            format!("{done} “{title}”")
                        }
                        Err(e) => format!("{e:#}"),
                    });
                }
                self.refresh(true);
                None
            }
            PickAction::Open(tid) => {
                let opened = self.deps.open_thread.is_some_and(|f| f(&tid).is_ok());
                if !opened {
                    self.open(Screen::Thread(tid));
                }
                None
            }
        }
    }

    fn on_inbox(&mut self, action: InboxAction) -> Option<Outcome> {
        let now = now_secs();
        let allow = matches!(action, InboxAction::Allow(_));
        match action {
            InboxAction::None => None,
            InboxAction::Quit => Some(Outcome::Quit),
            InboxAction::OpenLauncher => {
                self.open(Screen::Launcher);
                None
            }
            InboxAction::Refresh => {
                self.refresh(true);
                None
            }
            InboxAction::Allow(req) | InboxAction::Deny(req) => {
                match glue::reply_permission(self.deps.store, self.deps.keystore, &req, allow) {
                    Ok(()) => self.inbox.set_flash(
                        format!("{} {}'s request", if allow { "Allowed" } else { "Denied" }, req.agent_name),
                        now,
                    ),
                    Err(e) => self.inbox.set_flash(format!("reply failed: {e:#}"), now),
                }
                self.refresh(true);
                None
            }
            InboxAction::MarkRead => {
                match glue::mark_all_read(self.deps.store, self.owner_pk, &self.inbox.data) {
                    Ok(true) => self.inbox.set_flash("Marked all read", now),
                    Ok(false) => self.inbox.set_flash("Nothing to mark", now),
                    Err(e) => self.inbox.set_flash(format!("mark read failed: {e:#}"), now),
                }
                self.refresh(true);
                None
            }
        }
    }

    fn on_thread(&mut self, action: ThreadAction) -> Option<Outcome> {
        match action {
            // The thread controls are a floating pane of the composer; inline, nothing opens.
            ThreadAction::Manage | ThreadAction::OpenTerminal => None,
            ThreadAction::None => None,
            ThreadAction::Quit => Some(Outcome::Quit),
            ThreadAction::Back => {
                self.open(Screen::Dashboard);
                None
            }
            ThreadAction::OpenLog(sid) => Some(Outcome::OpenLog(sid)),
            ThreadAction::Send { session_id, cmd } => {
                let reply = (self.deps.send)(&session_id, cmd).map_err(|e| format!("{e:#}"));
                if let Some(t) = self.thread.as_mut() {
                    t.on_reply(reply);
                }
                self.refresh(true);
                None
            }
            ThreadAction::Summon { thread_id, agent, text, attachments } => {
                let reply = (self.deps.summon)(&thread_id, &agent, &text, &attachments).map_err(|e| format!("{e:#}"));
                if let Some(t) = self.thread.as_mut() {
                    match reply {
                        Ok(_session_id) => t.on_reply(Ok(crate::thread::SendReply::Ok)),
                        Err(e) => t.on_reply(Err(e)),
                    }
                }
                self.refresh(true);
                None
            }
        }
    }
}

/// Asking Rex for prompt rows until the pane has them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PromptFit {
    /// Rows the prompt needs.
    want: u16,
    /// Rows last asked of Rex.
    asked: u16,
    tries: u8,
    at: Instant,
}

/// Wait this long after a resize before checking it took.
const PROMPT_SETTLE: Duration = Duration::from_millis(250);

impl PromptFit {
    /// What to ask next when the pane is `height` rows: the rows still
    /// missing on top of the last ask; `None` once it fits (or after a few tries).
    fn correction(&mut self, height: u16) -> Option<u16> {
        if height >= self.want || self.tries >= 4 || self.at.elapsed() < PROMPT_SETTLE {
            return None;
        }
        self.asked += self.want - height;
        self.tries += 1;
        self.at = Instant::now();
        Some(self.asked)
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, deps: &Deps, owner_pk: &str, initial: Screen) -> Result<Outcome> {
    let mut app = App::new(deps, owner_pk, initial);
    loop {
        let size = terminal.size()?;
        app.dashboard.compact =
            view::dashboard_view::mode_for(ratatui::layout::Rect::new(0, 0, size.width, size.height))
                == view::dashboard_view::Mode::Compact;
        app.settle_prompt(size.height);
        terminal.draw(|f| app.draw(f))?;
        if event::poll(POLL)? {
            let ev = event::read()?;
            if let Some(outcome) = app.handle(ev) {
                return Ok(outcome);
            }
            // Drain any further queued events before redrawing (fast typing / paste).
            while event::poll(Duration::ZERO)? {
                let ev = event::read()?;
                if let Some(outcome) = app.handle(ev) {
                    return Ok(outcome);
                }
            }
        }
        app.refresh(false);
    }
}
#[cfg(test)]
mod prompt_fit_tests {
    use super::*;

    #[test]
    fn what_the_chrome_costs_carries_over_to_the_next_ask() {
        let past = Instant::now() - Duration::from_secs(1);
        // Growing to 8 rows needed an ask of 9.
        let grown = PromptFit { want: 8, asked: 9, tries: 1, at: past };
        let extra = grown.asked.saturating_sub(grown.want);
        // Deleting a line: 7 needed, so 8 asked straight away; the pane never
        // comes out short, so there is nothing to correct.
        let mut shrunk = PromptFit { want: 7, asked: 7 + extra, tries: 0, at: past };
        assert_eq!(shrunk.asked, 8);
        assert_eq!(shrunk.correction(7), None, "it fits at once");
    }

    #[test]
    fn a_short_pane_is_asked_again_for_the_missing_rows() {
        let past = Instant::now() - Duration::from_secs(1);
        let mut f = PromptFit { want: 8, asked: 8, tries: 0, at: past };
        assert_eq!(f.correction(7), Some(9), "a row short: ask for one more");
        f.at = past;
        assert_eq!(f.correction(7), Some(10), "still short: one more again");
        f.at = past;
        assert_eq!(f.correction(8), None, "it fits");
        assert_eq!(f.correction(12), None, "taller is fine (the pane never shrinks below its first size)");
        let mut f = PromptFit { want: 8, asked: 8, tries: 0, at: Instant::now() };
        assert_eq!(f.correction(5), None, "the resize may not have landed yet");
        let mut f = PromptFit { want: 8, asked: 8, tries: 4, at: past };
        assert_eq!(f.correction(5), None, "gives up after a few tries");
    }
}

