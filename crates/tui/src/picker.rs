//! Recent threads: the pane under the launcher's prompt.  Pure: it consumes
//! [`KeyEvent`]s and produces [`PickAction`]s.  Type to filter, ↑↓ to move,
//! Enter to open; ↑ on the first row goes back to the prompt.  Ctrl+S
//! settles (or reopens), Ctrl+X archives, Ctrl+D twice deletes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::input::TextInput;
use crate::ThreadOp;
use crate::model::ThreadRow;
use crate::util::fuzzy_score;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickAction {
    None,
    Quit,
    /// Open this thread.
    Open(String),
    /// Move to the prompt pane above.
    FocusPrompt,
    /// Settle, reopen, archive or delete this thread.
    Manage(String, ThreadOp),
}

#[derive(Clone, Debug, Default)]
pub struct ThreadPicker {
    pub threads: Vec<ThreadRow>,
    pub filter: TextInput,
    /// Index into [`ThreadPicker::visible`].
    pub sel: usize,
    /// Thread waiting for a second Ctrl+D.
    pub confirm_delete: Option<String>,
    /// One line about the last action, or what the next key will do.
    pub notice: Option<String>,
    /// Whether settle/archive/delete are available.
    pub can_manage: bool,
}

impl ThreadPicker {
    pub fn new(threads: Vec<ThreadRow>) -> Self {
        Self { threads, ..Default::default() }
    }

    /// Replace the rows, keeping the cursor on the same thread when it's still there.
    pub fn set_threads(&mut self, threads: Vec<ThreadRow>) {
        let current = self.selected().map(|t| t.id.clone());
        self.threads = threads;
        let visible = self.visible();
        self.sel = current
            .and_then(|id| visible.iter().position(|&i| self.threads[i].id == id))
            .unwrap_or_else(|| self.sel.min(visible.len().saturating_sub(1)));
    }

    /// Indices of the threads matching the filter: recency order, or best
    /// match first while filtering.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.text();
        let mut scored: Vec<(u32, usize)> = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let hay = format!("{} {} {}", t.title, t.projects.join(" "), t.agents.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(" "));
                fuzzy_score(&needle, &hay).map(|s| (s, i))
            })
            .collect();
        if !needle.is_empty() {
            scored.sort_by_key(|(score, i)| (std::cmp::Reverse(*score), *i));
        }
        scored.into_iter().map(|(_, i)| i).collect()
    }

    pub fn selected(&self) -> Option<&ThreadRow> {
        self.visible().get(self.sel).map(|&i| &self.threads[i])
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return PickAction::Quit;
        }
        let n = self.visible().len();
        let armed = self.confirm_delete.take();
        if !(ctrl && key.code == KeyCode::Char('d')) {
            self.notice = None;
        }
        if self.can_manage && ctrl {
            let target = self.selected().map(|t| (t.id.clone(), t.title.clone(), t.settled));
            match (key.code, target) {
                (KeyCode::Char('s'), Some((id, _, settled))) => {
                    return PickAction::Manage(id, if settled { ThreadOp::Reopen } else { ThreadOp::Settle });
                }
                (KeyCode::Char('x'), Some((id, _, _))) => return PickAction::Manage(id, ThreadOp::Archive),
                (KeyCode::Char('d'), Some((id, title, _))) => {
                    if armed.as_deref() == Some(id.as_str()) {
                        self.notice = None;
                        return PickAction::Manage(id, ThreadOp::Delete);
                    }
                    self.notice = Some(format!("Ctrl+D again deletes “{title}”"));
                    self.confirm_delete = Some(id);
                    return PickAction::None;
                }
                (KeyCode::Char('s' | 'x' | 'd'), None) => return PickAction::None,
                _ => {}
            }
        }
        match key.code {
            KeyCode::Up if self.sel == 0 => return PickAction::FocusPrompt,
            KeyCode::BackTab => return PickAction::FocusPrompt,
            KeyCode::Up => self.sel -= 1,
            KeyCode::Char('p') if ctrl => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('n') if ctrl => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(t) = self.selected() {
                    return PickAction::Open(t.id.clone());
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.sel = 0;
            }
            KeyCode::Esc => return PickAction::Quit,
            _ => {
                if self.filter.handle_key(key) {
                    self.sel = 0;
                }
            }
        }
        PickAction::None
    }

    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        let esc = if self.filter.is_empty() { "close" } else { "clear" };
        let mut v = vec![("Enter", "open")];
        if self.can_manage {
            let settle = if self.selected().is_some_and(|t| t.settled) { "reopen" } else { "settle" };
            v.extend([("^S", settle), ("^X", "archive"), ("^D", "delete")]);
        }
        v.extend([("type", "filter"), ("Esc", esc)]);
        v
    }

    /// Open and settled counts, for the title line.
    pub fn counts(&self) -> (usize, usize) {
        let settled = self.threads.iter().filter(|t| t.settled).count();
        (self.threads.len() - settled, settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(id: &str, title: &str, updated_at: i64) -> ThreadRow {
        ThreadRow { id: id.into(), title: title.into(), updated_at, ..Default::default() }
    }

    fn picker() -> ThreadPicker {
        ThreadPicker::new(vec![row("t1", "Fix the login bug", 30), row("t2", "Remote hosts", 20), row("t3", "Composer polish", 10)])
    }

    #[test]
    fn enter_opens_the_selected_thread_and_up_at_the_top_goes_to_the_prompt() {
        let mut p = picker();
        assert_eq!(p.handle_key(key(KeyCode::Up)), PickAction::FocusPrompt);
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickAction::Open("t2".into()));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.sel, 2, "clamped at the last row");
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickAction::Quit);
    }

    #[test]
    fn typing_filters_and_esc_clears_the_filter_first() {
        let mut p = picker();
        for c in "remote".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.visible(), vec![1]);
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickAction::Open("t2".into()));
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickAction::None);
        assert_eq!(p.visible().len(), 3);
        for c in "zzz".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PickAction::None, "nothing to open");
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn settle_archive_and_a_confirmed_delete() {
        let mut p = picker();
        p.can_manage = true;
        assert_eq!(p.handle_key(ctrl('s')), PickAction::Manage("t1".into(), ThreadOp::Settle));
        p.threads[0].settled = true;
        assert_eq!(p.handle_key(ctrl('s')), PickAction::Manage("t1".into(), ThreadOp::Reopen), "settled: reopens");
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.handle_key(ctrl('x')), PickAction::Manage("t2".into(), ThreadOp::Archive));
        assert_eq!(p.handle_key(ctrl('d')), PickAction::None, "first Ctrl+D only asks");
        assert!(p.notice.as_deref().is_some_and(|n| n.contains("Remote hosts")));
        assert_eq!(p.handle_key(ctrl('d')), PickAction::Manage("t2".into(), ThreadOp::Delete));
        // Any other key disarms it.
        p.handle_key(ctrl('d'));
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.handle_key(ctrl('d')), PickAction::None);
        assert!(p.notice.as_deref().is_some_and(|n| n.contains("Composer")));
        // Without the callback, the keys do nothing to threads.
        let mut p = picker();
        assert_eq!(p.handle_key(ctrl('x')), PickAction::None);
    }

    #[test]
    fn a_refresh_keeps_the_cursor_on_the_same_thread() {
        let mut p = picker();
        p.handle_key(key(KeyCode::Down));
        p.set_threads(vec![row("t9", "New one", 40), row("t1", "Fix the login bug", 30), row("t2", "Remote hosts", 20)]);
        assert_eq!(p.selected().map(|t| t.id.as_str()), Some("t2"));
    }
}
