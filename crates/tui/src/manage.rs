//! Thread controls: a floating pane over the thread, like the launcher, with
//! the thread's numbers on top (its sessions, what each spent, its context
//! fill, the last hour) and the things you do to a thread below: reset it
//! to a fresh context, move it to a host, settle, archive, stop, rename.
//! Pure state here ([`ManageView`]); the caller loads the data and carries
//! out the [`Outcome`].

use std::time::Duration;

use anyhow::Result;
use axel_i18n::t;
use axel_store::{AgentColor, ThreadState};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;
use crate::util::{ellipsize, format_tokens, pad_end, pad_start, text_width};

const POLL: Duration = Duration::from_millis(60);

/// Everything the pane shows, loaded by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct ManageData {
    pub thread_id: String,
    pub title: String,
    pub state: ThreadState,
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

impl Default for ManageData {
    /// An open thread with nothing on it yet.
    fn default() -> ManageData {
        ManageData {
            thread_id: String::new(),
            title: String::new(),
            state: ThreadState::Open,
            age: String::new(),
            projects: Vec::new(),
            sessions: Vec::new(),
            spark: Vec::new(),
            tokens: 0,
            cost_usd: 0.0,
            turns: 0,
            tool_calls: 0,
            files_changed: 0,
            agents: Vec::new(),
            hosts: Vec::new(),
            note: None,
        }
    }
}

/// What a session is doing, as the pane shows it: its own state, refined
/// by its last status report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionStatus {
    Working,
    #[default]
    Idle,
    Blocked,
    Done,
    Error,
    Paused,
    Ended,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Working => "working",
            SessionStatus::Idle => "idle",
            SessionStatus::Blocked => "blocked",
            SessionStatus::Done => "done",
            SessionStatus::Error => "error",
            SessionStatus::Paused => "paused",
            SessionStatus::Ended => "ended",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRow {
    pub agent: String,
    pub color: Option<AgentColor>,
    /// `claude/opus`.
    pub runs: String,
    pub state: SessionStatus,
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
    /// Last hour, oldest first, at most 60 one-minute token buckets.
    pub spark: Vec<i64>,
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
    StopAgent,
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
            Action::StopAgent => 't',
            Action::Rename => 'n',
            Action::HardReset => 'h',
        }
    }

    /// The action's name where it leaves the program (the web page).
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Reset => "reset",
            Action::Move => "move",
            Action::Settle => "settle",
            Action::Reopen => "reopen",
            Action::Archive => "archive",
            Action::Stop => "stop",
            Action::StopAgent => "agents",
            Action::Rename => "rename",
            Action::HardReset => "hard_reset",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::Reset => t!("Reset"),
            Action::Move => t!("Move"),
            Action::Settle => t!("Settle"),
            Action::Reopen => t!("Reopen"),
            Action::Archive => t!("Archive"),
            Action::Stop => t!("Stop all"),
            Action::StopAgent => t!("Agents"),
            Action::Rename => t!("Rename"),
            Action::HardReset => t!("Hard reset"),
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
    /// End only this agent's live sessions on the current thread.
    StopAgent { agent: String },
    PauseAgent { agent: String },
    ResumeAgent { agent: String },
    RemoveAgent { agent: String },
    Rename(String),
    /// End every session and relaunch the panes; no agent, no recap.
    HardReset,
}

/// Where the pane is: the menu, or a step of an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Menu,
    /// Which agent, for reset, move, or stop.
    PickAgent { action: Action, idx: usize },
    /// Which host, for move.
    PickHost { agent: String, idx: usize },
    AgentActions { agent: String, idx: usize },
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
        out.push(if self.data.state == ThreadState::Done { Action::Reopen } else { Action::Settle });
        if self.data.state != ThreadState::Archived {
            out.push(Action::Archive);
        }
        if !self.data.agents.is_empty() { out.push(Action::StopAgent); }
        if self.data.sessions.iter().any(|s| s.live) { out.push(Action::Stop); }
        out.push(Action::Rename);
        out.push(Action::HardReset);
        out
    }

    /// One line on what a row does, for this thread.
    pub fn describe(&self, action: Action) -> String {
        let agent = self.data.agents.first().cloned().unwrap_or_else(|| t!("the agent").into());
        match action {
            Action::Reset => t!("a fresh context for {agent}: a new session with a recap of the thread, nothing else carried over", agent = agent),
            Action::Move => t!("run {agent} on another machine: {hosts}", agent = agent, hosts = self.data.hosts.join(", ")),
            Action::Settle => t!("mark it done; it stays listed, dimmed").into(),
            Action::Reopen => t!("back to open").into(),
            Action::Archive => t!("stop its sessions and hide it (axel thread reopen brings it back)").into(),
            Action::StopAgent => t!("pause, resume, kill, or remove an agent from this thread").into(),
            Action::Stop if self.live_count() == 1 => t!("end 1 live session").into(),
            Action::Stop => t!("end {n} live sessions", n = self.live_count()),
            Action::Rename => t!("change the title (the window follows)").into(),
            Action::HardReset => t!("end every session and relaunch the panes; no agent, no recap: the next message starts from scratch").into(),
        }
    }

    fn live_count(&self) -> usize {
        self.data.sessions.iter().filter(|s| s.live).count()
    }

    /// What can be done to `agent` on this thread: a label, its key, and the outcome.
    pub fn agent_actions(&self, agent: &str) -> Vec<(&'static str, char, Outcome)> {
        let mut actions = Vec::new();
        let live: Vec<_> = self.data.sessions.iter().filter(|s| s.agent == agent && s.live).collect();
        let name = || agent.to_string();
        if live.iter().any(|s| s.state != SessionStatus::Paused) { actions.push((t!("Pause"), 'p', Outcome::PauseAgent { agent: name() })); }
        if live.iter().any(|s| s.state == SessionStatus::Paused) { actions.push((t!("Resume"), 'r', Outcome::ResumeAgent { agent: name() })); }
        if !live.is_empty() { actions.push((t!("Kill"), 'x', Outcome::StopAgent { agent: name() })); }
        actions.push((t!("Remove from thread"), 'd', Outcome::RemoveAgent { agent: name() }));
        actions
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
            Step::Menu => self.menu_key(key.code),
            Step::PickAgent { action, idx } => {
                self.pick_agent_key(key.code, action, idx);
                None
            }
            Step::PickHost { agent, idx } => {
                self.pick_host_key(key.code, agent, idx);
                None
            }
            Step::AgentActions { agent, idx } => {
                self.agent_actions_key(key.code, agent, idx);
                None
            }
            Step::Confirm(outcome) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.step = Step::Menu;
                    Some(outcome)
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.step = Step::Menu;
                    None
                }
                _ => None,
            },
            Step::Rename(text) => self.rename_key(key.code, text),
        }
    }

    fn menu_key(&mut self, code: KeyCode) -> Option<Outcome> {
        let actions = self.actions();
        match code {
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
        None
    }

    fn pick_agent_key(&mut self, code: KeyCode, action: Action, idx: usize) {
        let candidates = self.data.agents.clone();
        match code {
            KeyCode::Esc => self.step = Step::Menu,
            KeyCode::Up | KeyCode::Char('k') => self.step = Step::PickAgent { action, idx: idx.saturating_sub(1) },
            KeyCode::Down | KeyCode::Char('j') => self.step = Step::PickAgent { action, idx: (idx + 1).min(candidates.len().saturating_sub(1)) },
            KeyCode::Enter => {
                if let Some(agent) = candidates.get(idx) { self.after_agent(action, agent.clone()); }
            }
            _ => {}
        }
    }

    fn pick_host_key(&mut self, code: KeyCode, agent: String, idx: usize) {
        // Only entered with more than one host to pick from (see `after_agent`).
        assert!(idx < self.data.hosts.len(), "the host picked is listed");
        match code {
            KeyCode::Esc => self.step = Step::Menu,
            KeyCode::Up | KeyCode::Char('k') => self.step = Step::PickHost { agent, idx: idx.saturating_sub(1) },
            KeyCode::Down | KeyCode::Char('j') => self.step = Step::PickHost { agent, idx: (idx + 1).min(self.data.hosts.len() - 1) },
            KeyCode::Enter => self.step = Step::Confirm(Outcome::Move { agent, host: self.data.hosts[idx].clone() }),
            _ => {}
        }
    }

    fn agent_actions_key(&mut self, code: KeyCode, agent: String, idx: usize) {
        let actions = self.agent_actions(&agent);
        match code {
            KeyCode::Esc => self.step = Step::Menu,
            KeyCode::Up | KeyCode::Char('k') => self.step = Step::AgentActions { agent, idx: idx.saturating_sub(1) },
            KeyCode::Down | KeyCode::Char('j') => self.step = Step::AgentActions { agent, idx: (idx + 1).min(actions.len().saturating_sub(1)) },
            KeyCode::Enter => if let Some((_, _, outcome)) = actions.get(idx) { self.step = Step::Confirm(outcome.clone()); },
            KeyCode::Char(c) => if let Some((_, _, outcome)) = actions.iter().find(|(_, key, _)| *key == c) { self.step = Step::Confirm(outcome.clone()); },
            _ => {}
        }
    }

    fn rename_key(&mut self, code: KeyCode, mut text: String) -> Option<Outcome> {
        match code {
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
        }
        None
    }

    fn start(&mut self, action: Action) {
        match action {
            Action::Reset | Action::Move | Action::StopAgent => {
                let candidates = self.data.agents.clone();
                if candidates.len() > 1 {
                    self.step = Step::PickAgent { action, idx: 0 };
                } else if let Some(agent) = candidates.first().cloned() {
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
            Action::StopAgent => self.step = Step::AgentActions { agent, idx: 0 },
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
    let d = &v.data;
    let mut rows = Rows { frame, area, y: area.y };
    render_title(&mut rows, d, width);
    rows.push(Line::from(Span::styled("─".repeat(width), theme::dim())));
    render_sessions(&mut rows, d, width);
    render_totals(&mut rows, d, width);
    rows.push(Line::from(Span::styled("─".repeat(width), theme::dim())));
    render_step(&mut rows, v, width);
    // The footer owns its row, whatever the menu left there.
    let mut footer = footer_line(v);
    let footer_width = footer.width();
    if footer_width < width {
        footer.spans.push(Span::raw(" ".repeat(width - footer_width)));
    }
    let footer_y = area.y + area.height.saturating_sub(1);
    rows.put(footer_y, footer);
}

/// Lines written down the pane, one row each; past its bottom they are dropped.
struct Rows<'a, 'f> {
    frame: &'a mut Frame<'f>,
    area: Rect,
    y: u16,
}

impl Rows<'_, '_> {
    fn push(&mut self, line: Line) {
        self.put(self.y, line);
        self.y = self.y.saturating_add(1);
    }

    fn put(&mut self, y: u16, line: Line) {
        if y < self.area.y + self.area.height {
            self.frame.render_widget(Paragraph::new(line), Rect { x: self.area.x, y, width: self.area.width, height: 1 });
        }
    }
}

/// The title left, state · age · sessions right; the projects under it.
fn render_title(rows: &mut Rows, d: &ManageData, width: usize) {
    let sessions = if d.sessions.len() == 1 { t!("1 session").to_string() } else { t!("{n} sessions", n = d.sessions.len()) };
    let right = format!("{} · {} · {sessions}", d.state.as_str(), d.age);
    let title_width = width.saturating_sub(text_width(&right) + 2);
    rows.push(Line::from(vec![
        Span::styled(pad_end(&ellipsize(&d.title, title_width), title_width), theme::accent_bold()),
        Span::styled(format!("  {right}"), theme::dim()),
    ]));
    if !d.projects.is_empty() {
        rows.push(Line::from(Span::styled(d.projects.join(", "), theme::dim())));
    }
}

/// A column of the sessions table, after the agent and what it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionColumn {
    State,
    Turns,
    Tools,
    Tokens,
    Cost,
    Context,
    Age,
}

impl SessionColumn {
    /// Left to right; each shows only while it still fits.
    const ALL: [SessionColumn; 7] = [Self::Tokens, Self::Cost, Self::Context, Self::State, Self::Turns, Self::Tools, Self::Age];

    fn label(self) -> &'static str {
        match self {
            Self::State => t!("state"),
            Self::Turns => t!("turns"),
            Self::Tools => t!("tools"),
            Self::Tokens => t!("tokens"),
            Self::Cost => t!("cost"),
            Self::Context => t!("ctx"),
            Self::Age => t!("for"),
        }
    }

    fn width_cols(self) -> usize {
        match self {
            Self::State | Self::Cost | Self::Age => 8,
            Self::Turns | Self::Tools => 6,
            Self::Tokens => 7,
            // "上下文" takes six cells.
            Self::Context => 6,
        }
    }

    /// The cell for one session, and its style.
    fn cell(self, s: &SessionRow) -> (String, Style) {
        match self {
            Self::State => {
                let st = match (&s.host, s.live) {
                    (Some(h), true) => format!("{}@{h}", s.state.as_str()),
                    _ => s.state.as_str().to_string(),
                };
                (st, if s.live { theme::accent() } else { theme::dim() })
            }
            Self::Turns => (s.turns.to_string(), Style::default()),
            Self::Tools => (s.tool_calls.to_string(), Style::default()),
            Self::Tokens => (format_tokens(s.tokens), theme::bold()),
            Self::Cost => (format!("${:.2}", s.cost_usd), theme::dim()),
            Self::Context => match s.context {
                Some((used, size)) if size > 0 => {
                    let pct = (used as f64 / size as f64 * 100.0).round();
                    (format!("{pct:.0}%"), if pct >= 80.0 { theme::danger() } else if pct >= 60.0 { theme::accent() } else { Style::default() })
                }
                _ => (String::new(), Style::default()),
            },
            Self::Age => (duration_label(s.duration_secs), theme::dim()),
        }
    }
}

/// Sessions: columns drop from the right as the width shrinks.
fn render_sessions(rows: &mut Rows, d: &ManageData, width: usize) {
    let name_width = d.sessions.iter().map(|s| text_width(&s.agent)).max().unwrap_or(5).clamp(5, 12);
    let runs_width = d.sessions.iter().map(|s| text_width(&s.runs)).max().unwrap_or(0).min(18);
    // Six cells keep a chart legible; narrower panes prioritize the figures.
    const CHART_COLS_MIN: usize = 6;
    let chart_reserved = if width >= 70 { CHART_COLS_MIN + 1 } else { 0 };
    let table_width = width.saturating_sub(chart_reserved);
    let mut used = name_width + 1 + runs_width;
    let mut shown: Vec<SessionColumn> = Vec::new();
    for column in SessionColumn::ALL {
        if used + 1 + column.width_cols() <= table_width {
            shown.push(column);
            used += 1 + column.width_cols();
        }
    }
    let mut header = vec![Span::raw(" ".repeat(name_width + 1 + runs_width))];
    for column in &shown {
        let (label, w) = (column.label(), column.width_cols());
        header.push(Span::styled(format!(" {}", pad_start(label, w)), theme::dim()));
    }
    let chart_width = width.saturating_sub(used + 1).min(30);
    let chart_width = if chart_reserved > 0 { chart_width } else { 0 };
    let peak = d.sessions.iter().map(|s| crate::view::chart::peak(&s.spark, chart_width)).max().unwrap_or(0);
    if chart_width > 0 {
        header.push(Span::styled(" 1h", theme::dim()));
    }
    rows.push(Line::from(header));
    for s in &d.sessions {
        let mut spans = vec![
            Span::styled(pad_end(&ellipsize(&s.agent.to_uppercase(), name_width), name_width), Style::default().fg(theme::agent_color(s.color))),
            Span::styled(format!(" {}", pad_end(&ellipsize(&s.runs, runs_width), runs_width)), theme::dim()),
        ];
        for column in &shown {
            let (text, style) = column.cell(s);
            let w = column.width_cols();
            spans.push(Span::styled(format!(" {}", pad_start(&ellipsize(&text, w), w)), style));
        }
        if chart_width > 0 {
            spans.push(Span::raw(" "));
            spans.extend(crate::view::chart::activity(&s.spark, chart_width, peak, Style::default().fg(theme::agent_color(s.color))));
        }
        rows.push(Line::from(spans));
    }
}

/// The thread as a whole, and the last hour.
fn render_totals(rows: &mut Rows, d: &ManageData, width: usize) {
    let spark_width = width.saturating_sub(42).clamp(8, 30);
    let peak = crate::view::chart::peak(&d.spark, spark_width);
    let files = if d.files_changed > 0 { format!(" · {}", t!("{n} files changed", n = d.files_changed)) } else { String::new() };
    let numbers = t!("{turns} turns · {calls} tool calls", turns = d.turns, calls = d.tool_calls);
    let mut spans = vec![
        Span::styled(t!("{n} tokens", n = format_tokens(d.tokens)), theme::bold()),
        Span::styled(format!(" ${:.2} · {numbers}{files}  ", d.cost_usd), theme::dim()),
    ];
    spans.extend(crate::view::chart::activity(&d.spark, spark_width, peak, theme::accent()));
    rows.push(Line::from(spans));
}

/// The menu, or the step in progress.
fn render_step(rows: &mut Rows, v: &ManageView, width: usize) {
    let pick = |sel: bool, text: String| Line::from(Span::styled(format!("{} {text}", if sel { "›" } else { " " }), if sel { theme::accent_bold() } else { Style::default() }));
    match &v.step {
        Step::PickAgent { action, idx } => {
            rows.push(Line::from(Span::styled(t!("{action} which agent?", action = action.label()), theme::accent_bold())));
            for (i, a) in v.data.agents.iter().enumerate() {
                rows.push(pick(i == *idx, a.clone()));
            }
        }
        Step::AgentActions { agent, idx } => {
            rows.push(Line::from(Span::styled(t!("{agent} · agent controls", agent = agent), theme::accent_bold())));
            for (i, (label, key, outcome)) in v.agent_actions(agent).iter().enumerate() {
                let destructive = matches!(outcome, Outcome::StopAgent { .. } | Outcome::RemoveAgent { .. });
                let style = if destructive { theme::danger() } else if i == *idx { theme::accent_bold() } else { Style::default() };
                rows.push(Line::from(Span::styled(format!("{} {key} {label}", if i == *idx { "›" } else { " " }), style)));
            }
        }
        Step::PickHost { agent, idx } => {
            rows.push(Line::from(Span::styled(t!("move {agent} where?", agent = agent), theme::accent_bold())));
            for (i, h) in v.data.hosts.iter().enumerate() {
                rows.push(pick(i == *idx, h.clone()));
            }
        }
        _ => render_menu(rows, v, width),
    }
}

fn render_menu(rows: &mut Rows, v: &ManageView, width: usize) {
    const LABEL_WIDTH: usize = 11;
    for (i, a) in v.actions().iter().enumerate() {
        let sel = i == v.cursor && v.step == Step::Menu;
        let marker = if sel { "› " } else { "  " };
        let desc = ellipsize(&v.describe(*a), width.saturating_sub(LABEL_WIDTH + 6));
        let (label_style, desc_style) = match (a.is_destructive(), sel) {
            (true, true) => (theme::danger().add_modifier(ratatui::style::Modifier::BOLD), theme::danger()),
            (true, false) => (theme::danger(), theme::dim()),
            (false, true) => (theme::accent_bold(), Style::default()),
            (false, false) => (theme::bold(), theme::dim()),
        };
        rows.push(Line::from(vec![
            Span::styled(marker, if a.is_destructive() { theme::danger() } else { theme::accent() }),
            Span::styled(format!("{} ", a.key()), theme::dim()),
            Span::styled(pad_end(a.label(), LABEL_WIDTH), label_style),
            Span::styled(desc, desc_style),
        ]));
    }
}

/// The question to confirm, the title being typed, or the keys for the step.
fn footer_line(v: &ManageView) -> Line<'static> {
    match &v.step {
        Step::Confirm(o) => {
            let style = if matches!(o, Outcome::HardReset) { theme::danger().add_modifier(ratatui::style::Modifier::BOLD) } else { theme::accent_bold() };
            Line::from(Span::styled(format!("{}  y/n", confirm_question(o)), style))
        }
        Step::Rename(text) => Line::from(vec![Span::styled(format!("{} ", t!("title:")), theme::accent()), Span::raw(text.clone()), Span::styled("▏", theme::accent())]),
        Step::PickAgent { .. } | Step::PickHost { .. } | Step::AgentActions { .. } => Line::from(Span::styled(t!("↑↓ choose  ⏎ pick  Esc back"), theme::dim())),
        Step::Menu => match &v.data.note {
            Some(n) => Line::from(Span::styled(n.clone(), theme::accent())),
            None => Line::from(Span::styled(t!("↑↓ choose  ⏎ go  Esc close"), theme::dim())),
        },
    }
}

/// The question asked before `o` is carried out.
pub fn confirm_question(o: &Outcome) -> String {
    match o {
        Outcome::Reset { agent } => t!("reset: end the thread's sessions and start {agent} fresh with a recap?", agent = agent),
        Outcome::Move { agent, host } => t!("move {agent} to {host}?", agent = agent, host = host),
        Outcome::Settle => t!("settle this thread?").into(),
        Outcome::Reopen => t!("reopen this thread?").into(),
        Outcome::Archive => t!("archive this thread: stop its sessions and hide it?").into(),
        Outcome::Stop => t!("stop every live session?").into(),
        Outcome::StopAgent { agent } => t!("kill {agent}'s live sessions on this thread?", agent = agent),
        Outcome::PauseAgent { agent } => t!("pause {agent} in place, including its running tools?", agent = agent),
        Outcome::ResumeAgent { agent } => t!("resume {agent}'s paused work?", agent = agent),
        Outcome::RemoveAgent { agent } => t!("stop {agent} and remove it from this thread? Its history is kept.", agent = agent),
        Outcome::Rename(title) => t!("rename to {title}?", title = title),
        Outcome::HardReset => t!("hard reset: end every session, relaunch the panes, start nobody; the conversation is not carried over?").into(),
        Outcome::Close => String::new(),
    }
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
            state: ThreadState::Open,
            age: "2d".into(),
            projects: vec!["axel".into()],
            sessions: vec![
                SessionRow { agent: "opus".into(), runs: "claude/opus".into(), state: SessionStatus::Idle, live: true, turns: 12, tool_calls: 140, tokens: 34_300_000, cost_usd: 57.86, context: Some((62, 100)), duration_secs: 5 * 3600 + 12 * 60, ..Default::default() },
                SessionRow { agent: "fable".into(), runs: "claude/fable".into(), state: SessionStatus::Ended, live: false, turns: 4, tool_calls: 37, tokens: 3_100_000, cost_usd: 4.2, duration_secs: 41 * 60, ..Default::default() },
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
        assert_eq!(v.actions(), vec![Action::Reset, Action::Move, Action::Settle, Action::Archive, Action::StopAgent, Action::Stop, Action::Rename, Action::HardReset]);
        let mut done = data();
        done.state = ThreadState::Done;
        done.hosts.clear();
        done.sessions.iter_mut().for_each(|s| s.live = false);
        assert_eq!(ManageView::new(done).actions(), vec![Action::Reset, Action::Reopen, Action::Archive, Action::StopAgent, Action::Rename, Action::HardReset], "no hosts: no move; nothing live: no stop");
    }

    #[test]
    fn agent_controls_offer_pause_resume_kill_and_remove_as_appropriate() {
        let mut v = ManageView::new(data());
        v.handle_key(key(KeyCode::Char('t')));
        assert_eq!(v.step, Step::PickAgent { action: Action::StopAgent, idx: 0 });
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(v.step, Step::AgentActions { agent: "opus".into(), idx: 0 });
        v.handle_key(key(KeyCode::Char('p')));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::PauseAgent { agent: "opus".into() }));
        v.data.sessions[0].state = SessionStatus::Paused;
        let actions = v.agent_actions("opus");
        assert_eq!(actions.iter().map(|a| a.0).collect::<Vec<_>>(), vec!["Resume", "Kill", "Remove from thread"]);
        v.step = Step::AgentActions { agent: "opus".into(), idx: 0 };
        v.handle_key(key(KeyCode::Char('x')));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::StopAgent { agent: "opus".into() }));
        assert_eq!(v.agent_actions("fable").iter().map(|a| a.0).collect::<Vec<_>>(), vec!["Remove from thread"]);
        v.step = Step::AgentActions { agent: "fable".into(), idx: 0 };
        v.handle_key(key(KeyCode::Char('d')));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::RemoveAgent { agent: "fable".into() }));
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
        assert!(text[4].starts_with("OPUS") && text[4].contains("claude/opus") && text[4].contains("idle") && text[4].contains("34M") && text[4].contains("62%") && text[4].contains("5h 12m"), "{:?}", text[4]);
        assert!(text[6].starts_with("37M tokens") && text[6].contains("16 turns") && text[6].contains("9 files changed"), "{:?}", text[6]);
        assert!(text[8].starts_with("› r Reset") && text[9].starts_with("  m Move"), "{:?}", &text[8..10]);
        assert!(text[15].starts_with("  h Hard reset"), "last, apart: {:?}", text[15]);
        let red = term.backend().buffer()[(4, 15)].style().fg;
        assert_eq!(red, Some(theme::DANGER), "drawn in red");
        assert_eq!(text[16], "↑↓ choose  ⏎ go  Esc close");
        // Narrow: the rightmost columns go first.
        let mut term = Terminal::new(TestBackend::new(44, 17)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let row: String = (0..44).map(|x| term.backend().buffer()[(x, 3)].symbol().to_string()).collect();
        assert!(row.contains("tokens") && row.contains("cost") && row.contains("ctx") && !row.contains("turns"), "{row:?}");
    }
}
