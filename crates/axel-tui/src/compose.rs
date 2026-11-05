//! The composer pane of a split thread: prompt, queue, pending approval and
//! mode footer, with the timeline streaming natively in the pane above
//! ([`crate::stream`]).  Same state machine as the full thread view
//! ([`ThreadView`] with `compose_only`), so sending, queueing, steering,
//! history and Shift+Tab modes behave identically.

use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::Deps;
use crate::glue;
use crate::thread::{Row, ThreadAction, ThreadView};
use crate::util::now_secs;
use crate::view::thread_view::{composer_height, queue_height, render_composer, render_queue, row_lines};

const POLL: Duration = Duration::from_millis(60);
const REFRESH: Duration = Duration::from_millis(300);
const RETRY_FIT: Duration = Duration::from_millis(400);

/// Run the composer for `thread_id` until Esc (empty composer) or Ctrl+C.
///
/// `fit(rows_wanted, rows_now)` is called at start, on resize, and whenever
/// the pane needs a different height (an approval card appears, the queue
/// grows), so the host can move the split border; Rex sizes splits by ratio.
/// `on_question(item_id)` is called once for each question an agent asks
/// (an `AskUserQuestion` waiting for you), so it can be put in front of you.
/// `toggle_log(session_id)` shows or minimizes that session's log pane (^L).
pub fn run_compose(deps: Deps, thread_id: &str, fit: &dyn Fn(u16, u16), on_question: &dyn Fn(&str), toggle_log: &dyn Fn(&str)) -> Result<()> {
    let owner_pk = glue::owner_pubkey(deps.keystore)?;
    let data = glue::load_thread(deps.store, &owner_pk, thread_id, now_secs())?;
    let mut v = ThreadView::new(thread_id, data);
    v.compose_only = true;
    let mut asked: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ask_new = |v: &ThreadView| {
        for id in new_questions(v, &asked) {
            on_question(&id);
            asked.insert(id);
        }
    };
    ask_new(&v);

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Tell Shift+Enter (a line break) apart from Enter (send).
    let keys = crate::util::enhance_keys();
    let result = (|| -> Result<()> {
        let mut last = Instant::now();
        // Keep the pane as tall as the composer wants (see `PaneFit`).
        let mut pane = PaneFit::default();
        loop {
            let size = terminal.size()?;
            let want = desired_height(&v, size.width);
            if let Some(ask) = pane.next(want, size.height, Instant::now()) {
                fit(ask, size.height);
            }
            terminal.draw(|f| render(f, &v))?;
            if event::poll(POLL)? {
                let ev = event::read()?;
                crate::util::log_key(&ev);
                match ev {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match v.handle_compose_key(key) {
                            ThreadAction::None => {}
                            ThreadAction::OpenLog(session_id) => toggle_log(&session_id),
                            ThreadAction::Quit | ThreadAction::Back => return Ok(()),
                            ThreadAction::Send { session_id, cmd } => {
                                let reply = (deps.send)(&session_id, cmd).map_err(|e| format!("{e:#}"));
                                v.on_reply(reply);
                                last = Instant::now() - REFRESH;
                            }
                            ThreadAction::Summon { thread_id, agent, text, attachments } => {
                                let reply = (deps.summon)(&thread_id, &agent, &text, &attachments).map_err(|e| format!("{e:#}"));
                                v.on_reply(reply.map(|_| crate::thread::SendReply::Ok));
                                last = Instant::now() - REFRESH;
                            }
                        }
                    }
                    Event::Paste(text) => v.handle_paste(&text),
                    _ => {}
                }
            }
            if last.elapsed() >= REFRESH {
                if let Ok(d) = glue::load_thread(deps.store, &owner_pk, thread_id, now_secs()) {
                    v.set_data(d);
                    ask_new(&v);
                }
                last = Instant::now();
            }
        }
    })();
    crate::util::restore_keys(keys);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Questions waiting for you that `asked` hasn't seen, oldest first.
pub fn new_questions(v: &ThreadView, asked: &std::collections::HashSet<String>) -> Vec<String> {
    v.data
        .items
        .iter()
        .filter(|it| it.kind == crate::model::ItemKind::Question && it.status == Some(crate::model::ItemStatus::Pending) && !asked.contains(&it.id))
        .map(|it| it.id.clone())
        .collect()
}

/// One question, alone: the floating pane the composer opens when an agent
/// asks you to choose.  ↑↓ and Enter (or a digit) answer it; Esc leaves it
/// for the composer.  Returns once it is answered, here or anywhere else.
pub fn run_answer(deps: Deps, thread_id: &str, item_id: &str) -> Result<()> {
    let owner_pk = glue::owner_pubkey(deps.keystore)?;
    let load = || glue::load_thread(deps.store, &owner_pk, thread_id, now_secs()).map(|d| only_question(d, item_id));
    let mut v = ThreadView::new(thread_id, load()?);
    v.compose_only = true;
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let mut last = Instant::now();
        loop {
            if v.pending_choice().is_none() {
                return Ok(());
            }
            terminal.draw(|f| render_answer(f, &v))?;
            if event::poll(POLL)?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                match answer_key(&mut v, key) {
                    AnswerKey::Leave => return Ok(()),
                    AnswerKey::Send(session_id, cmd) => {
                        let reply = (deps.send)(&session_id, cmd).map_err(|e| format!("{e:#}"));
                        let failed = matches!(reply, Ok(crate::thread::SendReply::Error { .. }) | Err(_));
                        v.on_reply(reply);
                        if !failed {
                            return Ok(());
                        }
                    }
                    AnswerKey::None => {}
                }
            }
            if last.elapsed() >= REFRESH {
                if let Ok(d) = load() {
                    v.set_data(d);
                }
                last = Instant::now();
            }
        }
    })();
    ratatui::restore();
    result
}

/// The thread as the answer pane sees it: only `item_id`, so it is the one
/// pending choice.
fn only_question(mut d: crate::model::ThreadData, item_id: &str) -> crate::model::ThreadData {
    d.items.retain(|it| it.id == item_id);
    d
}

/// What a key does in the answer pane.
#[derive(Debug, PartialEq, Eq)]
pub enum AnswerKey {
    None,
    /// Esc / Ctrl+C: answer later, from the composer.
    Leave,
    /// The answer, for this session.
    Send(String, crate::thread::SendCommand),
}

/// Only picking and answering: ↑↓, Enter, digits; Esc leaves.  Typing does
/// nothing here (in the composer it declines and redirects).
pub fn answer_key(v: &mut ThreadView, key: crossterm::event::KeyEvent) -> AnswerKey {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Esc => return AnswerKey::Leave,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return AnswerKey::Leave,
        KeyCode::Up | KeyCode::Down | KeyCode::Enter => {}
        KeyCode::Char(c) if c.is_ascii_digit() => {}
        _ => return AnswerKey::None,
    }
    match v.handle_compose_key(key) {
        ThreadAction::Send { session_id, cmd } => AnswerKey::Send(session_id, cmd),
        _ => AnswerKey::None,
    }
}

/// The question card, its options and the keys, from the top of the pane.
fn render_answer(frame: &mut Frame, v: &ThreadView) {
    let area = frame.area();
    let Some(i) = v.pending_choice() else { return };
    let inner = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(2) };
    let mut lines = row_lines(v, &Row::Choice(i), inner.width as usize, true);
    lines.push(Line::from(""));
    lines.push(Line::styled("↑↓ pick · Enter or 1–9 answer · Esc answer later in the composer", crate::theme::dim()));
    if let Some(st) = &v.status {
        lines.push(Line::styled(st.clone(), crate::theme::dim()));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Asking Rex for the composer pane's rows: at once when the composer's height
/// changes (the host works out the exact split from the panes' geometry),
/// then, should it still come out short or tall, correcting it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneFit {
    /// Rows the composer wants now.
    want: Option<u16>,
    /// Rows last asked.
    asked: u16,
    tries: u8,
    at: Option<Instant>,
}

impl PaneFit {
    /// The rows to ask for, if any, given the rows the composer wants and
    /// the pane's height now.
    pub fn next(&mut self, want: u16, height: u16, now: Instant) -> Option<u16> {
        if self.want != Some(want) {
            self.want = Some(want);
            self.tries = 0;
            self.asked = want;
            self.at = Some(now);
            return Some(self.asked);
        }
        // Give the last ask time to land before reading anything into the size.
        if self.at.is_some_and(|t| now.duration_since(t) < RETRY_FIT) {
            return None;
        }
        // One spare row is fine: Rex rounds, and chasing it would flip-flop.
        if height == want || height == want + 1 || self.tries >= 4 {
            return None;
        }
        // Ask for the same rows again: the host works the split out afresh
        // from the panes as they are now. (Adjusting by the miss compounds a
        // bad reading instead.)
        self.asked = want;
        self.tries += 1;
        self.at = Some(now);
        Some(self.asked)
    }
}

/// Rows the pane wants: composer, queue and (when pending) the approval card.
pub fn desired_height(v: &ThreadView, width: u16) -> u16 {
    // `composer_height` picks its text rows from the area height; ask as a tall pane.
    let composer = composer_height(v, Rect::new(0, 0, width, 40));
    let card = v.pending_choice().map_or(0, |i| row_lines(v, &Row::Choice(i), width.saturating_sub(1) as usize, true).len() as u16);
    composer + queue_height(v) + card
}

/// Bottom-anchored: [approval card] · [queue] · composer.
pub fn render(frame: &mut Frame, v: &ThreadView) {
    let area = frame.area();
    let composer_h = composer_height(v, area).min(area.height);
    let queue_h = queue_height(v).min(area.height.saturating_sub(composer_h));
    let card: Vec<Line> = match v.pending_choice() {
        Some(i) => row_lines(v, &Row::Choice(i), area.width.saturating_sub(1) as usize, true),
        None => Vec::new(),
    };
    let card_h = (card.len() as u16).min(area.height.saturating_sub(composer_h + queue_h));
    let [_, card_area, queue_area, composer_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(card_h),
        Constraint::Length(queue_h),
        Constraint::Length(composer_h),
    ])
    .areas(area);
    if card_h > 0 {
        // Keep the options (the end of the card) when the pane is short.
        let skip = card.len().saturating_sub(card_h as usize);
        let shown: Vec<Line> = card.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(shown), Rect { x: card_area.x + 1, width: card_area.width.saturating_sub(1), ..card_area });
    }
    if queue_h > 0 {
        render_queue(frame, v, queue_area);
    }
    render_composer(frame, v, composer_area);
}

#[cfg(test)]
mod answer_tests {
    use super::*;
    use crate::model::{ChoiceOption, ItemBody, ItemKind, ItemStatus};
    use crate::thread::{SendCommand, fixtures};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn question_data() -> crate::model::ThreadData {
        let mut d = fixtures::data();
        let mut q = fixtures::item(
            "q1",
            ItemKind::Question,
            ItemBody::Choice {
                prompt: "Database: Which one?".into(),
                options: vec![ChoiceOption { id: "answer:0".into(), label: "Postgres".into() }, ChoiceOption { id: "answer:1".into(), label: "SQLite".into() }],
                chosen: None,
            },
            9_000,
        );
        q.status = Some(ItemStatus::Pending);
        q.session_id = "S1".into();
        d.items.push(q);
        d
    }

    #[test]
    fn each_question_is_put_in_front_of_you_once() {
        let v = ThreadView::new("T1", question_data());
        let mut asked = std::collections::HashSet::new();
        assert_eq!(new_questions(&v, &asked), vec!["q1".to_string()], "the fixture's pending permission isn't a question");
        asked.insert("q1".to_string());
        assert!(new_questions(&v, &asked).is_empty());
    }

    #[test]
    fn the_answer_pane_answers_its_question_and_ignores_typing() {
        let mut v = ThreadView::new("T1", only_question(question_data(), "q1"));
        v.compose_only = true;
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(answer_key(&mut v, k(KeyCode::Char('x'))), AnswerKey::None, "typing does nothing here");
        assert!(v.composer.is_empty());
        assert_eq!(answer_key(&mut v, k(KeyCode::Down)), AnswerKey::None);
        assert_eq!(
            answer_key(&mut v, k(KeyCode::Enter)),
            AnswerKey::Send("S1".into(), SendCommand::Answer { item_id: "q1".into(), option_id: "answer:1".into() })
        );
        assert_eq!(answer_key(&mut v, k(KeyCode::Esc)), AnswerKey::Leave);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::fixtures;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_pane_is_asked_at_once_per_change_and_corrected_if_it_misses() {
        let t0 = Instant::now();
        let later = |ms| t0 + Duration::from_millis(ms);
        let mut f = PaneFit::default();
        // A change: asked at once, for exactly what's wanted.
        assert_eq!(f.next(6, 5, t0), Some(6));
        assert_eq!(f.next(6, 5, later(10)), None, "the resize may not have landed yet");
        // It missed by a row: asked again, worked out afresh.
        assert_eq!(f.next(6, 5, later(500)), Some(6));
        assert_eq!(f.next(6, 6, later(1_000)), None, "it fits");
        // Growing and shrinking ask for exactly what they need.
        assert_eq!(f.next(7, 6, later(1_010)), Some(7));
        assert_eq!(f.next(7, 7, later(1_500)), None);
        assert_eq!(f.next(6, 7, later(1_510)), Some(6), "no extra row carried into a shrink");
        // A spare row is left alone.
        assert_eq!(f.next(6, 7, later(3_000)), None);
        // Way too tall (a bad split): asked again for what's wanted, not
        // "wanted minus the miss", which would ask for nothing.
        let mut f = PaneFit::default();
        assert_eq!(f.next(4, 8, t0), Some(4));
        assert_eq!(f.next(4, 8, later(500)), Some(4));
    }

    #[test]
    fn a_new_line_doesnt_scroll_the_first_away_while_the_pane_grows() {
        // No approval card waiting: just the composer.
        let mut d = fixtures::data();
        for it in &mut d.items {
            if it.status == Some(crate::model::ItemStatus::Pending) {
                it.status = Some(crate::model::ItemStatus::Allowed);
            }
        }
        let mut v = ThreadView::new("T1", d);
        v.compose_only = true;
        v.composer.paste("1\n2\n3");
        let h = desired_height(&v, 60);
        let draw = |v: &ThreadView, h: u16| {
            let mut term = Terminal::new(TestBackend::new(60, h)).unwrap();
            term.draw(|f| render(f, v)).unwrap();
            text(&term)
        };
        let before = draw(&v, h);
        assert!(before.contains("› 1"), "{before}");
        // A fourth line, before the pane has grown: 1, 2 and 3 stay put.
        v.composer.insert_newline();
        v.composer.insert_char('4');
        assert_eq!(draw(&v, h), before, "nothing moves until the pane grows");
        // Grown: all four show.
        let after = draw(&v, h + 1);
        assert!(after.contains("› 1") && after.lines().any(|l| l.trim() == "4"), "{after}");
    }

    #[test]
    fn the_pane_it_asks_for_shows_every_line_of_the_message() {
        let mut v = ThreadView::new("T1", fixtures::data());
        v.compose_only = true;
        let one = desired_height(&v, 90);
        v.composer.paste("first line\nsecond line\nthird line");
        let three = desired_height(&v, 90);
        assert_eq!(three, one + 2, "two more lines, two more rows");
        // Drawn at exactly that size, all three lines show (it used to keep two).
        let mut term = Terminal::new(TestBackend::new(90, three)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let t = text(&term);
        assert!(t.contains("first line") && t.contains("second line") && t.contains("third line"), "{t}");
        // And the size it asks for doesn't change once it has it: no back and forth.
        assert_eq!(desired_height(&v, 90), three);
    }

    #[test]
    fn a_pane_shorter_than_the_composer_still_shows_the_input() {
        let mut d = fixtures::data();
        d.items[7].status = Some(crate::model::ItemStatus::Allowed);
        let mut v = ThreadView::new("T1", d);
        v.compose_only = true;
        for rows in 1..=3u16 {
            let mut term = Terminal::new(TestBackend::new(90, rows)).unwrap();
            term.draw(|f| render(f, &v)).unwrap();
            let t = text(&term);
            assert!(t.contains("message the thread"), "{rows} rows: {t}");
        }
        let mut term = Terminal::new(TestBackend::new(90, 3)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let t = text(&term);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("› ") && lines[1].starts_with('─') && lines[2].contains("autopilot"), "input · rule · footer, no rule above: {t}");
    }

    #[test]
    fn approval_is_answered_from_the_composer_pane() {
        let mut v = ThreadView::new("T1", fixtures::data());
        v.compose_only = true;
        let mut term = Terminal::new(TestBackend::new(70, 12)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let t = text(&term);
        assert!(t.contains("Atlas asks permission: Run cargo test") && t.contains("▸ Allow once"), "{t}");
        assert!(t.contains("1-9") && t.contains("pick"), "{t}");
        assert!(!t.contains("PgUp") && !t.contains("timeline keys"), "no timeline here: {t}");
        // ↓ chooses, a digit answers directly.
        assert_eq!(v.handle_compose_key(key(KeyCode::Down)), ThreadAction::None);
        assert_eq!(v.choice_index("p1"), 1);
        match v.handle_compose_key(key(KeyCode::Char('3'))) {
            ThreadAction::Send { cmd: crate::thread::SendCommand::Answer { option_id, .. }, .. } => assert_eq!(option_id, "reject"),
            other => panic!("expected answer, got {other:?}"),
        }
        v.on_reply(Ok(crate::thread::SendReply::Ok));
        // Typing still declines and redirects; Tab never lands on a timeline.
        v.handle_compose_key(key(KeyCode::Tab));
        assert_eq!(v.focus, crate::thread::Focus::Composer);
        v.handle_compose_key(key(KeyCode::Char('n')));
        assert_eq!(v.composer.text(), "n");
        // Empty composer, no approval: ↑ is history.
        let mut d = fixtures::data();
        d.items[7].status = Some(crate::model::ItemStatus::Allowed);
        let mut v = ThreadView::new("T1", d);
        v.compose_only = true;
        v.handle_compose_key(key(KeyCode::Up));
        assert_eq!(v.composer.text(), "Fix the login bug");
    }
}
