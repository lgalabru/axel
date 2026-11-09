//! Print the diff once, as the conversation and tool panes do. No alternate
//! screen, mouse capture, or repainting while the reader scrolls and selects.

use std::io::{Write, stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{event::{self, Event, KeyCode, KeyEventKind, KeyModifiers}, terminal};
use ratatui::{style::Style, text::Line};

use super::{DiffView, Layout, Outcome, paint, render, run_review};
use crate::{stream::to_ansi, theme};

const SETTLE: Duration = Duration::from_millis(200);

struct Input;

impl Input {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = write!(stdout(), "\x1b[0m\x1b[?25h");
        let _ = stdout().flush();
        let _ = terminal::disable_raw_mode();
    }
}

fn line(text: &str, width: usize, style: Style) -> Line<'static> {
    Line::from(paint::code(text, &[], &[], 0, width, style, None))
}

fn help(v: &DiffView) -> String {
    let layout = if v.layout() == Layout::Unified { "s split" } else { "s unified" };
    let notes = if v.drafts.is_empty() { String::new() } else { format!(" · Enter sends {} note(s)", v.drafts.len()) };
    let undo = if v.data.can_undo { " · u undo" } else { "" };
    let restore = if v.data.offer_restore { " · R restore" } else { "" };
    format!("Scroll and select as usual · r review lines · {layout} · [ ] turn{notes}{undo}{restore} · q close")
}

fn print(out: &mut impl Write, v: &DiffView, width: usize) -> Result<()> {
    // Width changes produce a fresh rendering, as in the tools pane. Otherwise
    // the output is immutable and belongs entirely to native scrollback.
    write!(out, "\x1b[H\x1b[2J\x1b[3J\x1b[?25l")?;
    let heading = format!("{} · turn {} · {}", v.data.agent, v.data.turn, v.data.headline);
    writeln!(out, "{}\r", to_ansi(&line(&heading, width, theme::bold())))?;
    writeln!(out, "{}\r\n", to_ansi(&line(&help(v), width, theme::dim())))?;
    if v.data.files.is_empty() {
        writeln!(out, "{}\r", to_ansi(&line("No changes in this turn", width, theme::dim())))?;
    }
    for row in render::scrollback_rows(v, width) {
        writeln!(out, "{}\r", to_ansi(&row))?;
    }
    if let Some(note) = &v.data.note {
        writeln!(out, "\r\n{}\r", to_ansi(&line(note, width, theme::accent())))?;
    }
    writeln!(out, "\r\n{}\r", to_ansi(&line(&help(v), width, theme::dim())))?;
    out.flush()?;
    Ok(())
}

fn status(text: &str, width: usize) -> Result<()> {
    let mut out = stdout().lock();
    writeln!(out, "{}\r", to_ansi(&line(text, width, theme::accent())))?;
    out.flush()?;
    Ok(())
}

pub(super) fn run(v: &mut DiffView) -> Result<Outcome> {
    let _input = Input::new()?;
    // The floating pane opens at a default size before Rex lays it out.
    let mut pending = Some((terminal::size().unwrap_or((100, 40)).0, Instant::now()));
    let mut width = 0;
    loop {
        if pending.is_some_and(|(_, at)| at.elapsed() >= SETTLE) {
            let w = pending.take().unwrap().0;
            if usize::from(w) != width {
                width = usize::from(w).max(1);
                v.last_width = width;
                print(&mut std::io::BufWriter::new(stdout().lock()), v, width)?;
            }
        }
        if !event::poll(super::POLL)? { continue }
        match event::read()? {
            Event::Resize(w, _) => pending = Some((w, Instant::now())),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(Outcome::Close);
                }
                if v.confirm_undo {
                    v.confirm_undo = false;
                    if key.code == KeyCode::Char('y') { return Ok(Outcome::Undo) }
                    status("Undo cancelled", width.max(1))?;
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(Outcome::Close),
                    KeyCode::Char('r') => {
                        let drafts = v.drafts.clone();
                        let layout = v.layout;
                        let result = run_review(v);
                        // ratatui restores raw mode on its way out. Resume the
                        // primary-screen reader without clearing its scrollback.
                        terminal::enable_raw_mode()?;
                        write!(stdout(), "\x1b[?25l")?;
                        stdout().flush()?;
                        v.last_width = width.max(1);
                        match result? {
                            Outcome::Close => {
                                v.mark = None;
                                v.horizontal = 0;
                                if v.drafts != drafts || v.layout != layout {
                                    print(&mut std::io::BufWriter::new(stdout().lock()), v, width.max(1))?;
                                }
                            }
                            outcome => return Ok(outcome),
                        }
                    }
                    KeyCode::Char('s') => {
                        v.layout = Some(v.layout().other());
                        print(&mut std::io::BufWriter::new(stdout().lock()), v, width.max(1))?;
                    }
                    KeyCode::Char('u') if v.data.can_undo => {
                        v.confirm_undo = true;
                        status(&format!("Undo turn {} of {}? y/n", v.data.turn, v.data.agent), width.max(1))?;
                    }
                    KeyCode::Enter | KeyCode::Char('[' | ']' | 'R') => {
                        if let Some(outcome) = v.handle_key(key) { return Ok(outcome) }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffData, FileEntry, parse_patch};

    #[test]
    fn primary_screen_contains_all_files_without_capturing_mouse_or_switching_screens() {
        let patches = parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old();\n+new();\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+other();\n");
        let files = patches.iter().map(|p| FileEntry { path: p.path.clone(), label: p.path.clone(), status: 'M', ..Default::default() }).collect();
        let mut v = DiffView::new(DiffData { patches, files, ..Default::default() }, vec![]);
        for width in [70, 180] {
            v.last_width = width;
            let mut out = Vec::new();
            print(&mut out, &v, width).unwrap();
            let out = String::from_utf8(out).unwrap();
            let text: String = render::scrollback_rows(&v, width).flat_map(|line| line.spans.into_iter().map(|s| s.content.into_owned())).collect();
            assert!(text.contains("a.rs") && text.contains("b.rs") && text.contains("old();") && text.contains("other();"));
            assert!(out.contains("r review lines"));
            for mode in ["?1049", "?1000", "?1002", "?1003", "?1006"] {
                assert!(!out.contains(mode), "the reader must leave scrolling and selection to the terminal");
            }
        }
    }
}
