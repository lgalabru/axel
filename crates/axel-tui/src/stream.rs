//! The thread as a plain terminal stream (the top pane of a split thread).
//!
//! No alternate screen, no raw mode, no mouse capture: finished rows are
//! printed once and become ordinary scrollback, so the terminal scrolls,
//! searches and selects them itself.  Only the **live tail** — rows still
//! streaming or running, a pending approval, the "working" line — is redrawn
//! in place at the bottom (cursor up + clear), the way inline CLIs do it.

use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::glue;
use crate::model::{ItemBody, ItemKind, ItemStatus, ThreadItem};
use crate::theme;
use crate::thread::{Row, ThreadView};
use crate::util::now_secs;
use crate::view::thread_view::row_lines;

const TICK: Duration = Duration::from_millis(250);
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// Follow `thread_id` until the process is killed (the pane closes with its window).
pub fn run_stream(store: &axel_store::Store, owner_pk: &str, thread_id: &str) -> Result<()> {
    let mut out = std::io::stdout();
    let data = glue::load_thread(store, owner_pk, thread_id, now_secs())?;
    let mut view = ThreadView::new(thread_id, data);
    // Inside Rex, each run of tool calls links to its details (see `ToolsLink`).
    let links = std::env::var("REX_SESSION").ok().filter(|v| !v.is_empty()).map(|session| crate::util::ToolsLink {
        thread: thread_id.to_string(),
        fold: String::new(),
        session,
        server: std::env::var("REX_SERVER").ok().filter(|v| !v.is_empty()),
    });
    let mut s = Stream { links, ..Default::default() };
    write!(out, "\x1b[?25l")?; // the cursor would blink under the live tail
    let mut width = WidthWatch::default();
    // Each agent's state, as OSC 7501 records of this pane: the agents' own
    // panes are minimized, so this is where Rex (and its Activity) sees them.
    let mut reported: Vec<AgentRecord> = Vec::new();
    let mut root_reported: Option<String> = None;
    loop {
        let now = agent_records(&view.data);
        let reports = status_reports(&reported, &now);
        if !reports.is_empty() {
            // An agent finished, or stopped to wait for you: the bell, as
            // Claude Code rings it (Rex plays its sound).
            if chime(&reported, &now) {
                out.write_all(b"\x07")?;
            }
            out.write_all(reports.as_bytes())?;
            reported = now;
        }
        // The thread as a whole, in the record with no id: what the tab shows.
        let root = root_report(&view.data.title, &reported);
        if root_reported.as_ref() != Some(&root) {
            out.write_all(root.as_bytes())?;
            root_reported = Some(root);
        }
        let (w, h) = crossterm::terminal::size().unwrap_or((100, 40));
        // Everything is wrapped to the pane's width once, when printed: print
        // only at a settled width, and when it settles on another, print it
        // all again (the old lines, and the tail redrawn over them, wouldn't fit).
        // A pane grown shorter (the composer took rows) than the live tail it
        // shows can't redraw that tail in place: the cursor can't climb past
        // the top. Print it all again, as for a new width.
        let tail_overflows = !s.tail_fits(h);
        let print = match width.on_tick(w) {
            Width::Same if tail_overflows => {
                write!(out, "\x1b[H\x1b[2J\x1b[3J{}\r\n", to_ansi(&header_line(&view)))?;
                s.forget();
                true
            }
            Width::Wait => false,
            Width::First => {
                // The pane starts with the login banner (`Last login: …`): clear it.
                write!(out, "\x1b[H\x1b[2J\x1b[3J{}\r\n", to_ansi(&header_line(&view)))?;
                true
            }
            Width::Reprint => {
                write!(out, "\x1b[H\x1b[2J\x1b[3J{}\r\n", to_ansi(&header_line(&view)))?;
                s.forget();
                true
            }
            Width::Same => true,
        };
        if print {
            let frame = s.frame(&view, w as usize, h as usize);
            if !frame.is_empty() {
                out.write_all(frame.as_bytes())?;
            }
        }
        out.flush()?;
        std::thread::sleep(TICK);
        s.tick += 1;
        if let Ok(d) = glue::load_thread(store, owner_pk, thread_id, now_secs()) {
            view.set_data(d);
        }
    }
}

/// One agent's OSC 7501 record in the thread's pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRecord {
    /// The record id: the agent's name, lowercased.
    pub id: String,
    pub state: axel_rex::osc::State,
    pub kind: Option<axel_rex::osc::Kind>,
}

/// A record per agent with a live session on the thread, from its status,
/// blocked records saying on what (a question or a permission).
pub fn agent_records(d: &crate::model::ThreadData) -> Vec<AgentRecord> {
    use crate::model::{ItemKind, ItemStatus, StatusState};
    use axel_rex::osc::{Kind, State};
    let mut out: Vec<AgentRecord> = Vec::new();
    for s in &d.live_sessions {
        let id = s.agent_name.to_lowercase();
        if out.iter().any(|r| r.id == id) {
            continue;
        }
        let status = d.agents.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(&s.agent_name)).map(|(_, _, st)| *st).unwrap_or(StatusState::Idle);
        let state = match status {
            StatusState::Working => State::Working,
            StatusState::Blocked => State::Blocked,
            StatusState::Done => State::Done,
            StatusState::Error => State::Error,
            StatusState::Idle => State::Idle,
        };
        let kind = (state == State::Blocked).then(|| {
            let pending = d.items.iter().find(|it| {
                it.status == Some(ItemStatus::Pending)
                    && matches!(it.kind, ItemKind::Question | ItemKind::Permission)
                    && it.agent.as_ref().is_some_and(|(n, _)| n.eq_ignore_ascii_case(&s.agent_name))
            });
            match pending.map(|it| it.kind) {
                Some(ItemKind::Question) => Kind::Question,
                _ => Kind::Permission,
            }
        });
        out.push(AgentRecord { id, state, kind });
    }
    out
}

/// The reports that bring Rex from `before` to `now`: changed records in
/// full (each report replaces its record), and `clear` for agents that left.
pub fn status_reports(before: &[AgentRecord], now: &[AgentRecord]) -> String {
    use axel_rex::osc::{Report, State, encode};
    let mut out = String::new();
    for r in now.iter().filter(|r| !before.contains(r)) {
        out.push_str(&encode(&Report { state: r.state, id: Some(&r.id), kind: r.kind, progress: None, app: Some("axel"), title: Some(&r.id), msg: None }));
    }
    for r in before.iter().filter(|r| !now.iter().any(|n| n.id == r.id)) {
        out.push_str(&encode(&Report { state: State::Clear, id: Some(&r.id), kind: None, progress: None, app: None, title: None, msg: None }));
    }
    out
}

/// An agent went from something else to done (finished) or blocked (waiting
/// for you). Agents already there when the pane opened don't count.
pub fn chime(before: &[AgentRecord], now: &[AgentRecord]) -> bool {
    use axel_rex::osc::State;
    now.iter().any(|r| {
        matches!(r.state, State::Done | State::Blocked) && before.iter().any(|b| b.id == r.id && b.state != r.state)
    })
}

/// The pane's root record: the most urgent agent state (blocked, error,
/// working, done, then idle), the thread's title, and each agent's state.
pub fn root_report(title: &str, agents: &[AgentRecord]) -> String {
    use axel_rex::osc::{Report, State, encode};
    let rank = |s: State| match s {
        State::Blocked => 4,
        State::Error => 3,
        State::Working => 2,
        State::Done => 1,
        _ => 0,
    };
    let top = agents.iter().max_by_key(|r| rank(r.state));
    let state = top.map_or(State::Idle, |r| r.state);
    let kind = top.and_then(|r| r.kind);
    let msg = agents.iter().map(|r| format!("{} {}", r.id, r.state.as_str())).collect::<Vec<_>>().join(" · ");
    encode(&Report { state, id: None, kind, progress: None, app: Some("axel"), title: Some(title), msg: (!msg.is_empty()).then_some(msg.as_str()) })
}

/// What the pane's width says to do this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Not settled yet: print nothing.
    Wait,
    /// Settled for the first time: print the thread.
    First,
    /// Unchanged: carry on.
    Same,
    /// Settled on a new width: clear the pane and print it all again.
    Reprint,
}

/// A width counts once it has held for this many ticks.
const SETTLE_TICKS: u8 = 2;

/// Tracks the pane's width across ticks (see [`Width`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct WidthWatch {
    /// The width everything on screen was wrapped to.
    drawn: Option<u16>,
    /// A width seen, and for how many ticks in a row.
    seen: Option<(u16, u8)>,
}

impl WidthWatch {
    pub fn on_tick(&mut self, w: u16) -> Width {
        if self.drawn == Some(w) {
            self.seen = None;
            return Width::Same;
        }
        let ticks = match self.seen {
            Some((sw, n)) if sw == w => n + 1,
            _ => 1,
        };
        self.seen = Some((w, ticks));
        if ticks < SETTLE_TICKS {
            return Width::Wait;
        }
        self.seen = None;
        let first = self.drawn.is_none();
        self.drawn = Some(w);
        if first { Width::First } else { Width::Reprint }
    }
}

#[derive(Default)]
struct Stream {
    /// Row keys already printed to scrollback.
    committed: HashSet<String>,
    /// Rows the live tail occupies right now (to erase before redrawing): a
    /// line wider than the pane wraps onto more than one.
    live_lines: usize,
    /// The live tail as last drawn, to skip identical redraws.
    last_live: Vec<String>,
    tick: usize,
    /// Where fold lines link to (the fold filled in per row); `None` outside Rex.
    links: Option<crate::util::ToolsLink>,
}

impl Stream {
    /// The live tail on screen still fits a pane `height` rows tall (with the
    /// row the cursor sits on).
    fn tail_fits(&self, height: u16) -> bool {
        self.live_lines + 2 <= usize::from(height)
    }

    /// Nothing is on screen any more (it was cleared): print it all next frame.
    fn forget(&mut self) {
        self.committed.clear();
        self.live_lines = 0;
        self.last_live.clear();
    }

    /// A row as terminal output; a run of tool calls links to its details.
    fn row_ansi(&self, v: &ThreadView, row: &Row, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = render_row(v, row, width).iter().map(to_ansi).collect();
        if let (Row::Fold(idx), Some(base), Some(first)) = (row, &self.links, lines.first_mut()) {
            let link = crate::util::ToolsLink { fold: v.data.items[idx[0]].id.clone(), ..base.clone() };
            *first = crate::util::hyperlink(&link.url(), first);
        }
        lines
    }

    /// The bytes to write this tick: erase the old tail, print newly finished
    /// rows, draw the new tail.  Empty when nothing changed.
    fn frame(&mut self, v: &ThreadView, width: usize, height: usize) -> String {
        let cols = width.max(1);
        let width = width.saturating_sub(1).max(20);
        let rows = v.rows();
        let items = &v.data.items;
        let working = v.is_working();
        let mut commit: Vec<String> = Vec::new();
        let mut live_rows: Vec<&Row> = Vec::new();
        let mut blocked = false;
        for (i, row) in rows.iter().enumerate() {
            let key = row.key(items);
            if self.committed.contains(&key) {
                continue;
            }
            // Commit in order: once one row is still moving, the rest wait.
            if !blocked && is_settled(row, items, i + 1 == rows.len(), working) {
                commit.extend(self.row_ansi(v, row, width));
                self.committed.insert(key);
            } else {
                blocked = true;
                live_rows.push(row);
            }
        }
        let mut live: Vec<String> = live_rows.iter().flat_map(|r| self.row_ansi(v, r, width)).collect();
        if working {
            let who = v.data.agents.iter().find(|a| a.2 == crate::model::StatusState::Working).map(|a| a.0.clone()).unwrap_or_else(|| "agent".into());
            let frame = SPINNER[self.tick % SPINNER.len()];
            live.push(to_ansi(&Line::from(vec![
                Span::styled(format!("{frame} "), theme::accent()),
                Span::styled(format!("{who} is working…"), theme::dim()),
            ])));
        }
        // Never draw a tail taller than the pane: cursor-up can't go past the
        // top, and what it can't reach stays on screen, a copy per redraw.
        // Counted in rows, as the terminal shows them: a line wider than the
        // pane (a wide character, a tab) takes more than one.
        let cap = height.saturating_sub(2).max(1);
        let rows_of = |lines: &[String]| -> usize { lines.iter().map(|l| screen_rows(l, cols)).sum() };
        if rows_of(&live) > cap {
            let mut hidden = 0;
            // Keep the newest lines that fit, with a row for the note.
            while hidden < live.len() && rows_of(&live[hidden..]) + usize::from(cap > 1) > cap {
                hidden += 1;
            }
            live.drain(..hidden);
            if cap > 1 {
                live.insert(0, to_ansi(&Line::from(Span::styled(format!("  … {hidden} more lines above"), theme::dim()))));
            }
        }
        if commit.is_empty() && live == self.last_live {
            return String::new();
        }
        let mut buf = String::new();
        if self.live_lines > 0 {
            // To the first line of the old tail, then clear to the end of the screen.
            buf.push_str(&format!("\x1b[{}F\x1b[J", self.live_lines));
        }
        for l in commit.iter().chain(live.iter()) {
            buf.push_str(l);
            buf.push_str("\r\n");
        }
        self.live_lines = rows_of(&live);
        self.last_live = live;
        buf
    }
}

/// How many rows a printed line takes in a pane `cols` wide: its width as
/// the terminal shows it (escape sequences take none, a tab runs to the next
/// stop), wrapped. An empty line still takes its row.
fn screen_rows(line: &str, cols: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut width: usize = 0;
    let mut breaks: usize = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // OSC (a link), up to its `ESC \\` or BEL.
                Some(']') => {
                    while let Some(d) = chars.next() {
                        if d == '\x07' || (d == '\x1b' && chars.peek() == Some(&'\\')) {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // CSI and the rest, up to the final letter.
                _ => {
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            },
            '\t' => width += 8 - width % 8,
            // A row of its own (to_ansi never prints one; this is the safety net).
            '\n' => {
                breaks += width.div_ceil(cols.max(1)).max(1);
                width = 0;
            }
            c => width += c.width().unwrap_or(0),
        }
    }
    width.div_ceil(cols.max(1)).max(1) + breaks
}

/// A row can go to scrollback once nothing in it will change: not the last row
/// while an agent works, no running tool or streaming text, no open approval.
fn is_settled(row: &Row, items: &[ThreadItem], last: bool, working: bool) -> bool {
    if last && working {
        return false;
    }
    // Running, unless its turn is over: a tool call the turn ended without
    // (cancelled mid-call) never finishes, and would hold every row after it
    // in the live tail for good.
    let moving = |i: usize| matches!(items[i].status, Some(ItemStatus::Running | ItemStatus::Pending)) && !turn_over(items, i);
    match row {
        Row::Prose(i) | Row::Thought(i) | Row::Choice(i) => !moving(*i),
        Row::Tool { idx, .. } => !moving(*idx),
        Row::Fold(v) => !v.iter().any(|i| moving(*i)),
    }
}

/// Item `i`'s turn has ended: its session ended the turn, or went on to a later one.
fn turn_over(items: &[ThreadItem], i: usize) -> bool {
    let it = &items[i];
    items[i + 1..]
        .iter()
        .filter(|later| later.session_id == it.session_id)
        .any(|later| later.turn > it.turn || (later.turn == it.turn && later.kind == crate::model::ItemKind::TurnEnd))
}

/// Lines for one row.  A pending approval gets a stream-specific card (it is
/// answered in the composer pane below).
fn render_row(v: &ThreadView, row: &Row, width: usize) -> Vec<Line<'static>> {
    if let Row::Choice(i) = row {
        let it = &v.data.items[*i];
        if it.status == Some(ItemStatus::Pending)
            && let ItemBody::Choice { prompt, options, .. } = &it.body
        {
            let who = it.agent.as_ref().map(|(n, _)| n.clone()).unwrap_or_else(|| "agent".into());
            let (glyph, verb, color) = match it.kind {
                ItemKind::Permission => ("!", "asks permission", theme::DANGER),
                _ => ("?", "asks", Color::Yellow),
            };
            let opts: Vec<String> = options.iter().enumerate().map(|(n, o)| format!("{} {}", n + 1, o.label)).collect();
            return vec![
                Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color).bold()),
                    Span::styled(format!("{who} {verb}: "), theme::bold()),
                    Span::raw(crate::util::ellipsize(prompt, width.saturating_sub(who.len() + verb.len() + 6))),
                ]),
                Line::from(Span::styled(format!("    {} · answer below", opts.join("  ")), theme::dim())),
            ];
        }
    }
    let mut lines = row_lines(v, row, width.saturating_sub(1), false);
    // Same left margin as the TUI's cursor column.
    for l in &mut lines {
        l.spans.insert(0, Span::raw(" "));
    }
    lines
}

fn header_line(v: &ThreadView) -> Line<'static> {
    let mut spans = vec![Span::styled(v.data.title.clone(), theme::accent_bold())];
    for (name, color, _) in &v.data.agents {
        spans.push(Span::styled("  ·  ", theme::dim()));
        spans.push(Span::styled(name.clone(), Style::default().fg(theme::agent_color(*color)).bold()));
    }
    Line::from(spans)
}

/// A styled line as ANSI text (SGR per span, reset at the end).
pub fn to_ansi(line: &Line) -> String {
    let mut s = String::new();
    for span in &line.spans {
        let st = line.style.patch(span.style);
        let sgr = sgr(st);
        // A line is one row: a line break inside one (a multi-line command in
        // a title) would print a row the live tail doesn't count.
        let content: std::borrow::Cow<str> =
            if span.content.contains(['\n', '\r']) { span.content.replace(['\n', '\r'], " ").into() } else { span.content.as_ref().into() };
        if sgr.is_empty() {
            s.push_str(&content);
        } else {
            s.push_str(&format!("\x1b[{sgr}m{content}\x1b[0m"));
        }
    }
    s
}

fn sgr(st: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    let m = st.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if m.contains(flag) {
            codes.push(code.into());
        }
    }
    if let Some(c) = st.fg.and_then(|c| color(c, false)) {
        codes.push(c);
    }
    if let Some(c) = st.bg.and_then(|c| color(c, true)) {
        codes.push(c);
    }
    codes.join(";")
}

fn color(c: Color, bg: bool) -> Option<String> {
    let base = |n: u8| Some((if bg { n + 10 } else { n }).to_string());
    match c {
        Color::Reset => None,
        Color::Black => base(30),
        Color::Red => base(31),
        Color::Green => base(32),
        Color::Yellow => base(33),
        Color::Blue => base(34),
        Color::Magenta => base(35),
        Color::Cyan => base(36),
        Color::Gray => base(37),
        Color::DarkGray => base(90),
        Color::LightRed => base(91),
        Color::LightGreen => base(92),
        Color::LightYellow => base(93),
        Color::LightBlue => base(94),
        Color::LightMagenta => base(95),
        Color::LightCyan => base(96),
        Color::White => base(97),
        Color::Indexed(n) => Some(format!("{};5;{n}", if bg { 48 } else { 38 })),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", if bg { 48 } else { 38 })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::fixtures;

    fn plain(s: &str) -> String {
        // Strip SGR sequences for assertions.
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn each_agent_is_a_record_of_this_pane_and_only_changes_are_sent() {
        use crate::model::{ItemKind, ItemStatus, StatusState};
        let mut d = fixtures::data();
        // The fixture: atlas, live, blocked on a pending permission.
        let recs = agent_records(&d);
        assert_eq!(recs, vec![AgentRecord { id: "atlas".into(), state: axel_rex::osc::State::Blocked, kind: Some(axel_rex::osc::Kind::Permission) }]);
        let first = status_reports(&[], &recs);
        assert!(first.contains("state=blocked:id=atlas:kind=permission:app=axel"), "{first}");
        assert_eq!(status_reports(&recs, &recs), "", "nothing changed, nothing sent");
        // It asks a question instead.
        for it in &mut d.items {
            if it.kind == ItemKind::Permission {
                it.kind = ItemKind::Question;
            }
        }
        assert_eq!(agent_records(&d)[0].kind, Some(axel_rex::osc::Kind::Question));
        // Working, then gone from the thread.
        d.agents[0].2 = StatusState::Working;
        d.items.iter_mut().for_each(|it| it.status = Some(ItemStatus::Completed));
        let working = agent_records(&d);
        assert!(status_reports(&recs, &working).starts_with("\x1b]7501;state=working:id=atlas"));
        d.live_sessions.clear();
        assert!(status_reports(&working, &agent_records(&d)).contains("state=clear:id=atlas"));
    }

    #[test]
    fn the_pane_prints_at_a_settled_width_and_reprints_when_it_changes() {
        let mut w = WidthWatch::default();
        // Opening at Rex's default width, then resized: nothing printed until it holds.
        assert_eq!(w.on_tick(80), Width::Wait);
        assert_eq!(w.on_tick(200), Width::Wait, "it moved: start over");
        assert_eq!(w.on_tick(200), Width::First, "held: print the thread");
        assert_eq!(w.on_tick(200), Width::Same);
        // Widened later (the dock settling, a window resize): reprint at the new width.
        assert_eq!(w.on_tick(240), Width::Wait);
        assert_eq!(w.on_tick(240), Width::Reprint);
        assert_eq!(w.on_tick(240), Width::Same);
        // A blip that comes back changes nothing.
        assert_eq!(w.on_tick(230), Width::Wait);
        assert_eq!(w.on_tick(240), Width::Same);
    }

    #[test]
    fn a_tail_taller_than_the_pane_doesnt_fit() {
        let s = Stream { live_lines: 10, ..Default::default() };
        assert!(s.tail_fits(40));
        assert!(s.tail_fits(12));
        assert!(!s.tail_fits(11), "the pane shrank under its tail");
    }

    #[test]
    fn forgetting_the_screen_prints_every_row_again() {
        let v = ThreadView::new("T1", fixtures::data());
        let mut s = Stream::default();
        let first = s.frame(&v, 80, 40);
        assert!(!first.is_empty());
        assert_eq!(s.frame(&v, 80, 40), "", "nothing new");
        s.forget();
        let again = s.frame(&v, 120, 40);
        assert!(again.contains("Fix the login bug"), "everything again, at the new width: {again}");
    }

    #[test]
    fn a_run_of_tool_calls_links_to_its_details() {
        let v = ThreadView::new("T1", fixtures::data());
        let mut s = Stream { links: Some(crate::util::ToolsLink { thread: "T1".into(), fold: String::new(), session: "session:s".into(), server: None }), ..Default::default() };
        let fold = v.rows().into_iter().find(|r| matches!(r, Row::Fold(_))).expect("the fixture has a fold");
        let first_tool = match &fold { Row::Fold(idx) => v.data.items[idx[0]].id.clone(), _ => unreachable!() };
        let lines = s.row_ansi(&v, &fold, 80);
        let want = format!("\x1b]8;;axel-links://tools?thread=T1&fold={first_tool}&session=session%3As\x1b\\");
        assert!(lines[0].starts_with(&want) && lines[0].ends_with("\x1b]8;;\x1b\\"), "{:?}", lines[0]);
        // Outside Rex: plain.
        s.links = None;
        assert!(!s.row_ansi(&v, &fold, 80)[0].contains("\x1b]8"));
    }

    #[test]
    fn the_bell_rings_when_an_agent_finishes_or_waits_for_you() {
        use axel_rex::osc::State;
        let rec = |id: &str, state| AgentRecord { id: id.into(), state, kind: None };
        let working = [rec("atlas", State::Working), rec("orion", State::Idle)];
        assert!(chime(&working, &[rec("atlas", State::Done), rec("orion", State::Idle)]), "atlas finished");
        assert!(chime(&working, &[rec("atlas", State::Blocked), rec("orion", State::Idle)]), "atlas waits for you");
        assert!(!chime(&working, &[rec("atlas", State::Working), rec("orion", State::Working)]), "orion started: no bell");
        assert!(!chime(&[], &[rec("atlas", State::Done)]), "already done when the pane opened");
        assert!(!chime(&[rec("atlas", State::Done)], &[rec("atlas", State::Done)]), "once");
    }

    #[test]
    fn the_root_record_is_the_threads_most_urgent_state() {
        use axel_rex::osc::{Kind, State};
        use base64::Engine;
        let b64 = |s: &str| base64::prelude::BASE64_STANDARD.encode(s);
        let rec = |id: &str, state, kind| AgentRecord { id: id.into(), state, kind };
        let root = root_report("Fix the install", &[rec("atlas", State::Idle, None), rec("orion", State::Working, None)]);
        assert!(root.starts_with("\x1b]7501;state=working:app=axel"), "no id: the root record: {root}");
        assert!(root.contains(&format!(":title={}", b64("Fix the install"))) && root.contains(&format!(":msg={}", b64("atlas idle · orion working"))), "{root}");
        let blocked = root_report("t", &[rec("atlas", State::Working, None), rec("orion", State::Blocked, Some(Kind::Question))]);
        assert!(blocked.starts_with("\x1b]7501;state=blocked:kind=question"), "{blocked}");
        assert!(root_report("t", &[]).starts_with("\x1b]7501;state=idle:app=axel"));
    }

    #[test]
    fn ansi_styles_spans() {
        let l = Line::from(vec![Span::raw("a "), Span::styled("b", Style::default().fg(Color::Indexed(179)).bold())]);
        assert_eq!(to_ansi(&l), "a \x1b[1;38;5;179mb\x1b[0m");
    }

    #[test]
    fn settled_rows_commit_once_and_the_tail_redraws() {
        // Fixture: user, assistant, fold, pending permission (agent blocked, not working).
        let mut v = ThreadView::new("T1", fixtures::data());
        let mut s = Stream::default();
        let f1 = plain(&s.frame(&v, 80, 40));
        assert!(f1.contains("Fix the login bug") && f1.contains("5 tool calls"), "{f1}");
        assert!(f1.contains("Atlas asks permission: Run cargo test"), "pending card in the tail: {f1}");
        assert!(f1.contains("1 Allow once  2 Always allow  3 Reject · answer below"), "{f1}");
        assert_eq!(s.committed.len(), 3, "user, assistant, fold are settled");
        assert_eq!(s.live_lines, 2);
        // Nothing changed: nothing written.
        assert_eq!(s.frame(&v, 80, 40), "");
        // Answered: the old tail is erased and the card is committed in its resolved form.
        let mut d = fixtures::data();
        d.items[7].status = Some(ItemStatus::Allowed);
        v.set_data(d.clone());
        let raw = s.frame(&v, 80, 40);
        assert!(raw.starts_with("\x1b[2F\x1b[J"), "erases the 2-line tail first: {raw:?}");
        assert!(plain(&raw).contains("allowed"), "{raw}");
        assert!(!plain(&raw).contains("Fix the login bug"), "settled rows are never printed twice");
        assert_eq!(s.live_lines, 0);
        // While the agent works, the last row and a spinner stay live.
        d.agents[0].2 = crate::model::StatusState::Working;
        let mut it = fixtures::item("a2", ItemKind::AssistantMessage, ItemBody::Text("Running the".into()), 9_000);
        it.status = Some(ItemStatus::Running);
        d.items.push(it);
        v.set_data(d);
        let f3 = plain(&s.frame(&v, 80, 40));
        assert!(f3.contains("Running the") && f3.contains("Atlas is working…"), "{f3}");
        assert_eq!(s.live_lines, 3, "header, text, spinner");
        // A tail taller than the pane is cut from the top, keeping the newest line.
        let f4 = plain(&s.frame(&v, 80, 3));
        assert_eq!(s.live_lines, 1);
        assert!(f4.contains("Atlas is working…"), "{f4}");
    }

    #[test]
    fn a_tool_call_its_turn_ended_without_doesnt_hold_the_tail() {
        // A call cancelled mid-turn stays `running` in the store: once its turn
        // is over it can't change, so it and everything after it settle.
        let mut d = fixtures::data();
        d.items[7].status = Some(ItemStatus::Allowed);
        let mut stuck = fixtures::tool("t9", "Write", "edit", 8_000);
        stuck.status = Some(ItemStatus::Running);
        d.items.push(stuck);
        d.items.push(fixtures::item("e1", ItemKind::TurnEnd, ItemBody::Empty, 8_100));
        let mut next = fixtures::item("u2", ItemKind::UserMessage, ItemBody::Text("and now?".into()), 8_200);
        next.agent = None;
        next.turn = 2;
        d.items.push(next);
        d.items.push(fixtures::item("a3", ItemKind::AssistantMessage, ItemBody::Text("Done.".into()), 8_300));
        let v = ThreadView::new("T1", d.clone());
        let mut s = Stream::default();
        let out = plain(&s.frame(&v, 80, 40));
        assert!(out.contains("and now?") && out.contains("Done."), "{out}");
        assert_eq!(s.live_lines, 0, "nothing held live: {out}");
        // Still in its turn (no turn end yet), it's live.
        let mut d2 = fixtures::data();
        d2.items[7].status = Some(ItemStatus::Allowed);
        let mut running = fixtures::tool("t9", "Write", "edit", 8_000);
        running.status = Some(ItemStatus::Running);
        d2.items.push(running);
        let mut s2 = Stream::default();
        let _ = s2.frame(&ThreadView::new("T1", d2), 80, 40);
        assert!(s2.live_lines > 0, "a call still in its turn stays live");
    }

    #[test]
    fn a_running_command_with_a_heredoc_redraws_in_place() {
        // The repeat bug, again: the fold line of a running `python3 - <<'PY'`
        // carried the command's line breaks, printing rows the tail didn't count.
        let mut d = fixtures::data();
        d.items[7].status = Some(ItemStatus::Allowed);
        d.agents[0].2 = crate::model::StatusState::Working;
        let mut cmd = fixtures::tool("t9", "cd ~/w && python3 - <<'PY'\np=\"main.rs\"; s=open(p).read()\nPY", "execute", 8_000);
        cmd.status = Some(ItemStatus::Running);
        d.items.push(cmd);
        let v = ThreadView::new("T1", d);
        let mut s = Stream::default();
        let first = s.frame(&v, 120, 40);
        let tail_start = first.rfind("1 tool call").expect("the fold line is in the tail");
        let line = &first[first[..tail_start].rfind("\r\n").map_or(0, |i| i + 2)..];
        assert_eq!(line.matches('\n').count(), s.live_lines, "one row per counted row: {line:?}");
        assert!(plain(line).contains("python3 - <<'PY' …"), "the command's first line: {line:?}");
        s.tick += 1;
        let again = s.frame(&v, 120, 40);
        assert!(again.starts_with(&format!("\x1b[{}F\x1b[J", s.live_lines)), "{again:?}");
    }

    #[test]
    fn a_line_wider_than_the_pane_is_counted_in_rows() {
        assert_eq!(screen_rows("", 80), 1, "an empty line still takes its row");
        assert_eq!(screen_rows("abc", 80), 1);
        assert_eq!(screen_rows(&"x".repeat(80), 80), 1);
        assert_eq!(screen_rows(&"x".repeat(81), 80), 2);
        assert_eq!(screen_rows(&format!("\x1b[1;31m{}\x1b[0m", "x".repeat(80)), 80), 1, "colours take no room");
        assert_eq!(screen_rows(&crate::util::hyperlink("axel-links://tools?x=1", &"x".repeat(80)), 80), 1, "nor do links");
        assert_eq!(screen_rows("\t\tx", 10), 2, "a tab runs to the next stop");
        assert_eq!(screen_rows(&"字".repeat(41), 80), 2, "wide characters take two columns");
        assert_eq!(screen_rows("a\nb", 80), 2, "a line break inside a line is a row");
        let multi = to_ansi(&Line::from(vec![Span::raw("▸ "), Span::styled("cd x && python3 - <<'PY'\np=1", Style::default().bold())]));
        assert!(!multi.contains('\n'), "a line prints as one row: {multi:?}");
        assert_eq!(crate::view::thread_view::one_line("cd x && python3 - <<'PY'\np=1\nPY"), "cd x && python3 - <<'PY' …");
        assert_eq!(crate::view::thread_view::one_line("ls -la"), "ls -la");
    }

    #[test]
    fn a_wrapped_tail_is_erased_whole_before_its_redraw() {
        // The repeat bug: a tail line the terminal wraps takes two rows, and a
        // redraw that climbs one per line leaves the top of the old tail behind.
        let mut d = fixtures::data();
        d.items[7].status = Some(ItemStatus::Allowed);
        d.agents[0].2 = crate::model::StatusState::Working;
        let mut it = fixtures::item("a2", ItemKind::AssistantMessage, ItemBody::Text(format!("start\t{}", "\tcol".repeat(30))), 9_000);
        it.status = Some(ItemStatus::Running);
        d.items.push(it);
        let v = ThreadView::new("T1", d);
        let mut s = Stream::default();
        let _ = s.frame(&v, 80, 40);
        let drawn: usize = s.last_live.iter().map(|l| screen_rows(l, 80)).sum();
        assert!(drawn > s.last_live.len(), "the fixture's tab-wide line wraps: {drawn} rows, {} lines", s.last_live.len());
        assert_eq!(s.live_lines, drawn, "the tail is remembered in rows");
        s.tick += 1; // the spinner moves: a redraw
        let raw = s.frame(&v, 80, 40);
        assert!(raw.starts_with(&format!("\x1b[{drawn}F\x1b[J")), "climbs every row of the old tail: {raw:?}");
        // And a tail capped to the pane is capped in rows.
        let _ = s.frame(&v, 80, 6);
        assert!(s.live_lines <= 4, "at most the pane's height less two rows: {}", s.live_lines);
    }
}
