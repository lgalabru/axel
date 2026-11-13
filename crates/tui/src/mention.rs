//! Suggestions for the `@name` being typed, as a shell completes a command:
//! type `@` and the agents whose names match what follows show under the
//! input; ↑/↓ pick one, Tab or Enter puts it in, Esc hides the list.
//! Pure: it reads a [`TextInput`] and the agent names, and edits the input.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::input::TextInput;
use crate::model::AgentColor;
use crate::theme;
use crate::util::fuzzy_score;

/// The most suggestions shown at once.
pub const MAX_ROWS: usize = 5;

/// An agent an `@name` can name: its name and colour.
pub type Name = (String, Option<AgentColor>);

/// Agents matching `query`, best first: names starting with it, then names
/// containing it, then fuzzy matches. An empty query lists them all, in order.
pub fn matches(query: &str, names: &[Name]) -> Vec<Name> {
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, u32, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, (n, _))| {
            let lower = n.to_lowercase();
            if lower.starts_with(&q) {
                Some((0, 0, i))
            } else {
                fuzzy_score(&q, &lower).map(|s| (1, u32::MAX - s, i))
            }
        })
        .collect();
    scored.sort();
    scored.into_iter().map(|(_, _, i)| names[i].clone()).collect()
}

/// The suggestion list's state: which row is picked, and whether Esc hid it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MentionMenu {
    sel: usize,
    /// The `@` (its index) whose list Esc hid; typing another `@` shows lists again.
    dismissed: Option<usize>,
}

impl MentionMenu {
    /// What the list shows for `input` now: empty when the cursor isn't on an
    /// `@name`, Esc hid it, or the name is already typed in full.
    pub fn options(&self, input: &TextInput, names: &[Name]) -> Vec<Name> {
        let Some((at, query)) = input.mention_at_cursor() else { return Vec::new() };
        if self.dismissed == Some(at) {
            return Vec::new();
        }
        let found = matches(&query, names);
        if let [(only, _)] = found.as_slice()
            && only.eq_ignore_ascii_case(&query)
        {
            return Vec::new();
        }
        found
    }

    /// The highlighted row, within `n` options.
    pub fn selected(&self, n: usize) -> usize {
        self.sel.min(n.saturating_sub(1))
    }

    /// Feed a key: `true` when the list took it (move, accept, hide). Any
    /// other key is the input's, and starts the pick over at the top.
    pub fn handle_key(&mut self, key: KeyEvent, input: &mut TextInput, names: &[Name]) -> bool {
        let options = self.options(input, names);
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if !options.is_empty() {
            let n = options.len();
            let sel = self.selected(n);
            match key.code {
                KeyCode::Up if plain => self.sel = sel.checked_sub(1).unwrap_or(n - 1),
                KeyCode::Char('p') if ctrl => self.sel = sel.checked_sub(1).unwrap_or(n - 1),
                KeyCode::Down if plain => self.sel = (sel + 1) % n,
                KeyCode::Char('n') if ctrl => self.sel = (sel + 1) % n,
                KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                    if let Some((at, _)) = input.mention_at_cursor() {
                        input.complete_mention(at, &options[sel].0);
                    }
                    self.sel = 0;
                }
                KeyCode::Esc => {
                    self.dismissed = input.mention_at_cursor().map(|(at, _)| at);
                    self.sel = 0;
                }
                _ => {
                    self.typed(key);
                    return false;
                }
            }
            return true;
        }
        self.typed(key);
        false
    }

    fn typed(&mut self, key: KeyEvent) {
        self.sel = 0;
        if key.code == KeyCode::Char('@') {
            self.dismissed = None;
        }
    }

    /// Rows for the list, `width` wide, indented by `indent`: the picked one
    /// highlighted, scrolled to keep it in view; at most [`MAX_ROWS`].
    pub fn lines(&self, input: &TextInput, names: &[Name], indent: usize, width: usize) -> Vec<Line<'static>> {
        let options = self.options(input, names);
        let sel = self.selected(options.len());
        let first = sel.saturating_sub(MAX_ROWS - 1);
        let room = width.saturating_sub(indent + 1).max(1);
        options
            .iter()
            .enumerate()
            .skip(first)
            .take(MAX_ROWS)
            .map(|(i, (name, color))| {
                let label = crate::util::ellipsize(&format!("@{name}"), room);
                let style = Style::default().fg(theme::agent_color(*color));
                let (mark, style) = if i == sel { ("›", style.bold().reversed()) } else { (" ", style) };
                Line::from(vec![
                    Span::raw(" ".repeat(indent.saturating_sub(2))),
                    Span::styled(format!("{mark} "), theme::accent()),
                    Span::styled(format!(" {label} "), style),
                ])
            })
            .collect()
    }

    /// Rows the list takes for `input`.
    pub fn height(&self, input: &TextInput, names: &[Name]) -> u16 {
        self.options(input, names).len().min(MAX_ROWS) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<Name> {
        ["nova", "atlas", "nora", "orion"].iter().map(|n| (n.to_string(), None)).collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(m: &mut MentionMenu, input: &mut TextInput, s: &str) {
        for c in s.chars() {
            if !m.handle_key(key(KeyCode::Char(c)), input, &names()) {
                input.handle_key(key(KeyCode::Char(c)));
            }
        }
    }

    fn shown(m: &MentionMenu, input: &TextInput) -> Vec<String> {
        m.options(input, &names()).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn a_bare_at_lists_every_agent_and_letters_narrow_it() {
        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "@");
        assert_eq!(shown(&m, &input), ["nova", "atlas", "nora", "orion"]);
        type_str(&mut m, &mut input, "no");
        assert_eq!(shown(&m, &input), ["nova", "nora"], "names starting with it come first");
        type_str(&mut m, &mut input, "r");
        assert_eq!(shown(&m, &input), ["nora"]);
    }

    #[test]
    fn prefix_matches_rank_above_fuzzy_ones() {
        assert_eq!(matches("o", &names()).first().map(|n| n.0.as_str()), Some("orion"));
    }

    #[test]
    fn tab_or_enter_puts_the_picked_name_in_with_a_space() {
        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "ask @no");
        assert!(m.handle_key(key(KeyCode::Down), &mut input, &names()));
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &names()));
        assert_eq!(input.text(), "ask @nora ");
        assert!(shown(&m, &input).is_empty(), "the list closes once the name is in");

        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "@at");
        assert!(m.handle_key(key(KeyCode::Enter), &mut input, &names()));
        assert_eq!(input.text(), "@atlas ");
    }

    #[test]
    fn completing_in_the_middle_replaces_the_whole_word() {
        let mut input = TextInput::with_text("@nxx fix it");
        input.text_start();
        input.right();
        input.right();
        let mut m = MentionMenu::default();
        // the cursor after `@n` suggests n…, and the rest of the word goes
        assert_eq!(shown(&m, &input), ["nova", "nora", "orion"]);
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &names()));
        assert_eq!(input.text(), "@nova fix it");
        assert_eq!(input.cursor(), "@nova ".len());
    }

    #[test]
    fn esc_hides_the_list_until_another_at() {
        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "@n");
        assert!(m.handle_key(key(KeyCode::Esc), &mut input, &names()));
        assert!(shown(&m, &input).is_empty());
        type_str(&mut m, &mut input, "o");
        assert!(shown(&m, &input).is_empty(), "still hidden for this @");
        type_str(&mut m, &mut input, " @");
        assert_eq!(shown(&m, &input).len(), 4);
    }

    #[test]
    fn no_list_outside_a_mention_or_once_the_name_is_typed() {
        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "mail me@no");
        assert!(shown(&m, &input).is_empty(), "an @ inside a word isn't a mention");
        let mut input = TextInput::new();
        type_str(&mut m, &mut input, "@nova");
        assert!(shown(&m, &input).is_empty(), "nothing left to complete");
        type_str(&mut m, &mut input, " hi");
        assert!(shown(&m, &input).is_empty());
        assert!(!m.handle_key(key(KeyCode::Enter), &mut input, &names()), "Enter is the caller's again");
    }

    #[test]
    fn arrows_wrap_and_only_move_while_the_list_shows() {
        let (mut m, mut input) = (MentionMenu::default(), TextInput::new());
        type_str(&mut m, &mut input, "@");
        assert!(m.handle_key(key(KeyCode::Up), &mut input, &names()));
        assert_eq!(m.selected(4), 3, "up from the top wraps to the bottom");
        let mut plain = TextInput::with_text("hi");
        assert!(!m.handle_key(key(KeyCode::Up), &mut plain, &names()));
    }
}
