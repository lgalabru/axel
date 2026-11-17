//! Suggestions for the `/skill` being typed, as `@name` works: type `/` at
//! the start of a word and press Tab to list every installed skill; ↑/↓ pick
//! one, Tab or Enter puts it in, Esc hides the list. Any agent can be sent
//! any of them: its session host spells each the way its harness loads it,
//! or points it at the skill's file. Pure: it reads a [`TextInput`] and the
//! skills the glue found, and edits the input.

use axel_config::SkillHint;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

use crate::input::TextInput;
use crate::theme;
use crate::util::{cells, ellipsize, fuzzy_score, text_width};

/// The most suggestions shown at once.
pub const MAX_ROWS: usize = crate::mention::MAX_ROWS;

/// Skills matching `query`, best first: names starting with it, then fuzzy
/// matches. An empty query lists them all, in order.
pub fn matches<'a>(query: &str, skills: &'a [SkillHint]) -> Vec<&'a SkillHint> {
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, u32, usize)> = skills
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let lower = s.name.to_lowercase();
            if lower.starts_with(&q) { Some((0, 0, i)) } else { fuzzy_score(&q, &lower).map(|f| (1, u32::MAX - f, i)) }
        })
        .collect();
    scored.sort();
    scored.into_iter().map(|(_, _, i)| &skills[i]).collect()
}

/// The suggestion list's state: which skill word is open and which row is picked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillMenu {
    sel: usize,
    /// The `/` (its index) whose list was opened with Tab.
    opened: Option<usize>,
}

impl SkillMenu {
    /// What the list shows for `input` now: empty when the cursor isn't on a
    /// skill word, Tab hasn't opened it, or the name is already typed in full.
    pub fn options<'a>(&self, input: &TextInput, skills: &'a [SkillHint]) -> Vec<&'a SkillHint> {
        let Some((at, _)) = input.skill_at_cursor() else { return Vec::new() };
        if self.opened != Some(at) {
            return Vec::new();
        }
        Self::matching(input, skills)
    }

    fn matching<'a>(input: &TextInput, skills: &'a [SkillHint]) -> Vec<&'a SkillHint> {
        let Some((_, query)) = input.skill_at_cursor() else { return Vec::new() };
        let found = matches(&query, skills);
        if let [only] = found.as_slice()
            && only.name.eq_ignore_ascii_case(&query)
        {
            return Vec::new();
        }
        found
    }

    /// The highlighted row, within `n` options.
    pub fn selected(&self, n: usize) -> usize {
        self.sel.min(n.saturating_sub(1))
    }

    /// Feed a key: `true` when the list took it (open, move, accept, hide). Any
    /// other key is the input's, and starts the pick over at the top.
    pub fn handle_key(&mut self, key: KeyEvent, input: &mut TextInput, skills: &[SkillHint]) -> bool {
        let at = input.skill_at_cursor().map(|(at, _)| at);
        if self.opened != at {
            self.opened = None;
        }
        if key.code == KeyCode::Tab && key.modifiers.is_empty() && self.opened.is_none() && !Self::matching(input, skills).is_empty() {
            self.opened = at;
            self.sel = 0;
            return true;
        }
        let options = self.options(input, skills);
        if options.is_empty() {
            self.sel = 0;
            return false;
        }
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let n = options.len();
        let sel = self.selected(n);
        match key.code {
            KeyCode::Up if plain => self.sel = sel.checked_sub(1).unwrap_or(n - 1),
            KeyCode::Char('p') if ctrl => self.sel = sel.checked_sub(1).unwrap_or(n - 1),
            KeyCode::Down if plain => self.sel = (sel + 1) % n,
            KeyCode::Char('n') if ctrl => self.sel = (sel + 1) % n,
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                if let Some(at) = at {
                    let name = options[sel].name.clone();
                    input.complete_skill(at, &name);
                }
                self.opened = None;
                self.sel = 0;
            }
            KeyCode::Esc => {
                self.opened = None;
                self.sel = 0;
            }
            _ => {
                self.sel = 0;
                return false;
            }
        }
        true
    }

    /// Rows for the list, `width` wide, indented by `indent`: each skill's
    /// `/name`, then its description, dim; the picked row highlighted,
    /// scrolled to keep it in view; at most [`MAX_ROWS`].
    pub fn lines(&self, input: &TextInput, skills: &[SkillHint], indent: usize, width: usize) -> Vec<Line<'static>> {
        let options = self.options(input, skills);
        let sel = self.selected(options.len());
        let first = sel.saturating_sub(MAX_ROWS - 1);
        let room = width.saturating_sub(indent + 1).max(1);
        options
            .iter()
            .enumerate()
            .skip(first)
            .take(MAX_ROWS)
            .map(|(i, s)| {
                let label = ellipsize(&format!("/{}", s.name), room.saturating_sub(2).max(1));
                let style = if i == sel { theme::bold().reversed() } else { theme::bold() };
                let mut spans = vec![
                    Span::raw(" ".repeat(indent.saturating_sub(2))),
                    Span::styled(if i == sel { "› " } else { "  " }, theme::accent()),
                    Span::styled(format!(" {label} "), style),
                ];
                let left = room.saturating_sub(text_width(&label) + 4);
                if let Some(d) = s.description.as_deref().filter(|_| left > 4) {
                    spans.push(Span::styled(format!(" {}", ellipsize(d, left)), theme::dim()));
                }
                Line::from(spans)
            })
            .collect()
    }

    /// Rows the list takes for `input`.
    pub fn height(&self, input: &TextInput, skills: &[SkillHint]) -> u16 {
        cells(self.options(input, skills).len().min(MAX_ROWS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axel_config::SkillScope;

    fn skills() -> Vec<SkillHint> {
        let skill = |name: &str, d: &str| SkillHint { name: name.into(), description: Some(d.into()), scope: SkillScope::User, path: format!("/s/{name}/SKILL.md").into() };
        vec![skill("rex", "Drive Rex"), skill("review", "Review a diff"), skill("writing:plain-english", "Plain prose")]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_lists_the_matching_skills_and_enter_puts_one_in() {
        let s = skills();
        let mut m = SkillMenu::default();
        let mut input = TextInput::with_text("please /re");
        assert!(m.options(&input, &s).is_empty(), "closed until Tab");
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &s));
        let shown: Vec<&str> = m.options(&input, &s).iter().map(|s| s.name.as_str()).collect();
        assert_eq!(shown[..2], ["rex", "review"], "names starting with it first: {shown:?}");
        assert!(m.handle_key(key(KeyCode::Down), &mut input, &s));
        assert!(m.handle_key(key(KeyCode::Enter), &mut input, &s));
        assert_eq!(input.text(), "please /review ");
        assert!(m.options(&input, &s).is_empty());
    }

    #[test]
    fn an_empty_name_lists_every_skill_and_esc_hides_them() {
        let s = skills();
        let mut m = SkillMenu::default();
        let mut input = TextInput::with_text("/");
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &s));
        assert_eq!(m.options(&input, &s).len(), 3);
        assert!(m.handle_key(key(KeyCode::Esc), &mut input, &s));
        assert!(m.options(&input, &s).is_empty());
        let mut input = TextInput::with_text("/plain");
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &s));
        assert!(m.handle_key(key(KeyCode::Tab), &mut input, &s));
        assert_eq!(input.text(), "/writing:plain-english ", "a plugin skill, by fuzzy match");
    }

    #[test]
    fn paths_and_mid_word_slashes_are_not_skills() {
        let s = skills();
        let mut m = SkillMenu::default();
        for text in ["/Users/dylan", "and/or", "/Users/", "$re"] {
            let mut input = TextInput::with_text(text);
            assert!(!m.handle_key(key(KeyCode::Tab), &mut input, &s), "{text}");
        }
        let mut input = TextInput::with_text("/rex");
        assert!(!m.handle_key(key(KeyCode::Tab), &mut input, &s), "already typed in full: Tab is the composer's");
    }
}
