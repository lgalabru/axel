//! Thread controls: a floating pane over the thread, like the launcher, with
//! the thread's numbers on top (its sessions, what each spent, its context
//! fill, the last hour) and the things you do to a thread below: reset it
//! to a fresh context, move it to a host, settle, archive, stop, rename.
//! Pure state here ([`ManageView`]); the caller loads the data and carries
//! out the [`Outcome`].

use std::time::Duration;

use anyhow::Result;
use axel_store::AgentColor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;
use crate::util::{ellipsize, format_tokens};

const POLL: Duration = Duration::from_millis(60);

/// Everything the pane shows, loaded by the caller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManageData {
    pub thread_id: String,
    pub title: String,
    /// `open`, `done` or `archived`.
    pub state: String,
    /// `2d`, `41m`: since the thread was created.
    pub age: String,
    /// Project names, or none for a scratch thread.
    pub projects: Vec<String>,
    /// Newest first.
    pub sessions: Vec<SessionRow>,
    /// Tokens per minute over the last hour, oldest first, every session together.
    pub spark: Vec<i64>,
    pub tokens: i64,
    pub cost_usd: f64,
    pub turns: i64,
    pub tool_calls: i64,
    /// Files changed over every turn with checkpoints.
    pub files_changed: i64,
    /// Agents that worked on the thread, latest first: the candidates for reset and move.
    pub agents: Vec<String>,
    /// Hosts this machine knows, by label.
    pub hosts: Vec<String>,
    /// What just happened, for the footer.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRow {
    pub agent: String,
    pub color: Option<AgentColor>,
    /// `claude/opus`.
    pub runs: String,
    /// `working`, `idle`, `blocked`, `ended`…
    pub state: String,
    pub live: bool,
    /// Where it runs when not here.
    pub host: Option<String>,
    pub turns: i64,
    pub tool_calls: i64,
    pub tokens: i64,
    pub cost_usd: f64,
    /// `(used, size)` of its context window, when it reports it.
    pub context: Option<(i64, i64)>,
    pub duration_secs: i64,
}

/// What a row of the menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Reset,
    Move,
    Settle,
    Reopen,
    Archive,
    Stop,
    Rename,
    /// Everything ends, nobody restarts: the next message starts from scratch.
    HardReset,
}

impl Action {
    /// The key that picks the row from the menu.
    pub fn key(self) -> char {
        match self {
            Action::Reset => 'r',
            Action::Move => 'm',
            Action::Settle => 's',
            Action::Reopen => 'o',
            Action::Archive => 'a',
            Action::Stop => 'x',
            Action::Rename => 'n',
            Action::HardReset => 'h',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::Reset => "Reset",
            Action::Move => "Move",
            Action::Settle => "Settle",
            Action::Reopen => "Reopen",
            Action::Archive => "Archive",
            Action::Stop => "Stop",
            Action::Rename => "Rename",
            Action::HardReset => "Hard reset",
        }
    }

    /// Drawn in red: it throws the conversation away.
    pub fn is_destructive(self) -> bool {
        matches!(self, Action::HardReset)
    }
}

/// What the pane asks its caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Close,
    /// End the thread's sessions and start `agent` again with a fresh context and a recap.
    Reset { agent: String },
    /// Run `agent` on `host` from now on, moving its session when it can.
    Move { agent: String, host: String },
    Settle,
    Reopen,
    Archive,
    Stop,
    Rename(String),
    /// End every session and relaunch the panes; no agent, no recap.
    HardReset,
}

/// Where the pane is: the menu, or a step of an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Menu,
    /// Which agent, for reset or move.
    PickAgent { action: Action, idx: usize },
    /// Which host, for move.
    PickHost { agent: String, idx: usize },
    /// `y` to go ahead.
    Confirm(Outcome),
    Rename(String),
}

#[derive(Clone, Debug)]
pub struct ManageView {
    pub data: ManageData,
    pub cursor: usize,
    pub step: Step,
}

impl ManageView {
    pub fn new(data: ManageData) -> Self {
        ManageView { data, cursor: 0, step: Step::Menu }
    }

    /// The rows of the menu, for this thread as it is.
    pub fn actions(&self) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.data.agents.is_empty() {
            out.push(Action::Reset);
            if !self.data.hosts.is_empty() {
                out.push(Action::Move);
            }
        }
        out.push(if self.data.state == "done" { Action::Reopen } else { Action::Settle });
        if self.data.state != "archived" {
            out.push(Action::Archive);
        }
        if self.data.sessions.iter().any(|s| s.live) {
            out.push(Action::Stop);
        }
        out.push(Action::Rename);
        out.push(Action::HardReset);
        out
    }

    /// One line on what a row does, for this thread.
    pub fn describe(&self, action: Action) -> String {
        let agent = self.data.agents.first().cloned().unwrap_or_else(|| "the agent".into());
        match action {
            Action::Reset => format!("a fresh context for {agent}: a new session with a recap of the thread, nothing else carried over"),
            Action::Move => format!("run {agent} on another machine: {}", self.data.hosts.join(", ")),
            Action::Settle => "mark it done; it stays listed, dimmed".into(),
            Action::Reopen => "back to open".into(),
            Action::Archive => "stop its sessions and hide it (axel thread reopen brings it back)".into(),
            Action::Stop => format!("end {} live session{}", self.live_count(), if self.live_count() == 1 { "" } else { "s" }),
            Action::Rename => "change the title (the window follows)".into(),
            Action::HardReset => "end every session and relaunch the panes; no agent, no recap: the next message starts from scratch".into(),
        }
    }

    fn live_count(&self) -> usize {
        self.data.sessions.iter().filter(|s| s.live).count()
    }

    /// Handle a key; `Some` when the caller has something to do.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Outcome::Close);
        }
        match self.step.clone() {
            Step::Menu => {
                let actions = self.actions();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Some(Outcome::Close),
                    KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(actions.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        let action = actions.get(self.cursor).copied()?;
                        self.start(action);
                    }
                    KeyCode::Char(c) => {
                        // Each row has a key of its own, shown beside it.
                        if let Some(a) = actions.iter().find(|a| a.key() == c.to_ascii_lowercase()) {
                            self.start(*a);
                        }
                    }
                    _ => {}
                }
            }
            Step::PickAgent { action, idx } => match key.code {
                KeyCode::Esc => self.step = Step::Menu,
                KeyCode::Up | KeyCode::Char('k') => self.step = Step::PickAgent { action, idx: idx.saturating_sub(1) },
                KeyCode::Down | KeyCode::Char('j') => self.step = Step::PickAgent { action, idx: (idx + 1).min(self.data.agents.len() - 1) },
                KeyCode::Enter => {
                    let agent = self.data.agents[idx].clone();
                    self.after_agent(action, agent);
                }
                _ => {}
            },
            Step::PickHost { agent, idx } => match key.code {
                KeyCode::Esc => self.step = Step::Menu,
                KeyCode::Up | KeyCode::Char('k') => self.step = Step::PickHost { agent, idx: idx.saturating_sub(1) },
                KeyCode::Down | KeyCode::Char('j') => self.step = Step::PickHost { agent, idx: (idx + 1).min(self.data.hosts.len() - 1) },
                KeyCode::Enter => self.step = Step::Confirm(Outcome::Move { agent, host: self.data.hosts[idx].clone() }),
                _ => {}
            },
            Step::Confirm(outcome) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.step = Step::Menu;
                    return Some(outcome);
                }
                KeyCode::Esc | KeyCode::Char('n') => self.step = Step::Menu,
                _ => {}
            },
            Step::Rename(mut text) => match key.code {
                KeyCode::Esc => self.step = Step::Menu,
                KeyCode::Enter => {
                    self.step = Step::Menu;
                    if !text.trim().is_empty() && text.trim() != self.data.title {
                        return Some(Outcome::Rename(text.trim().to_string()));
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    self.step = Step::Rename(text);
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.step = Step::Rename(text);
                }
                _ => {}
            },
        }
        None
    }

    fn start(&mut self, action: Action) {
        match action {
            Action::Reset | Action::Move => {
                if self.data.agents.len() > 1 {
                    self.step = Step::PickAgent { action, idx: 0 };
                } else if let Some(agent) = self.data.agents.first().cloned() {
                    self.after_agent(action, agent);
                }
            }
            Action::Settle => self.step = Step::Confirm(Outcome::Settle),
            Action::Reopen => self.step = Step::Confirm(Outcome::Reopen),
            Action::Archive => self.step = Step::Confirm(Outcome::Archive),
            Action::Stop => self.step = Step::Confirm(Outcome::Stop),
            Action::Rename => self.step = Step::Rename(self.data.title.clone()),
            Action::HardReset => self.step = Step::Confirm(Outcome::HardReset),
        }
    }

    fn after_agent(&mut self, action: Action, agent: String) {
        match action {
            Action::Move if self.data.hosts.len() > 1 => self.step = Step::PickHost { agent, idx: 0 },
            Action::Move => {
                let host = self.data.hosts[0].clone();
                self.step = Step::Confirm(Outcome::Move { agent, host });
            }
            _ => self.step = Step::Confirm(Outcome::Reset { agent }),
        }
    }
}

/// `5h 12m`, `41m`, `12s`.
pub fn duration_label(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// The pane, from the top of the frame: title, the sessions, the numbers,
/// the menu, the footer for the current step.
pub fn render(frame: &mut Frame, v: &ManageView) {
    let area = frame.area();
    let width = area.width as usize;
    let mut y = area.y;
    let line_at = |frame: &mut Frame, y: u16, line: Line| {
        if y < area.y + area.height {
            frame.render_widget(Paragraph::new(line), Rect { x: area.x, y, width: area.width, height: 1 });
        }
    };
    let d = &v.data;
    // Title line: the title left, state · age · sessions right.
    let right = format!("{} · {} · {} session{}", d.state, d.age, d.sessions.len(), if d.sessions.len() == 1 { "" } else { "s" });
    let title_w = width.saturating_sub(right.chars().count() + 2);
    line_at(
        frame,
        y,
        Line::from(vec![
            Span::styled(format!("{:<title_w$}", ellipsize(&d.title, title_w)), theme::accent_bold()),
            Span::styled(format!("  {right}"), theme::dim()),
        ]),
    );
    y += 1;
    if !d.projects.is_empty() {
        line_at(frame, y, Line::from(Span::styled(d.projects.join(", "), theme::dim())));
        y += 1;
    }
    line_at(frame, y, Line::from(Span::styled("─".repeat(width), theme::dim())));
    y += 1;
    // Sessions: columns drop from the right as the width shrinks.
    let name_w = d.sessions.iter().map(|s| s.agent.chars().count()).max().unwrap_or(5).clamp(5, 12);
    let runs_w = d.sessions.iter().map(|s| s.runs.chars().count()).max().unwrap_or(0).min(18);
    let cols: &[(&str, usize)] = &[("state", 8), ("turns", 6), ("tools", 6), ("tokens", 7), ("cost", 8), ("ctx", 5), ("for", 8)];
    let mut used = name_w + 1 + runs_w;
    let mut shown: Vec<(&str, usize)> = Vec::new();
    for (label, w) in cols {
        if used + 1 + w <= width {
            shown.push((label, *w));
            used += 1 + w;
        }
    }
    let mut header = vec![Span::raw(" ".repeat(name_w + 1 + runs_w))];
    for (label, w) in &shown {
        header.push(Span::styled(format!(" {label:>w$}"), theme::dim()));
    }
    line_at(frame, y, Line::from(header));
    y += 1;
    for s in &d.sessions {
        let mut spans = vec![
            Span::styled(format!("{:<name_w$}", ellipsize(&s.agent, name_w)), Style::default().fg(theme::agent_color(s.color))),
            Span::styled(format!(" {:<runs_w$}", ellipsize(&s.runs, runs_w)), theme::dim()),
        ];
        for (label, w) in &shown {
            let (text, style) = match *label {
                "state" => {
                    let st = match (&s.host, s.live) {
                        (Some(h), true) => format!("{}@{h}", s.state),
                        _ => s.state.clone(),
                    };
                    (st, if s.live { theme::accent() } else { theme::dim() })
                }
                "turns" => (s.turns.to_string(), Style::default()),
                "tools" => (s.tool_calls.to_string(), Style::default()),
                "tokens" => (format_tokens(s.tokens), theme::bold()),
                "cost" => (format!("${:.2}", s.cost_usd), theme::dim()),
                "ctx" => match s.context {
                    Some((u, n)) if n > 0 => {
                        let pct = (u as f64 / n as f64 * 100.0).round();
                        (format!("{pct:.0}%"), if pct >= 80.0 { theme::danger() } else if pct >= 60.0 { theme::accent() } else { Style::default() })
                    }
                    _ => (String::new(), Style::default()),
                },
                _ => (duration_label(s.duration_secs), theme::dim()),
            };
            spans.push(Span::styled(format!(" {:>w$}", ellipsize(&text, *w)), style));
        }
        line_at(frame, y, Line::from(spans));
        y += 1;
    }
    // The thread as a whole, and the last hour.
    let spark_w = width.saturating_sub(42).clamp(8, 30);
    let peak = crate::view::chart::peak(&d.spark, spark_w);
    let files = if d.files_changed > 0 { format!(" · {} files changed", d.files_changed) } else { String::new() };
    let mut spans = vec![
        Span::styled(format!("{} tokens", format_tokens(d.tokens)), theme::bold()),
        Span::styled(format!(" ${:.2} · {} turns · {} tool calls{files}  ", d.cost_usd, d.turns, d.tool_calls), theme::dim()),
    ];
    spans.extend(crate::view::chart::braille(&d.spark, spark_w, peak, theme::accent()));
    line_at(frame, y, Line::from(spans));
    y += 1;
    line_at(frame, y, Line::from(Span::styled("─".repeat(width), theme::dim())));
    y += 1;
    // The menu, or the step in progress.
    match &v.step {
        Step::PickAgent { action, idx } => {
            line_at(frame, y, Line::from(Span::styled(format!("{} which agent?", action.label()), theme::accent_bold())));
            y += 1;
            for (i, a) in d.agents.iter().enumerate() {
                let sel = i == *idx;
                line_at(frame, y, Line::from(Span::styled(format!("{} {a}", if sel { "›" } else { " " }), if sel { theme::accent_bold() } else { Style::default() })));
                y += 1;
            }
        }
        Step::PickHost { agent, idx } => {
            line_at(frame, y, Line::from(Span::styled(format!("move {agent} where?"), theme::accent_bold())));
            y += 1;
            for (i, h) in d.hosts.iter().enumerate() {
                let sel = i == *idx;
                line_at(frame, y, Line::from(Span::styled(format!("{} {h}", if sel { "›" } else { " " }), if sel { theme::accent_bold() } else { Style::default() })));
                y += 1;
            }
        }
        _ => {
            let actions = v.actions();
            let label_w = 10;
            for (i, a) in actions.iter().enumerate() {
                let sel = i == v.cursor && v.step == Step::Menu;
                let marker = if sel { "› " } else { "  " };
                let desc = ellipsize(&v.describe(*a), width.saturating_sub(label_w + 6));
                let (label_style, desc_style) = match (a.is_destructive(), sel) {
                    (true, true) => (theme::danger().add_modifier(ratatui::style::Modifier::BOLD), theme::danger()),
                    (true, false) => (theme::danger(), theme::dim()),
                    (false, true) => (theme::accent_bold(), Style::default()),
                    (false, false) => (theme::bold(), theme::dim()),
                };
                line_at(
                    frame,
                    y,
                    Line::from(vec![
                        Span::styled(marker, if a.is_destructive() { theme::danger() } else { theme::accent() }),
                        Span::styled(format!("{} ", a.key()), theme::dim()),
                        Span::styled(format!("{:<label_w$}", a.label()), label_style),
                        Span::styled(desc, desc_style),
                    ]),
                );
                y += 1;
            }
        }
    }
    // Footer.
    let footer_y = area.y + area.height.saturating_sub(1);
    let footer = match &v.step {
        Step::Confirm(o) => {
            let what = match o {
                Outcome::Reset { agent } => format!("reset: end the thread's sessions and start {agent} fresh with a recap?"),
                Outcome::Move { agent, host } => format!("move {agent} to {host}?"),
                Outcome::Settle => "settle this thread?".into(),
                Outcome::Reopen => "reopen this thread?".into(),
                Outcome::Archive => "archive this thread: stop its sessions and hide it?".into(),
                Outcome::Stop => "stop every live session?".into(),
                Outcome::Rename(t) => format!("rename to {t}?"),
                Outcome::HardReset => "hard reset: end every session, relaunch the panes, start nobody; the conversation is not carried over?".into(),
                Outcome::Close => String::new(),
            };
            let style = if matches!(o, Outcome::HardReset) { theme::danger().add_modifier(ratatui::style::Modifier::BOLD) } else { theme::accent_bold() };
            Line::from(Span::styled(format!("{what}  y/n"), style))
        }
        Step::Rename(text) => Line::from(vec![Span::styled("title: ", theme::accent()), Span::raw(text.clone()), Span::styled("▏", theme::accent())]),
        Step::PickAgent { .. } | Step::PickHost { .. } => Line::from(Span::styled("↑↓ choose  ⏎ pick  Esc back", theme::dim())),
        Step::Menu => match &d.note {
            Some(n) => Line::from(Span::styled(n.clone(), theme::accent())),
            None => Line::from(Span::styled("↑↓ choose  ⏎ go  Esc close", theme::dim())),
        },
    };
    // The footer owns its row, whatever the menu left there.
    let mut footer = footer;
    let w = footer.width();
    if w < width {
        footer.spans.push(Span::raw(" ".repeat(width - w)));
    }
    line_at(frame, footer_y, footer);
}

/// Run the pane until it has something for the caller.
pub fn run_manage(data: ManageData) -> Result<Outcome> {
    let mut v = ManageView::new(data);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<Outcome> {
        loop {
            terminal.draw(|f| render(f, &v))?;
            if event::poll(POLL)?
                && let Event::Key(key) = event::read()?
                && let Some(out) = v.handle_key(key)
            {
                return Ok(out);
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn data() -> ManageData {
        ManageData {
            thread_id: "T1".into(),
            title: "Ship it".into(),
            state: "open".into(),
            age: "2d".into(),
            projects: vec!["axel".into()],
            sessions: vec![
                SessionRow { agent: "opus".into(), runs: "claude/opus".into(), state: "idle".into(), live: true, turns: 12, tool_calls: 140, tokens: 34_300_000, cost_usd: 57.86, context: Some((62, 100)), duration_secs: 5 * 3600 + 12 * 60, ..Default::default() },
                SessionRow { agent: "fable".into(), runs: "claude/fable".into(), state: "ended".into(), live: false, turns: 4, tool_calls: 37, tokens: 3_100_000, cost_usd: 4.2, duration_secs: 41 * 60, ..Default::default() },
            ],
            spark: vec![0; 60],
            tokens: 37_400_000,
            cost_usd: 62.06,
            turns: 16,
            tool_calls: 177,
            files_changed: 9,
            agents: vec!["opus".into(), "fable".into()],
            hosts: vec!["mini".into()],
            note: None,
        }
    }

    #[test]
    fn the_menu_offers_what_fits_the_thread() {
        let v = ManageView::new(data());
        assert_eq!(v.actions(), vec![Action::Reset, Action::Move, Action::Settle, Action::Archive, Action::Stop, Action::Rename, Action::HardReset]);
        let mut done = data();
        done.state = "done".into();
        done.hosts.clear();
        done.sessions.iter_mut().for_each(|s| s.live = false);
        assert_eq!(ManageView::new(done).actions(), vec![Action::Reset, Action::Reopen, Action::Archive, Action::Rename, Action::HardReset], "no hosts: no move; nothing live: no stop");
    }

    #[test]
    fn reset_asks_which_agent_then_confirms() {
        let mut v = ManageView::new(data());
        assert_eq!(v.handle_key(key(KeyCode::Enter)), None, "Reset: two agents, so it asks");
        assert_eq!(v.step, Step::PickAgent { action: Action::Reset, idx: 0 });
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(v.step, Step::Confirm(Outcome::Reset { agent: "fable".into() }));
        assert_eq!(v.handle_key(key(KeyCode::Char('n'))), None, "n backs out");
        assert_eq!(v.step, Step::Menu);
        // Each row has a key; one host means no host question.
        v.handle_key(key(KeyCode::Char('m')));
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::Move { agent: "opus".into(), host: "mini".into() }));
        // Rename edits the title in place.
        v.handle_key(key(KeyCode::Char('n')));
        assert_eq!(v.step, Step::Rename("Ship it".into()));
        for _ in 0..3 {
            v.handle_key(key(KeyCode::Backspace));
        }
        for c in " now".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(v.handle_key(key(KeyCode::Enter)), Some(Outcome::Rename("Ship now".into())));
        // Hard reset is the last row, confirmed like the rest.
        v.handle_key(key(KeyCode::Char('h')));
        assert_eq!(v.step, Step::Confirm(Outcome::HardReset));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::HardReset));
        assert_eq!(v.handle_key(key(KeyCode::Esc)), Some(Outcome::Close));
    }

    #[test]
    fn the_pane_draws_the_sessions_and_the_menu() {
        let v = ManageView::new(data());
        let mut term = Terminal::new(TestBackend::new(90, 17)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let text: Vec<String> = (0..17).map(|y| (0..90).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
        assert!(text[0].starts_with("Ship it") && text[0].ends_with("open · 2d · 2 sessions"), "{:?}", text[0]);
        assert!(text[3].contains("state") && text[3].contains("tokens") && text[3].contains("ctx") && text[3].contains("for"), "{:?}", text[3]);
        assert!(text[4].starts_with("opus") && text[4].contains("claude/opus") && text[4].contains("idle") && text[4].contains("34M") && text[4].contains("62%") && text[4].contains("5h 12m"), "{:?}", text[4]);
        assert!(text[6].starts_with("37M tokens") && text[6].contains("16 turns") && text[6].contains("9 files changed"), "{:?}", text[6]);
        assert!(text[8].starts_with("› r Reset") && text[9].starts_with("  m Move"), "{:?}", &text[8..10]);
        assert!(text[14].starts_with("  h Hard reset"), "last, apart: {:?}", text[14]);
        let red = term.backend().buffer()[(4, 14)].style().fg;
        assert_eq!(red, Some(theme::DANGER), "drawn in red");
        assert_eq!(text[16], "↑↓ choose  ⏎ go  Esc close");
        // Narrow: the rightmost columns go first.
        let mut term = Terminal::new(TestBackend::new(44, 17)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let row: String = (0..44).map(|x| term.backend().buffer()[(x, 3)].symbol().to_string()).collect();
        assert!(row.contains("state") && row.contains("turns") && !row.contains("ctx"), "{row:?}");
    }
}
