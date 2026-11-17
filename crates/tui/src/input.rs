//! A text editor driven by [`KeyEvent`]s: single-line fields and the
//! multi-line composer. Pure data: no rendering.
//!
//! The keys are what a macOS text field and readline both teach: move and
//! delete by character, word (Option / Ctrl) and line (Cmd, ^A ^E ^U ^K),
//! yank what was killed (^Y), undo and redo (⌘Z / ^Z, ⇧⌘Z).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::util::{cells, char_width};

/// How an edit groups for undo: consecutive typing is one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Typing,
    Other,
}

/// What a character is, for word motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// What an `@name` mention is made of.
pub fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// A character of a skill's name: a name character, or `:` and `.` (plugin
/// skills are `plugin:skill`).
pub fn is_skill_char(c: char) -> bool {
    is_name_char(c) || c == ':' || c == '.'
}

/// Steps kept for undo.
const UNDO_DEPTH: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    chars: Vec<char>,
    /// Cursor position in characters, `0..=chars.len()`.
    cursor: usize,
    /// (text, cursor) before each undoable step, oldest first.
    undo: Vec<(Vec<char>, usize)>,
    redo: Vec<(Vec<char>, usize)>,
    /// The last edit, to group typing; `None` after a move.
    last: Option<Edit>,
    /// What ^U ^K ^W ⌥⌫ last removed, for ^Y.
    killed: Vec<char>,
    /// Keep the desired column while walking through short screen rows.
    vertical_goal: Option<(usize, usize)>,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor, ..Default::default() }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the text (a recalled prompt, a reset): one undoable step.
    pub fn set_text(&mut self, text: &str) {
        self.save(Edit::Other);
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        if !self.chars.is_empty() {
            self.save(Edit::Other);
        }
        self.chars.clear();
        self.cursor = 0;
    }

    /// Remember the state before an edit. Typing right after typing joins
    /// the same step, until a space ends the word.
    fn save(&mut self, edit: Edit) {
        self.vertical_goal = None;
        if edit == Edit::Typing && self.last == Some(Edit::Typing) {
            return;
        }
        self.undo.push((self.chars.clone(), self.cursor));
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.last = Some(edit);
    }

    fn moved(&mut self) {
        self.last = None;
        self.vertical_goal = None;
    }

    pub fn undo(&mut self) {
        self.vertical_goal = None;
        if let Some((chars, cursor)) = self.undo.pop() {
            self.redo.push((std::mem::replace(&mut self.chars, chars), self.cursor));
            self.cursor = cursor;
            debug_assert!(self.cursor <= self.chars.len(), "a saved cursor fits its saved text");
        }
        self.last = None;
    }

    pub fn redo(&mut self) {
        self.vertical_goal = None;
        if let Some((chars, cursor)) = self.redo.pop() {
            self.undo.push((std::mem::replace(&mut self.chars, chars), self.cursor));
            self.cursor = cursor;
            debug_assert!(self.cursor <= self.chars.len(), "a saved cursor fits its saved text");
        }
        self.last = None;
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' || ch == '\r' {
            return;
        }
        assert!(self.cursor <= self.chars.len(), "the cursor is within the text");
        self.save(Edit::Typing);
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
        if ch.is_whitespace() {
            // A word typed is one undo step.
            self.last = None;
        }
    }

    /// The `@name` being typed at the cursor: the index of its `@` and the
    /// characters between it and the cursor. An `@` counts at the start of
    /// the text or after a character that can't be part of a name (so
    /// `me@host` isn't one).
    pub fn mention_at_cursor(&self) -> Option<(usize, String)> {
        let mut i = self.cursor;
        while i > 0 && is_name_char(self.chars[i - 1]) {
            i -= 1;
        }
        let at = i.checked_sub(1)?;
        if self.chars[at] != '@' || (at > 0 && is_name_char(self.chars[at - 1])) {
            return None;
        }
        Some((at, self.chars[at + 1..self.cursor].iter().collect()))
    }

    /// Replace the `@…` starting at `at` (the whole name, also what follows
    /// the cursor) with `@name` and a space: one undoable step.
    pub fn complete_mention(&mut self, at: usize, name: &str) {
        if self.chars.get(at) != Some(&'@') {
            return;
        }
        self.save(Edit::Other);
        let mut end = at + 1;
        while end < self.chars.len() && is_name_char(self.chars[end]) {
            end += 1;
        }
        let mut with: Vec<char> = std::iter::once('@').chain(name.chars()).collect();
        let spaced = self.chars.get(end).is_some_and(|c| c.is_whitespace());
        if !spaced {
            with.push(' ');
        }
        let n = with.len();
        self.chars.splice(at..end, with);
        self.cursor = at + n + usize::from(spaced);
        self.last = None;
    }

    /// The `/skill` word the cursor is in, if any: where its `/` is, and the
    /// name typed up to the cursor. It starts a word, so `a/b` isn't one, and
    /// a word with another `/` in it is a path, not a skill.
    pub fn skill_at_cursor(&self) -> Option<(usize, String)> {
        let mut i = self.cursor;
        while i > 0 && is_skill_char(self.chars[i - 1]) {
            i -= 1;
        }
        let at = i.checked_sub(1)?;
        if self.chars[at] != '/' || (at > 0 && !self.chars[at - 1].is_whitespace()) {
            return None;
        }
        if self.chars[self.cursor..].iter().take_while(|c| !c.is_whitespace()).any(|&c| c == '/') {
            return None;
        }
        Some((at, self.chars[at + 1..self.cursor].iter().collect()))
    }

    /// Replace the skill word starting at `at` (the whole word, also what
    /// follows the cursor) with `/name` and a space: one undoable step.
    pub fn complete_skill(&mut self, at: usize, name: &str) {
        if self.chars.get(at) != Some(&'/') {
            return;
        }
        self.save(Edit::Other);
        let mut end = at + 1;
        while end < self.chars.len() && is_skill_char(self.chars[end]) {
            end += 1;
        }
        let mut with: Vec<char> = std::iter::once('/').chain(name.chars()).collect();
        let spaced = self.chars.get(end).is_some_and(|c| c.is_whitespace());
        if !spaced {
            with.push(' ');
        }
        let n = with.len();
        self.chars.splice(at..end, with);
        self.cursor = at + n + usize::from(spaced);
        self.last = None;
    }

    /// A line break (Shift+Enter in the composer).
    pub fn insert_newline(&mut self) {
        self.save(Edit::Other);
        self.chars.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Insert pasted text on one line: line breaks and tabs become spaces
    /// (for single-line fields).
    pub fn insert_str(&mut self, s: &str) {
        self.save(Edit::Other);
        for ch in s.chars() {
            let ch = match ch {
                '\n' | '\r' | '\t' => ' ',
                c if c.is_control() => continue,
                c => c,
            };
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
        }
        self.last = None;
    }

    /// Insert pasted text keeping its line breaks (the composer); a tab is
    /// four spaces.
    pub fn paste(&mut self, s: &str) {
        self.save(Edit::Other);
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        for ch in s.chars() {
            match ch {
                '\n' => {
                    self.chars.insert(self.cursor, '\n');
                    self.cursor += 1;
                }
                '\t' => {
                    for _ in 0..4 {
                        self.chars.insert(self.cursor, ' ');
                        self.cursor += 1;
                    }
                }
                c if c.is_control() => {}
                c => {
                    self.chars.insert(self.cursor, c);
                    self.cursor += 1;
                }
            }
        }
        self.last = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.save(Edit::Other);
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.save(Edit::Other);
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.moved();
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.moved();
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// The start of the line the cursor is on.
    fn line_start(&self) -> usize {
        self.chars[..self.cursor].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1)
    }

    /// The end of the line the cursor is on (before its line break).
    fn line_end(&self) -> usize {
        self.chars[self.cursor..].iter().position(|c| *c == '\n').map_or(self.chars.len(), |i| self.cursor + i)
    }

    /// Home, ^A, ⌘←: the start of the line.
    pub fn home(&mut self) {
        self.moved();
        self.cursor = self.line_start();
    }

    /// End, ^E, ⌘→: the end of the line.
    pub fn end(&mut self) {
        self.moved();
        self.cursor = self.line_end();
    }

    /// Ctrl+Home, ⌘↑: the start of the text.
    pub fn text_start(&mut self) {
        self.moved();
        self.cursor = 0;
    }

    /// Ctrl+End, ⌘↓: the end of the text.
    pub fn text_end(&mut self) {
        self.moved();
        self.cursor = self.chars.len();
    }

    /// ↑ in a multi-line text: the same column on the line above.
    /// `false` on the first line (where ↑ means history).
    pub fn line_up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.cursor - start;
        let prev_end = start - 1;
        let prev_start = self.chars[..prev_end].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
        self.moved();
        self.cursor = (prev_start + col).min(prev_end);
        true
    }

    /// ↓ in a multi-line text: the same column on the line below.
    /// `false` on the last line (where ↓ means history).
    pub fn line_down(&mut self) -> bool {
        let end = self.line_end();
        if end >= self.chars.len() {
            return false;
        }
        let col = self.cursor - self.line_start();
        let next_start = end + 1;
        let next_end = self.chars[next_start..].iter().position(|c| *c == '\n').map_or(self.chars.len(), |i| next_start + i);
        self.moved();
        self.cursor = (next_start + col).min(next_end);
        true
    }

    /// Walk the same wrapped rows the composer draws, keeping the column in
    /// terminal cells (a CJK character takes two). Only the outermost rows
    /// return false, allowing the caller to browse prompt history.
    pub(crate) fn screen_line(&mut self, width: usize, up: bool) -> bool {
        let width = width.max(1);
        let (rows, (row, col)) = input_rows(&self.chars, width, self.cursor);
        let next = if up { row.checked_sub(1) } else { (row + 1 < rows.len()).then_some(row + 1) };
        let Some(next) = next else { return false };
        let goal = self.vertical_goal.filter(|&(w, _)| w == width).map_or(col, |(_, col)| col);
        let (start, end) = rows[next];
        // The end of a wrapped row belongs to the following row. Explicit
        // newlines and the end of the text can hold a cursor of their own.
        let end = if rows.get(next + 1).is_some_and(|&(s, _)| s == end) && end > start { end - 1 } else { end };
        self.moved();
        self.cursor = index_at_column(&self.chars[start..end], goal) + start;
        debug_assert!(self.cursor <= end, "the cursor stays on the row it moved to");
        self.vertical_goal = Some((width, goal));
        true
    }

    /// Remove `from..to`, keeping it for ^Y.
    fn kill(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        self.save(Edit::Other);
        self.killed = self.chars.drain(from..to).collect();
        self.cursor = from;
    }

    /// ^U, ⌘⌫: delete back to the start of the line (at its start, the line break).
    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        let from = if start == self.cursor { start.saturating_sub(1) } else { start };
        self.kill(from, self.cursor);
    }

    /// ^K: delete to the end of the line (at its end, the line break).
    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        let to = if end == self.cursor { (end + 1).min(self.chars.len()) } else { end };
        self.kill(self.cursor, to);
    }

    /// ^W, ⌥⌫: delete the word before the cursor.
    pub fn delete_word(&mut self) {
        let start = self.word_start_before(self.cursor);
        self.kill(start, self.cursor);
    }

    /// ⌥d, ⌥⌦: delete the word after the cursor.
    pub fn delete_word_forward(&mut self) {
        let end = self.word_end_after(self.cursor);
        self.kill(self.cursor, end);
    }

    /// ^Y: put back what was last deleted by word or line.
    pub fn yank(&mut self) {
        if self.killed.is_empty() {
            return;
        }
        self.save(Edit::Other);
        let k = self.killed.clone();
        let n = k.len();
        self.chars.splice(self.cursor..self.cursor, k);
        self.cursor += n;
        self.last = None;
    }

    /// Back over spaces, then over one run of word characters or of punctuation.
    fn word_start_before(&self, mut i: usize) -> usize {
        while i > 0 && class(self.chars[i - 1]) == Class::Space {
            i -= 1;
        }
        if i > 0 {
            let run = class(self.chars[i - 1]);
            while i > 0 && class(self.chars[i - 1]) == run {
                i -= 1;
            }
        }
        i
    }

    /// Forward over spaces, then over one run of word characters or of punctuation.
    fn word_end_after(&self, mut i: usize) -> usize {
        let n = self.chars.len();
        while i < n && class(self.chars[i]) == Class::Space {
            i += 1;
        }
        if i < n {
            let run = class(self.chars[i]);
            while i < n && class(self.chars[i]) == run {
                i += 1;
            }
        }
        i
    }

    /// ⌥←, ⌥b, ^←: to the start of the word.
    pub fn word_left(&mut self) {
        self.moved();
        self.cursor = self.word_start_before(self.cursor);
    }

    /// ⌥→, ⌥f, ^→: to the end of the word.
    pub fn word_right(&mut self) {
        self.moved();
        self.cursor = self.word_end_after(self.cursor);
    }

    /// Returns `true` when the key was consumed as an editing command.
    /// ↑/↓ are the caller's ([`Self::line_up`] / [`Self::line_down`]).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let cmd = m.contains(KeyModifiers::SUPER);
        let shift = m.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char(c) if cmd => match c.to_ascii_lowercase() {
                'z' if shift || c == 'Z' => self.redo(),
                'z' => self.undo(),
                _ => return false,
            },
            KeyCode::Char(c) if ctrl => match c {
                'u' => self.kill_to_start(),
                'k' => self.kill_to_end(),
                'w' => self.delete_word(),
                'y' => self.yank(),
                'a' => self.home(),
                'e' => self.end(),
                'b' => self.left(),
                'f' => self.right(),
                'h' => self.backspace(),
                'd' => self.delete(),
                'z' | 'Z' if shift || c == 'Z' => self.redo(),
                'z' | '_' | '/' => self.undo(),
                _ => return false,
            },
            // Option+←/→ arrive as ⌥b / ⌥f in most macOS terminals (readline).
            KeyCode::Char(c) if alt => match c {
                'b' => self.word_left(),
                'f' => self.word_right(),
                'd' => self.delete_word_forward(),
                _ => return false,
            },
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace if cmd => self.kill_to_start(),
            KeyCode::Backspace if alt || ctrl => self.delete_word(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete if alt || ctrl => self.delete_word_forward(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if cmd => self.home(),
            KeyCode::Right if cmd => self.end(),
            KeyCode::Left if alt || ctrl => self.word_left(),
            KeyCode::Right if alt || ctrl => self.word_right(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up if cmd => self.text_start(),
            KeyCode::Down if cmd => self.text_end(),
            KeyCode::Home if ctrl => self.text_start(),
            KeyCode::End if ctrl => self.text_end(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => return false,
        }
        true
    }

    /// Visible window of the text for a field `width` cells wide, and the
    /// cursor column (in cells) relative to that window. Keeps the cursor in
    /// view, with a cell after it; never shows half a wide character.
    pub fn view(&self, width: usize) -> (String, u16) {
        if width == 0 {
            return (String::new(), 0);
        }
        let inner = width.saturating_sub(1).max(1); // leave one cell for the cursor at the end
        // Back from the cursor while what is left of it fits in `inner`.
        let mut start = self.cursor;
        let mut before = 0usize;
        while start > 0 && before + char_width(self.chars[start - 1]) <= inner {
            start -= 1;
            before += char_width(self.chars[start]);
        }
        debug_assert!(start <= self.cursor && self.cursor <= self.chars.len());
        let mut shown = String::new();
        let mut used = 0usize;
        for &c in &self.chars[start..] {
            let w = char_width(c);
            if used + w > width {
                break;
            }
            shown.push(c);
            used += w;
        }
        let col = cells(before.min(width.saturating_sub(1)));
        (shown, col)
    }
}

/// The index in `chars` at which a cursor sits `col` cells in: the last one
/// whose characters before it fit in `col`.
fn index_at_column(chars: &[char], col: usize) -> usize {
    let mut used = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        used += char_width(c);
        if used > col {
            return i;
        }
    }
    chars.len()
}

/// Shared screen rows for composer rendering and vertical navigation: each
/// row a `(start, end)` range of `chars` at most `width` cells wide (a wide
/// character that doesn't fit starts the next row), and the cursor's (row,
/// column in cells). A cursor right after a full row gets a row of its own.
pub(crate) fn input_rows(chars: &[char], width: usize, cursor: usize) -> (Vec<(usize, usize)>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    loop {
        let line_end = chars[start..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| start + i);
        let mut s = start;
        loop {
            let (e, used) = row_end(chars, s, line_end, width);
            rows.push((s, e));
            if e == line_end && used >= width {
                rows.push((e, e));
            }
            if e >= line_end {
                break;
            }
            assert!(e > s, "every row takes at least one character");
            s = e;
        }
        if line_end >= chars.len() {
            break;
        }
        start = line_end + 1;
    }
    let mut at = None;
    for (r, &(s, e)) in rows.iter().enumerate() {
        let last_of_line = rows.get(r + 1).is_none_or(|&(ns, _)| ns != e);
        if cursor >= s && (cursor < e || (cursor == e && (last_of_line || e == s))) {
            at = Some((r, chars[s..cursor].iter().map(|&c| char_width(c)).sum()));
            break;
        }
    }
    let at = at.unwrap_or((rows.len().saturating_sub(1), 0));
    (rows, at)
}

/// Where a row starting at `start` ends, before `line_end`, to fit in
/// `width` cells, and the cells it takes. A character wider than the whole
/// row still takes one, so every row moves on.
fn row_end(chars: &[char], start: usize, line_end: usize, width: usize) -> (usize, usize) {
    let mut used = 0usize;
    let mut e = start;
    while e < line_end {
        let w = char_width(chars[e]);
        if used + w > width && e > start {
            break;
        }
        used += w;
        e += 1;
    }
    (e, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_and_backspace() {
        let mut t = TextInput::new();
        for c in "hello".chars() {
            assert!(t.handle_key(key(KeyCode::Char(c))));
        }
        assert_eq!(t.text(), "hello");
        t.handle_key(key(KeyCode::Backspace));
        assert_eq!(t.text(), "hell");
        assert_eq!(t.cursor(), 4);
    }

    #[test]
    fn cursor_movement_and_insert_in_middle() {
        let mut t = TextInput::with_text("ac");
        t.handle_key(key(KeyCode::Left));
        t.handle_key(key(KeyCode::Char('b')));
        assert_eq!(t.text(), "abc");
        t.handle_key(key(KeyCode::Home));
        assert_eq!(t.cursor(), 0);
        t.handle_key(key(KeyCode::End));
        assert_eq!(t.cursor(), 3);
        t.handle_key(key(KeyCode::Right)); // no-op at end
        assert_eq!(t.cursor(), 3);
    }

    #[test]
    fn ctrl_u_and_ctrl_w() {
        let mut t = TextInput::with_text("fix the login bug");
        t.handle_key(ctrl('w'));
        assert_eq!(t.text(), "fix the login ");
        t.handle_key(ctrl('w'));
        assert_eq!(t.text(), "fix the ");
        t.handle_key(ctrl('u'));
        assert_eq!(t.text(), "");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn ctrl_u_only_kills_before_cursor() {
        let mut t = TextInput::with_text("abcdef");
        t.left();
        t.left();
        t.handle_key(ctrl('u'));
        assert_eq!(t.text(), "ef");
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn paste_collapses_newlines() {
        let mut t = TextInput::new();
        t.insert_str("line one\nline two\t!");
        assert_eq!(t.text(), "line one line two !");
    }

    #[test]
    fn view_scrolls_to_cursor() {
        let t = TextInput::with_text("abcdefghij");
        let (shown, col) = t.view(5);
        assert_eq!(shown, "ghij");
        assert_eq!(col, 4);
        let (shown, col) = t.view(20);
        assert_eq!(shown, "abcdefghij");
        assert_eq!(col, 10);
    }

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn option_arrows_and_readline_keys_move_by_word() {
        let mut t = TextInput::with_text("fix the login-bug now");
        // ⌥← as the arrow, and as ESC b (what macOS terminals send).
        t.handle_key(k(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(t.cursor(), 18, "start of `now`");
        t.handle_key(k(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(t.cursor(), 14, "start of `bug`: punctuation splits words");
        t.handle_key(k(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(t.cursor(), 13, "the `-`");
        t.handle_key(k(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(t.cursor(), 14);
        t.handle_key(k(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(t.cursor(), 17, "end of `bug`");
        assert_eq!(t.text(), "fix the login-bug now", "moving types nothing");
    }

    #[test]
    fn cmd_and_ctrl_reach_line_and_text_ends() {
        let mut t = TextInput::with_text("one\ntwo three");
        t.handle_key(k(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), 4, "⌘←: start of this line");
        t.handle_key(k(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), 13, "⌘→: end of it");
        t.handle_key(k(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(t.cursor(), 4, "^A is the line too");
        t.handle_key(k(KeyCode::Up, KeyModifiers::SUPER));
        assert_eq!(t.cursor(), 0, "⌘↑: start of the text");
        t.handle_key(k(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(t.cursor(), 13);
    }

    #[test]
    fn deleting_by_word_and_line_and_yanking_it_back() {
        let mut t = TextInput::with_text("fix the login bug");
        t.handle_key(k(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(t.text(), "fix the login ");
        t.handle_key(k(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "fix the login bug", "^Y puts it back");
        t.text_start();
        t.handle_key(k(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(t.text(), " the login bug", "⌥d deletes the next word");
        let mut t = TextInput::with_text("one\ntwo");
        t.handle_key(k(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(t.text(), "one\n", "⌘⌫: to the start of this line only");
        t.handle_key(k(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(t.text(), "one", "at the start, it joins the line above");
        t.text_start();
        t.handle_key(k(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "");
    }

    #[test]
    fn undo_steps_back_a_word_at_a_time_and_redo_returns() {
        let mut t = TextInput::new();
        for c in "fix the".chars() {
            t.handle_key(k(KeyCode::Char(c), KeyModifiers::NONE));
        }
        t.handle_key(k(KeyCode::Char('z'), KeyModifiers::SUPER));
        assert_eq!(t.text(), "fix ", "⌘Z: the last word");
        t.handle_key(k(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "", "^Z: the one before");
        t.handle_key(k(KeyCode::Char('z'), KeyModifiers::SUPER | KeyModifiers::SHIFT));
        assert_eq!(t.text(), "fix ", "⇧⌘Z: redo");
        t.handle_key(k(KeyCode::Backspace, KeyModifiers::ALT));
        t.undo();
        assert_eq!(t.text(), "fix ", "a deletion is a step too");
    }

    #[test]
    fn up_and_down_walk_the_lines_keeping_the_column() {
        let mut t = TextInput::with_text("abcdef\nxy\nlonger line");
        assert!(!t.line_down(), "last line: ↓ is history");
        assert!(t.line_up());
        assert_eq!(t.cursor(), 9, "end of `xy`: the column is clamped");
        assert!(t.line_up());
        assert_eq!(t.cursor(), 2);
        assert!(!t.line_up(), "first line: ↑ is history");
        assert!(t.line_down() && t.line_down());
        assert_eq!(t.cursor(), 12);
    }

    #[test]
    fn screen_arrows_walk_wrapped_rows_and_restore_the_column_after_short_lines() {
        let mut t = TextInput::with_text("abcdefghij");
        assert!(t.screen_line(4, true));
        assert_eq!(t.cursor(), 6);
        assert!(t.screen_line(4, false));
        assert_eq!(t.cursor(), 10);

        let mut t = TextInput::with_text("abcdef\nxy\nlonger line");
        let end = t.cursor();
        assert!(t.screen_line(80, true));
        assert_eq!(t.cursor(), 9);
        assert!(t.screen_line(80, true));
        assert_eq!(t.cursor(), 6);
        assert!(t.screen_line(80, false) && t.screen_line(80, false));
        assert_eq!(t.cursor(), end, "short lines must not lose the desired column");
    }

    #[test]
    fn screen_arrows_can_return_to_the_cursor_row_after_an_exact_wrap() {
        let mut t = TextInput::with_text("abcdefgh");
        assert!(t.screen_line(4, true));
        assert_eq!(t.cursor(), 4);
        assert!(t.screen_line(4, true));
        assert_eq!(t.cursor(), 0);
        assert!(!t.screen_line(4, true));
        assert!(t.screen_line(4, false) && t.screen_line(4, false));
        assert_eq!(t.cursor(), 8);
        assert!(!t.screen_line(4, false));
        // The reserved final row exists even when the cursor is elsewhere.
        let chars: Vec<_> = "abcdefgh".chars().collect();
        assert_eq!(input_rows(&chars, 4, 0).0, input_rows(&chars, 4, 8).0);
    }

    #[test]
    fn vertical_motion_recalculates_the_column_after_a_resize_or_horizontal_move() {
        let mut t = TextInput::with_text("abcdefghijklmnopqrstuvwxyz");
        assert!(t.screen_line(4, true));
        assert_eq!(t.cursor(), 22);
        assert!(t.screen_line(8, false));
        assert_eq!(t.cursor(), 26);

        let mut t = TextInput::with_text("abcdef\nxy\nlonger line");
        assert!(t.screen_line(80, true));
        t.left();
        assert!(t.screen_line(80, false));
        assert_eq!(t.cursor(), 11, "left resets the desired column to 1");
    }

    #[test]
    fn chinese_rows_and_the_cursor_are_measured_in_cells() {
        let chars: Vec<char> = "修复登录错误".chars().collect();
        // Two characters (four cells) a row of five: the third would straddle the edge.
        assert_eq!(input_rows(&chars, 5, 6), (vec![(0, 2), (2, 4), (4, 6)], (2, 4)));
        assert_eq!(input_rows(&chars, 5, 3), (vec![(0, 2), (2, 4), (4, 6)], (1, 2)), "one character in: two cells");
        // A full row of four cells: the cursor after it gets its own row.
        assert_eq!(input_rows(&chars[..2], 4, 2), (vec![(0, 2), (2, 2)], (1, 0)));
        let mixed: Vec<char> = "ab修复".chars().collect();
        assert_eq!(input_rows(&mixed, 80, 4).1, (0, 6));
        // A row narrower than one wide character still moves on.
        assert_eq!(input_rows(&chars[..2], 1, 0).0, vec![(0, 1), (1, 2), (2, 2)]);

        // ↑ keeps the column in cells: from after `错` (8 cells) up to after `登录`.
        let mut t = TextInput::with_text("ab登录\n修复登录错误");
        t.left();
        assert!(t.screen_line(80, true));
        assert_eq!(t.cursor(), 4, "8 cells in on `ab登录` is its end");
        let mut t = TextInput::with_text("abcd\n修复登录");
        t.home();
        t.right();
        t.right();
        t.right();
        assert!(t.screen_line(80, true));
        assert_eq!(t.cursor(), 4, "the column is past the line's last character");
        let mut t = TextInput::with_text("abcdef\n修复登录");
        t.home();
        t.right();
        assert!(t.screen_line(80, true));
        assert_eq!(t.cursor(), 2, "after `修`, two cells in: after `ab`");

        // A one-line field: the window and the cursor column are in cells.
        let t = TextInput::with_text("修复登录错误");
        assert_eq!(t.view(5), ("错误".to_string(), 4));
        assert_eq!(t.view(20), ("修复登录错误".to_string(), 12));
    }

    /// The cases from issue #24 (CJK input in the launcher and composer).
    #[test]
    fn cjk_input_cases_from_issue_24() {
        // Eight Chinese characters: the cursor sits sixteen cells in, not eight.
        let t = TextInput::with_text("修复登录错误问题");
        assert_eq!(t.view(80), ("修复登录错误问题".to_string(), 16));
        assert_eq!(input_rows(&t.chars, 80, 8).1, (0, 16));
        // Four CJK characters in a four-cell field: two per row.
        let four: Vec<char> = "修复登录".chars().collect();
        assert_eq!(input_rows(&four, 4, 4).0, vec![(0, 2), (2, 4), (4, 4)]);
        // Mixed widths: `看a` and `看b` are three cells each.
        let mixed: Vec<char> = "看a看b".chars().collect();
        assert_eq!(input_rows(&mixed, 3, 4), (vec![(0, 2), (2, 4), (4, 4)], (2, 0)));
        assert_eq!(input_rows(&mixed, 3, 3), (vec![(0, 2), (2, 4), (4, 4)], (1, 2)), "after the second `看`");
        // Fields narrower than a wide character still end, one character a row.
        let wide: Vec<char> = "看看".chars().collect();
        assert_eq!(input_rows(&wide, 1, 2).0, vec![(0, 1), (1, 2), (2, 2)]);
        assert_eq!(input_rows(&wide, 3, 2).0, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn a_composer_paste_keeps_its_lines() {
        let mut t = TextInput::new();
        t.paste("line one\r\nline two\tend");
        assert_eq!(t.text(), "line one\nline two    end");
    }
}

