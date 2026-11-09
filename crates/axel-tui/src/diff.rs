//! The diff pane: what one turn changed, to read, mark and comment on.
//!
//! A file navigator and a source-aligned code view with inline review notes.
//! `v` marks lines, `c` writes a note under them; the notes pile up as drafts
//! and `⏎` sends them to the agent as one message, each note with its path,
//! line range and the marked lines, so "this is wrong" points at the code.
//! `u` undoes the turn.  The pane is pure state here ([`DiffView`]) plus a
//! loop ([`run_diff`]) that hands every outcome back to the caller, which owns
//! the repository, the drafts on disk and the session.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::Frame;
use serde::{Deserialize, Serialize};

const POLL: Duration = Duration::from_millis(60);

mod paint;
mod palette;
mod render;

#[derive(Clone, Copy, Debug)]
enum Hit { File(usize), Line(usize, usize, Side) }

/// Everything the pane shows, prepared by the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffData {
    pub agent: String,
    pub turn: i64,
    /// The thread's title.
    pub title: String,
    /// `3 files +42 −7`.
    pub headline: String,
    pub files: Vec<FileEntry>,
    /// The parsed patch, one entry per file that has one (binary files have none).
    pub patches: Vec<FilePatch>,
    /// Optional display spans, aligned with each file's patch lines.
    pub syntax: BTreeMap<String, Vec<Vec<SyntaxSpan>>>,
    pub between_turns: bool,
    pub truncated: bool,
    /// A line for the footer: what just happened, or where the diff is.
    pub note: Option<String>,
    /// The last undo hit overlapping later work: `R` restores instead.
    pub offer_restore: bool,
    /// The session is between turns, so undo is allowed.
    pub can_undo: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyntaxTone {
    #[default]
    Plain,
    Muted,
    Keyword,
    String,
    Number,
    Function,
    Type,
    Bold,
    Italic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxSpan {
    pub text: String,
    pub tone: SyntaxTone,
}

/// One changed file, as the file list shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// `old → new` for a rename, else the path.
    pub label: String,
    pub status: char,
    pub added: Option<i64>,
    pub removed: Option<i64>,
    pub authored: bool,
}

/// A file's part of a unified diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Add,
    Del,
    /// `Binary files differ` and the like.
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// Without the leading `+`, `-` or space.
    pub text: String,
}

/// Which side of the diff a note's lines are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Old,
    New,
}

/// A review note on a range of lines, not sent yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub side: Side,
    /// The marked lines, as they read in the file.
    pub text: String,
    pub note: String,
}

/// What the pane asks its caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Close,
    /// Send this message to the agent (the drafts, formatted).
    Send(String),
    Undo,
    /// Restore the turn's files to their `before` content (after a conflicting undo).
    Restore,
    /// Show the previous (−1) or next (+1) turn of the session.
    Turn(i64),
}

/// How the diff is laid out: one column of +/− lines, or the old and new
/// text side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Unified,
    SideBySide,
}

impl Layout {
    /// Side by side needs room for two readable columns.
    pub const SIDE_BY_SIDE_MIN_WIDTH: usize = 110;

    /// What a pane `width` columns wide gets when nobody chose.
    pub fn auto(width: usize) -> Layout {
        if width >= Self::SIDE_BY_SIDE_MIN_WIDTH { Layout::SideBySide } else { Layout::Unified }
    }

    pub fn other(self) -> Layout {
        match self {
            Layout::Unified => Layout::SideBySide,
            Layout::SideBySide => Layout::Unified,
        }
    }
}

/// One row of the side-by-side layout: indices into the patch lines.  A
/// hunk header or a `Binary files` line spans both columns (`full`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SideRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub full: Option<usize>,
}

impl SideRow {
    /// The line the cursor sits on when it lands on this row.
    pub fn first(&self) -> usize {
        self.full.or(self.left).or(self.right).unwrap_or(0)
    }

    pub fn holds(&self, line: usize) -> bool {
        self.full == Some(line) || self.left == Some(line) || self.right == Some(line)
    }
}

/// Pair the patch lines into side-by-side rows: context on both sides, a
/// run of removed lines beside the run of added lines that follows it, the
/// longer run's tail alone, headers across both columns.
pub fn side_rows(lines: &[DiffLine]) -> Vec<SideRow> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            LineKind::Hunk | LineKind::Meta => {
                rows.push(SideRow { full: Some(i), ..Default::default() });
                i += 1;
            }
            LineKind::Context => {
                rows.push(SideRow { left: Some(i), right: Some(i), full: None });
                i += 1;
            }
            LineKind::Del | LineKind::Add => {
                let dels: Vec<usize> = (i..lines.len()).take_while(|&j| lines[j].kind == LineKind::Del).collect();
                let after = i + dels.len();
                let adds: Vec<usize> = (after..lines.len()).take_while(|&j| lines[j].kind == LineKind::Add).collect();
                for k in 0..dels.len().max(adds.len()) {
                    rows.push(SideRow { left: dels.get(k).copied(), right: adds.get(k).copied(), full: None });
                }
                i = after + adds.len();
            }
        }
    }
    rows
}

/// The pane's state.
#[derive(Clone, Debug)]
pub struct DiffView {
    pub data: DiffData,
    pub drafts: Vec<Draft>,
    /// Selected file (an index into `data.files`).
    pub file: usize,
    /// Cursor line in the selected file's patch.
    pub cursor: usize,
    /// First displayed row, including inline annotations.
    pub scroll: usize,
    /// Where a mark started (`v`), until a note is written or it's cleared.
    pub mark: Option<usize>,
    /// A note being typed.
    pub input: Option<String>,
    /// Waiting for `y` to undo.
    pub confirm_undo: bool,
    /// The layout chosen with `s`; `None` follows the pane's width.
    pub layout: Option<Layout>,
    /// Rows the diff got at the last draw, to page by.
    page: usize,
    /// The code area's width at the last draw, for the automatic layout.
    last_width: usize,
    horizontal: usize,
    side: Side,
    emphasis: BTreeMap<String, Vec<Vec<std::ops::Range<usize>>>>,
    follow_cursor: bool,
    jump_file: bool,
    palette: palette::Palette,
    hits: Vec<(Rect, Hit)>,
}

impl DiffView {
    pub fn new(data: DiffData, drafts: Vec<Draft>) -> Self {
        let file = data.files.iter().position(|f| f.authored).unwrap_or(0);
        let emphasis = data.patches.iter().map(|p| (p.path.clone(), paint::word_changes(&p.lines))).collect();
        DiffView { data, drafts, file, cursor: 0, scroll: 0, mark: None, input: None, confirm_undo: false, layout: None, page: 20, last_width: 80, horizontal: 0, side: Side::Old, emphasis, follow_cursor: true, jump_file: true, palette: palette::Palette::default(), hits: Vec::new() }
    }

    /// The layout in force: the one chosen, else the one the width calls for.
    pub fn layout(&self) -> Layout {
        self.layout.unwrap_or_else(|| Layout::auto(self.last_width))
    }

    /// The side-by-side rows of the selected file.
    pub fn rows(&self) -> Vec<SideRow> {
        side_rows(self.lines())
    }

    /// The row the cursor is on (side by side).
    pub fn cursor_row(&self) -> usize {
        self.rows().iter().position(|r| r.holds(self.cursor)).unwrap_or(0)
    }

    /// The selected file's patch lines (none for a binary file or no diff).
    pub fn lines(&self) -> &[DiffLine] {
        self.lines_for(self.file)
    }

    fn lines_for(&self, file: usize) -> &[DiffLine] {
        let Some(f) = self.data.files.get(file) else { return &[] };
        self.data.patches.iter().find(|p| p.path == f.path).map(|p| p.lines.as_slice()).unwrap_or(&[])
    }

    /// The marked range (inclusive, in patch lines), when a mark is set.
    pub fn marked(&self) -> Option<(usize, usize)> {
        self.mark.map(|m| (m.min(self.cursor), m.max(self.cursor)))
    }

    /// Drafts on the selected file.
    pub fn drafts_here(&self) -> Vec<&Draft> {
        let Some(f) = self.data.files.get(self.file) else { return Vec::new() };
        self.drafts.iter().filter(|d| d.path == f.path).collect()
    }

    fn select_file(&mut self, i: usize) {
        if i < self.data.files.len() {
            self.file = i;
            self.cursor = 0;
            self.jump_file = true;
            self.mark = None;
            self.horizontal = 0;
            self.follow_cursor = true;
        }
    }

    fn file_order(&self) -> Vec<usize> {
        (0..self.data.files.len()).filter(|&i| self.data.files[i].authored)
            .chain((0..self.data.files.len()).filter(|&i| !self.data.files[i].authored)).collect()
    }

    fn step_file(&mut self, next: bool) {
        let order = self.file_order();
        let current = order.iter().position(|&i| i == self.file).unwrap_or(0);
        let target = if next { current.saturating_add(1) } else { current.saturating_sub(1) };
        if let Some(&i) = order.get(target) { self.select_file(i); }
    }

    /// Move by lines (unified) or by rows (side by side); the draw keeps the
    /// cursor on screen.
    fn move_cursor(&mut self, delta: isize) {
        self.follow_cursor = true;
        let split = self.layout() == Layout::SideBySide;
        let mut stops = Vec::new();
        for file in self.file_order() {
            if self.mark.is_some() && file != self.file { continue }
            let lines = self.lines_for(file);
            if lines.is_empty() { stops.push((file, SideRow::default())); }
            else if split { stops.extend(side_rows(lines).into_iter().map(|row| (file, row))); }
            else { stops.extend((0..lines.len()).map(|i| (file, SideRow { full: Some(i), ..Default::default() }))); }
        }
        if stops.is_empty() { return }
        let current = stops.iter().position(|(file, row)| *file == self.file && (row.holds(self.cursor) || self.lines().is_empty())).unwrap_or(0);
        let target = (current as isize + delta).clamp(0, stops.len() as isize - 1) as usize;
        let (file, row) = stops[target];
        if file != self.file { self.mark = None; }
        self.file = file;
        self.jump_file = false;
        self.cursor = match self.side { Side::Old => row.left, Side::New => row.right }.or(row.full).unwrap_or_else(|| row.first());
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.input.is_some() || self.confirm_undo { return }
        match mouse.kind {
            MouseEventKind::ScrollUp => { self.scroll = self.scroll.saturating_sub(3); self.follow_cursor = false; }
            MouseEventKind::ScrollDown => { self.scroll = self.scroll.saturating_add(3); self.follow_cursor = false; }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, hit)) = self.hits.iter().find(|(r, _)| r.contains((mouse.column, mouse.row).into())) {
                    match *hit {
                        Hit::File(i) => self.select_file(i),
                        Hit::Line(file, i, side) => {
                            if self.file != file { self.mark = None; }
                            self.file = file; self.cursor = i; self.side = side;
                            self.follow_cursor = true; self.jump_file = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The draft a note on the current mark (or cursor line) would be.
    fn draft_at_cursor(&self, note: String) -> Option<Draft> {
        let file = self.data.files.get(self.file)?;
        let (a, b) = self.marked().unwrap_or((self.cursor, self.cursor));
        let lines: Vec<&DiffLine> = self.lines().get(a..=b)?.iter().filter(|l| l.kind != LineKind::Hunk && l.kind != LineKind::Meta).collect();
        if lines.is_empty() {
            return None;
        }
        // Removed lines live on the old side; anything else reads on the new.
        let side = if lines.iter().all(|l| l.kind == LineKind::Del) { Side::Old } else if lines.iter().any(|l| l.kind == LineKind::Add) { Side::New } else if self.layout() == Layout::SideBySide { self.side } else { Side::New };
        let lines: Vec<_> = lines.into_iter().filter(|l| match side { Side::Old => l.old.is_some(), Side::New => l.new.is_some() }).collect();
        let numbers: Vec<usize> = lines.iter().filter_map(|l| if side == Side::Old { l.old } else { l.new }).collect();
        let (start, end) = (*numbers.iter().min()?, *numbers.iter().max()?);
        let text = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        Some(Draft { path: file.path.clone(), start, end, side, text, note })
    }

    /// Handle a key; `Some` when the caller has something to do.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Outcome::Close);
        }
        if let Some(buf) = &mut self.input {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let note = self.input.take().unwrap_or_default();
                    if !note.trim().is_empty()
                        && let Some(d) = self.draft_at_cursor(note.trim().to_string())
                    {
                        self.drafts.push(d);
                    }
                    self.mark = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return None;
        }
        if self.confirm_undo {
            self.confirm_undo = false;
            return (key.code == KeyCode::Char('y')).then_some(Outcome::Undo);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Some(Outcome::Close),
            KeyCode::Up => self.step_file(false),
            KeyCode::Down => self.step_file(true),
            KeyCode::Left => self.horizontal = self.horizontal.saturating_sub(8),
            KeyCode::Right => {
                let max = self.lines().iter().map(|l| unicode_width::UnicodeWidthStr::width(l.text.as_str())).max().unwrap_or(0);
                self.horizontal = self.horizontal.saturating_add(8).min(max);
            }
            KeyCode::Home => self.horizontal = 0,
            KeyCode::Tab if self.layout() == Layout::SideBySide => {
                self.side = if self.side == Side::Old { Side::New } else { Side::Old };
                if let Some(row) = self.rows().get(self.cursor_row()) {
                    self.cursor = match self.side { Side::Old => row.left, Side::New => row.right }.or(row.full).unwrap_or_else(|| row.first());
                }
                self.mark = None;
            }
            KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page as isize)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.move_cursor(self.page as isize),
            KeyCode::Char('g') => self.move_cursor(-(self.data.patches.iter().map(|p| p.lines.len() + 1).sum::<usize>() as isize)),
            KeyCode::Char('G') => self.move_cursor(self.data.patches.iter().map(|p| p.lines.len() + 1).sum::<usize>() as isize),
            KeyCode::Char('v') => self.mark = if self.mark.is_some() { None } else { Some(self.cursor) },
            KeyCode::Char('s') => { self.layout = Some(self.layout().other()); self.follow_cursor = true; self.jump_file = false; },
            KeyCode::Char('n') | KeyCode::Char('p') => {
                let next = key.code == KeyCode::Char('n');
                let hunks: Vec<_> = self.file_order().into_iter().flat_map(|file| {
                    self.lines_for(file).iter().enumerate().filter(|(_, l)| l.kind == LineKind::Hunk).map(move |(i, _)| (file, i))
                }).collect();
                let order = self.file_order();
                let position = |file| order.iter().position(|&f| f == file).unwrap_or(0);
                let current = (position(self.file), self.cursor);
                let eligible = |&(file, i): &(usize, usize)| self.mark.is_none() || (file == self.file && i != self.cursor);
                let target = if next { hunks.iter().copied().filter(eligible).find(|&(file, i)| (position(file), i) > current) }
                    else { hunks.iter().rev().copied().filter(eligible).find(|&(file, i)| (position(file), i) < current) };
                if let Some((file, i)) = target { self.file = file; self.cursor = i; self.follow_cursor = true; self.jump_file = false; }
            }
            KeyCode::Char('c') => {
                if !self.lines().is_empty() {
                    self.input = Some(String::new());
                }
            }
            KeyCode::Char('d') => {
                // Drop the draft under the cursor.
                if let Some(path) = self.data.files.get(self.file).map(|f| f.path.clone())
                    && let Some(line) = self.lines().get(self.cursor).cloned()
                    && let Some(i) = self.drafts.iter().position(|d| d.path == path && covers(d, &line))
                {
                    self.drafts.remove(i);
                }
            }
            KeyCode::Enter => {
                if !self.drafts.is_empty() {
                    return Some(Outcome::Send(review_message(self.data.turn, &self.drafts)));
                }
            }
            KeyCode::Char('u') => {
                if self.data.can_undo {
                    self.confirm_undo = true;
                }
            }
            KeyCode::Char('R') => {
                if self.data.offer_restore {
                    return Some(Outcome::Restore);
                }
            }
            KeyCode::Char('[') => return Some(Outcome::Turn(-1)),
            KeyCode::Char(']') => return Some(Outcome::Turn(1)),
            _ => {}
        }
        None
    }
}

/// `line` is one of `d`'s lines.
fn covers(d: &Draft, line: &DiffLine) -> bool {
    let n = match d.side {
        Side::Old => line.old,
        Side::New => line.new,
    };
    n.is_some_and(|n| d.start <= n && n <= d.end)
}

/// The drafts as one message to the agent: a fenced block per note with
/// the path, the line range, the side, the marked lines, then the note.
/// Plain text, so it reads the same on every harness.
pub fn review_message(turn: i64, drafts: &[Draft]) -> String {
    let mut out = format!("Review of your turn {turn}:\n");
    for d in drafts {
        let fence = "`".repeat(3.max(d.text.matches("```").count() + 3));
        let side = match d.side {
            Side::Old => "old",
            Side::New => "new",
        };
        let lines = if d.start == d.end { d.start.to_string() } else { format!("{}-{}", d.start, d.end) };
        out.push_str(&format!("\n{fence}review path={} lines={lines} side={side}\n{}\n{fence}\n{}\n", d.path, d.text, d.note));
    }
    out
}

/// Parse `git diff --no-color` output into per-file patches.  Headers other
/// than hunks are dropped; `Binary files differ` stays as a line.
pub fn parse_patch(diff: &str) -> Vec<FilePatch> {
    let mut out: Vec<FilePatch> = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            // `a/x b/y`: the new path is the second; quoted paths keep their quotes.
            let path = rest.rsplit_once(" b/").map(|(_, b)| b.to_string()).unwrap_or_default();
            out.push(FilePatch { path, lines: Vec::new() });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        if let Some(rest) = raw.strip_prefix("+++ b/") {
            // The authoritative new path (renames, odd names).
            cur.path = rest.to_string();
            continue;
        }
        if raw.starts_with("--- ") || raw.starts_with("index ") || raw.starts_with("+++ ") || raw.starts_with("\\ ") {
            continue;
        }
        if raw.starts_with("@@") {
            let nums = raw.split_whitespace().nth(1).and_then(|s| s.strip_prefix('-')).and_then(|s| s.split(',').next()).and_then(|s| s.parse().ok());
            let nums2 = raw.split_whitespace().nth(2).and_then(|s| s.strip_prefix('+')).and_then(|s| s.split(',').next()).and_then(|s| s.parse().ok());
            old = nums.unwrap_or(1);
            new = nums2.unwrap_or(1);
            cur.lines.push(DiffLine { kind: LineKind::Hunk, old: None, new: None, text: raw.to_string() });
            continue;
        }
        if cur.lines.is_empty() && !raw.starts_with(['+', '-', ' ']) {
            // Still in the header: modes, similarity, rename from/to, Binary files…
            if raw.starts_with("Binary files") {
                cur.lines.push(DiffLine { kind: LineKind::Meta, old: None, new: None, text: raw.to_string() });
            }
            continue;
        }
        let (kind, text) = match raw.chars().next() {
            Some('+') => (LineKind::Add, &raw[1..]),
            Some('-') => (LineKind::Del, &raw[1..]),
            Some(' ') => (LineKind::Context, &raw[1..]),
            _ => (LineKind::Context, raw),
        };
        let line = match kind {
            LineKind::Add => {
                new += 1;
                DiffLine { kind, old: None, new: Some(new - 1), text: text.to_string() }
            }
            LineKind::Del => {
                old += 1;
                DiffLine { kind, old: Some(old - 1), new: None, text: text.to_string() }
            }
            _ => {
                old += 1;
                new += 1;
                DiffLine { kind, old: Some(old - 1), new: Some(new - 1), text: text.to_string() }
            }
        };
        cur.lines.push(line);
    }
    out
}

/// Render the review pane using its shared code and annotation row plan.
pub fn render(frame: &mut Frame, v: &mut DiffView) {
    render::render(frame, v);
}

/// Run the pane until it has something for the caller: draws, reads keys.
/// Returns the outcome and the drafts as they stand.
pub fn run_diff(data: DiffData, drafts: Vec<Draft>) -> Result<(Outcome, Vec<Draft>)> {
    let mut v = DiffView::new(data, drafts);
    v.palette = palette::Palette::detect();
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), event::EnableMouseCapture, event::EnableBracketedPaste);
    let result = (|| -> Result<Outcome> {
        loop {
            terminal.draw(|f| render(f, &mut v))?;
            if event::poll(POLL)? {
                match event::read()? {
                    Event::Key(key) => if let Some(out) = v.handle_key(key) { return Ok(out); },
                    Event::Mouse(mouse) => v.handle_mouse(mouse),
                    Event::Paste(text) => if let Some(note) = &mut v.input {
                        note.extend(text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t'));
                    },
                    _ => {}
                }
            }
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), event::DisableMouseCapture, event::DisableBracketedPaste);
    ratatui::restore();
    Ok((result?, v.drafts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const PATCH: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    old();\n+    new();\n+    more();\n }\ndiff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\ndiff --git a/img.png b/img.png\nnew file mode 100644\nBinary files /dev/null and b/img.png differ\n";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn data() -> DiffData {
        DiffData {
            agent: "lyra".into(),
            turn: 4,
            title: "Ship it".into(),
            headline: "3 files +2 −1".into(),
            files: vec![
                FileEntry { path: "Cargo.lock".into(), label: "Cargo.lock".into(), status: 'M', added: Some(0), removed: Some(1), authored: false },
                FileEntry { path: "src/a.rs".into(), label: "src/a.rs".into(), status: 'M', added: Some(2), removed: Some(1), authored: true },
                FileEntry { path: "img.png".into(), label: "img.png".into(), status: 'A', added: None, removed: None, authored: true },
            ],
            patches: parse_patch(PATCH),
            can_undo: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_patch_parses_into_numbered_lines_per_file() {
        let p = parse_patch(PATCH);
        assert_eq!(p.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["src/a.rs", "new.rs", "img.png"]);
        let a = &p[0].lines;
        assert_eq!(a[0].kind, LineKind::Hunk);
        assert_eq!((a[1].kind, a[1].old, a[1].new, a[1].text.as_str()), (LineKind::Context, Some(1), Some(1), "fn main() {"));
        assert_eq!((a[2].kind, a[2].old, a[2].new), (LineKind::Del, Some(2), None));
        assert_eq!((a[3].kind, a[3].old, a[3].new, a[3].text.as_str()), (LineKind::Add, None, Some(2), "    new();"));
        assert_eq!((a[4].new, a[5].old, a[5].new), (Some(3), Some(3), Some(4)));
        assert!(p[1].lines.is_empty(), "a pure rename has no lines");
        assert_eq!(p[2].lines[0].kind, LineKind::Meta);
        assert!(parse_patch("").is_empty());
    }

    #[test]
    fn marking_and_commenting_makes_a_draft_and_the_message() {
        let mut v = DiffView::new(data(), vec![]);
        assert_eq!(v.file, 1, "opens on the agent's first file");
        // j to the removed line, mark, j twice, comment.
        v.handle_key(key(KeyCode::Char('j')));
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.lines()[v.cursor].kind, LineKind::Del);
        v.handle_key(key(KeyCode::Char('v')));
        v.handle_key(key(KeyCode::Char('j')));
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.marked(), Some((2, 4)));
        v.handle_key(key(KeyCode::Char('c')));
        for ch in "use one call".chars() {
            v.handle_key(key(KeyCode::Char(ch)));
        }
        assert!(v.handle_key(key(KeyCode::Enter)).is_none(), "Enter in the note saves it");
        assert_eq!(v.drafts.len(), 1);
        let d = &v.drafts[0];
        assert_eq!((d.path.as_str(), d.start, d.end, d.side), ("src/a.rs", 2, 3, Side::New), "mixed lines read on the new side");
        assert_eq!(d.text, "    new();\n    more();", "new-side notes exclude removed text");
        assert_eq!(v.mark, None);
        // A note on a removed line alone is on the old side.
        v.cursor = 2;
        v.handle_key(key(KeyCode::Char('c')));
        v.handle_key(key(KeyCode::Char('x')));
        v.handle_key(key(KeyCode::Enter));
        assert_eq!((v.drafts[1].side, v.drafts[1].start), (Side::Old, 2));
        let Some(Outcome::Send(msg)) = v.handle_key(key(KeyCode::Enter)) else { panic!("Enter sends") };
        assert_eq!(
            msg,
            "Review of your turn 4:\n\n```review path=src/a.rs lines=2-3 side=new\n    new();\n    more();\n```\nuse one call\n\n```review path=src/a.rs lines=2 side=old\n    old();\n```\nx\n"
        );
        // d drops the draft under the cursor.
        v.handle_key(key(KeyCode::Char('d')));
        assert_eq!(v.drafts.len(), 1);
        assert_eq!(v.drafts[0].note, "use one call");
    }

    #[test]
    fn undo_asks_first_and_other_keys_route() {
        let mut v = DiffView::new(data(), vec![]);
        assert_eq!(v.handle_key(key(KeyCode::Char('u'))), None);
        assert!(v.confirm_undo);
        assert_eq!(v.handle_key(key(KeyCode::Char('n'))), None);
        v.handle_key(key(KeyCode::Char('u')));
        assert_eq!(v.handle_key(key(KeyCode::Char('y'))), Some(Outcome::Undo));
        assert_eq!(v.handle_key(key(KeyCode::Char(']'))), Some(Outcome::Turn(1)));
        assert_eq!(v.handle_key(key(KeyCode::Char('q'))), Some(Outcome::Close));
        assert_eq!(v.handle_key(key(KeyCode::Char('R'))), None, "no restore offered");
        v.data.offer_restore = true;
        assert_eq!(v.handle_key(key(KeyCode::Char('R'))), Some(Outcome::Restore));
        v.data.can_undo = false;
        v.handle_key(key(KeyCode::Char('u')));
        assert!(!v.confirm_undo, "mid-turn: no undo");
        assert_eq!(v.handle_key(key(KeyCode::Enter)), None, "nothing to send");
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.file, 1, "the first authored file is first in the navigator");
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.file, 2);
        assert!(v.lines().iter().all(|l| l.kind == LineKind::Meta));
    }

    #[test]
    fn side_by_side_pairs_removed_lines_with_the_added_ones_that_follow() {
        let rows = side_rows(&parse_patch(PATCH)[0].lines);
        assert_eq!(rows[0], SideRow { full: Some(0), ..Default::default() }, "the hunk header spans both");
        assert_eq!(rows[1], SideRow { left: Some(1), right: Some(1), full: None }, "context on both sides");
        assert_eq!(rows[2], SideRow { left: Some(2), right: Some(3), full: None }, "old() beside new()");
        assert_eq!(rows[3], SideRow { left: None, right: Some(4), full: None }, "the extra added line alone on the right");
        assert_eq!(rows[4], SideRow { left: Some(5), right: Some(5), full: None });
        assert_eq!(rows.len(), 5);
        assert!(side_rows(&[]).is_empty());
    }

    #[test]
    fn s_switches_the_layout_and_the_cursor_walks_rows_side_by_side() {
        let mut v = DiffView::new(data(), vec![]);
        assert_eq!(v.layout(), Layout::Unified, "narrow by default");
        v.last_width = 120;
        assert_eq!(v.layout(), Layout::SideBySide, "wide panes split by themselves");
        v.handle_key(key(KeyCode::Char('s')));
        assert_eq!(v.layout(), Layout::Unified, "s overrides the width");
        v.handle_key(key(KeyCode::Char('s')));
        assert_eq!(v.layout(), Layout::SideBySide);
        // j from the header: context row, then the paired row, landing on its left (removed) line.
        v.handle_key(key(KeyCode::Char('j')));
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.cursor, 2);
        assert_eq!(v.cursor_row(), 2);
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.cursor, 4, "the row with only an added line puts the cursor on it");
        v.handle_key(key(KeyCode::Char('k')));
        assert_eq!(v.cursor, 2);
    }

    #[test]
    fn side_by_side_draws_old_and_new_on_one_row() {
        let mut v = DiffView::new(data(), vec![]);
        let mut term = Terminal::new(TestBackend::new(120, 14)).unwrap();
        term.draw(|f| render(f, &mut v)).unwrap();
        let text: Vec<String> = (0..14).map(|y| (0..120).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
        assert_eq!(v.layout(), Layout::SideBySide, "120 columns: split by default");
        let paired = text.iter().find(|l| l.contains("old();")).expect("the removed line is drawn");
        assert!(paired.contains("old();") && paired.contains("│") && paired.contains("new();"), "{paired:?}");
        let (left, right) = paired.split_once('│').unwrap();
        assert!(left.trim_start().starts_with("▎ 2 −"), "old numbers on the left: {left:?}");
        assert!(right.trim_start().starts_with("▎ 2 +"), "new numbers on the right: {right:?}");
        let alone = text.iter().find(|l| l.contains("more();")).expect("the extra added line");
        assert!(alone.split_once('│').unwrap().0.trim().is_empty(), "nothing on the left of it: {alone:?}");
        assert!(text[13].contains("s unified"), "{}", text[13]);
    }

    #[test]
    fn the_pane_draws_the_list_the_diff_and_the_keys() {
        let mut v = DiffView::new(data(), vec![Draft { path: "src/a.rs".into(), start: 2, end: 2, side: Side::New, text: "    new();".into(), note: "why?".into() }]);
        let mut term = Terminal::new(TestBackend::new(100, 14)).unwrap();
        term.draw(|f| render(f, &mut v)).unwrap();
        let text: Vec<String> = (0..14).map(|y| (0..100).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
        assert_eq!(text[0], "lyra · turn 4 · 3 files +2 −1");
        assert!(text[1].contains("src/a.rs") && text[1].contains("+2  −1"));
        assert!(text.iter().any(|l| l.contains("old();")));
        assert!(text.iter().any(|l| l.contains("new();")));
        assert!(text.iter().any(|l| l.contains("note · new L2")));
        assert!(text.iter().any(|l| l.contains("why?")));
        assert!(text[13].contains("←→ pan") && text[13].contains("send 1 note"));
    }
}
