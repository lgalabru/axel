//! Rendering. Views are pure functions of state onto a ratatui [`Frame`].

pub mod chart;
pub mod dashboard_view;
pub mod inbox_view;
pub mod launcher_view;
pub mod picker_view;
pub mod markdown;
pub mod thread_view;
pub mod usage_view;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme;

/// A `width`×`height` rectangle centred in `area`, clamped to fit.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Footer help line: `key action · key action`.
pub fn help_line(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 3);
    for (i, (key, what)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled((*key).to_string(), theme::accent()));
        spans.push(Span::styled(format!(" {what}"), theme::dim()));
    }
    Line::from(spans)
}

/// Attachment chips as spans: dim, with the selected one inverted and marked `✕`.
pub fn chips_spans(attachments: &[std::path::PathBuf], width: usize, selected: Option<usize>) -> Vec<Span<'static>> {
    let (parts, hidden) = crate::attach::chip_parts(attachments, width, selected);
    let mut spans = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if p.selected {
            Style::default().bg(theme::ACCENT).fg(ratatui::style::Color::Black)
        } else {
            theme::dim()
        };
        spans.push(Span::styled(p.text.clone(), style));
    }
    if hidden > 0 {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("+{hidden}"), theme::dim()));
    }
    spans
}

/// A one-line text field: shows the visible window and the cursor position
/// the caller should set when the field is focused.
pub fn field_line(text_view: &str, placeholder: &str, focused: bool) -> Line<'static> {
    if text_view.is_empty() && !placeholder.is_empty() {
        return Line::from(Span::styled(placeholder.to_string(), theme::dim()));
    }
    let style = if focused { Style::default() } else { theme::dim() };
    Line::from(Span::styled(text_view.to_string(), style))
}

#[cfg(test)]
mod tests {
    //! Render smoke tests: every screen at 80x24, a narrow 60x18, and a wide
    //! 140x40 frame. They catch layout panics and verify key text is present.
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::dashboard::Dashboard;
    use crate::inbox::Inbox;
    use crate::launcher::Launcher;
    use crate::model::*;

    fn text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        // The cell after the double-width 📎 is a blank placeholder; collapse it so
        // assertions can match the text as a terminal shows it.
        out.replace("📎  ", "📎 ")
    }

    fn agent(name: &str) -> AgentRow {
        AgentRow {
            pubkey: format!("PK_{name}"),
            name: name.into(),
            harness: Harness::Claude,
            model: Some("sonnet".into()),
            color: Some(AgentColor::Purple),
            runtime_mode: RuntimeMode::Autopilot,
            last_seen_at: Some(1_000),
            unread: 3,
        }
    }

    fn launcher() -> Launcher {
        Launcher::new(
            PathBuf::from("/work/axel-next"),
            vec![ProjectRow { name: "pay-kit".into(), path: PathBuf::from("/work/pay-kit") }],
            vec![agent("atlas"), agent("nova")],
            vec!["claude".into(), "codex".into()],
        )
    }

    fn dashboard() -> Dashboard {
        Dashboard::new(DashboardData {
            threads: vec![
                ThreadRow {
                    id: "T1".into(),
                    title: "Fix login".into(),
                    state: StatusState::Working,
                    agents: vec![("Atlas".into(), Some(AgentColor::Purple)), ("Nova".into(), Some(AgentColor::Green))],
                    projects: vec!["axel-next".into()],
                    environments: vec!["macbook".into()],
                    sessions_total: 2,
                    sessions_live: 2,
                    tokens: 12_400,
                    spark: vec![1, 2, 5, 3, 8, 2, 1, 0, 4, 6],
                    updated_at: 4_000,
                    focus_session: Some("S1".into()),
                    settled: false,
                },
                ThreadRow { id: "T2".into(), title: "Scratch ideas".into(), state: StatusState::Blocked, ..Default::default() },
            ],
            agents: vec![
                DashAgentRow {
                    pubkey: "PK".into(),
                    name: "Atlas".into(),
                    color: Some(AgentColor::Purple),
                    harness: Harness::Claude,
                    runtime_mode: RuntimeMode::Autopilot,
                    state: StatusState::Working,
                    thread_id: Some("T1".into()),
                    thread_title: Some("Fix login".into()),
                    environment: "macbook".into(),
                    tokens: 12_400,
                    spark: vec![1, 2, 5, 3, 8, 2, 1, 0, 4, 6],
                    last_event_at: Some(4_000),
                    active_session_id: Some("S1".into()),
                },
                DashAgentRow { name: "Nova".into(), state: StatusState::Blocked, ..Default::default() },
            ],
            tokens: TokensPanel { series: (0..60).map(|i| (i * 7 % 23) as u64).collect(), input: 310_000, output: 42_000, cache_read: 1_200_000, cache_write: 0, cost_usd: 1.84 },
            requests: vec![RequestRow {
                id: "R1".into(),
                from_pubkey: "PK".into(),
                agent_name: "Atlas".into(),
                kind: RequestKind::Question,
                content: "pick ORM: sqlx or diesel?".into(),
                thread_id: Some("T1".into()),
                session_id: Some("S1".into()),
                thread_title: Some("Fix login".into()),
                created_at: 4_000,
            }],
            messages: vec![MessageRow {
                id: "M1".into(),
                from_pubkey: "PK".into(),
                from_name: "Nova".into(),
                kind: 20,
                kind_label: "status".into(),
                content: "turn finished".into(),
                thread_id: None,
                created_at: 3_900,
            }],
            unread: 1,
        })
    }

    fn sizes() -> Vec<(u16, u16)> {
        vec![(80, 24), (60, 18), (140, 40), (100, 30), (40, 10)]
    }

    #[test]
    fn the_launcher_prompt_shows_its_lines_and_scrolls_to_the_cursor() {
        let mut l = launcher();
        l.title.paste("fix the login bug\nit fails on token refresh");
        let mut term = Terminal::new(TestBackend::new(60, 5)).unwrap();
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("› fix the login bug") && t.contains("  it fails on token refresh"), "{t}");
        assert!(!t.contains("atlas ·"), "who/where step aside for a multi-line prompt: {t}");
        // A pane with fewer rows than lines shows the ones up to the cursor.
        l.title.paste("\nand logout too\nand signup");
        let mut term = Terminal::new(TestBackend::new(60, 3)).unwrap();
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("and signup") && !t.contains("fix the login bug"), "four lines in three rows: the cursor's last three: {t}");
    }

    #[test]
    fn a_pane_one_row_short_drops_the_blank_row_not_line_one() {
        let mut l = launcher();
        l.title.paste("1\n2\n3\n4\n5\n6\n7");
        // Seven lines, seven rows (the blank row above doesn't fit).
        let mut term = Terminal::new(TestBackend::new(60, 7)).unwrap();
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        let lines: Vec<&str> = t.lines().map(str::trim).collect();
        assert_eq!(lines, vec!["› 1", "2", "3", "4", "5", "6", "7"], "{t}");
    }

    #[test]
    fn a_growing_launcher_keeps_its_lines_in_place_while_the_pane_catches_up() {
        let mut l = launcher();
        l.pane_grows = true;
        l.title.paste("1\n2\n3\n4");
        let draw = |l: &Launcher, h: u16| {
            let mut term = Terminal::new(TestBackend::new(40, h)).unwrap();
            term.draw(|f| super::launcher_view::render(f, l, 5_000)).unwrap();
            text(&term).lines().map(|x| x.trim().to_string()).collect::<Vec<_>>()
        };
        // The pane fits four lines (and the blank row above them).
        let before = draw(&l, 5);
        assert_eq!(before, vec!["", "› 1", "2", "3", "4"]);
        // Shift+Enter: a fifth line, before the pane has grown. Nothing moves.
        l.title.insert_newline();
        l.title.insert_char('5');
        assert_eq!(draw(&l, 5), before, "the lines stay where they were");
        // The pane grows: the new line appears under them.
        assert_eq!(draw(&l, 6), vec!["", "› 1", "2", "3", "4", "5"]);
    }

    #[test]
    fn the_launcher_pane_asks_for_a_row_per_line() {
        let mut l = launcher();
        let one = super::launcher_view::prompt_pane_rows(&l, 80);
        l.title.paste("a\nb\nc");
        assert_eq!(super::launcher_view::prompt_pane_rows(&l, 80), one + 2);
        l.title.paste(&"\nmore".repeat(20));
        assert_eq!(super::launcher_view::prompt_pane_rows(&l, 80), 1 + super::thread_view::MAX_INPUT_ROWS as u16, "then it scrolls");
    }

    #[test]
    fn launcher_renders_every_step_at_every_size() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for (w, h) in sizes() {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut l = launcher();
            // step 1
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            assert!(text(&term).contains("What are we building?"), "{w}x{h}");
            for c in "Fix login".chars() {
                l.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            l.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("Where?"), "{w}x{h}\n{t}");
            assert!(t.contains("Other path"), "{w}x{h}\n{t}");
            assert!(t.contains("◻"), "{w}x{h}\n{t}");
            // toggle cwd, draw with the checkbox on
            l.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("◼") && t.contains("1 project selected"), "{w}x{h}\n{t}");
            // path mode and back
            l.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            l.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // confirm toggled set from Other path row? no: Enter on Other opens path mode
            l.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // wrap to row 0
            l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("Who?"), "{w}x{h}\n{t}");
            assert!(t.contains("+ New agent"), "{w}x{h}\n{t}");
            if w >= 80 {
                assert!(t.contains("atlas") && t.contains("✉ 3"), "{w}x{h}\n{t}");
            }
            // new agent form
            l.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("New agent") && t.contains("Harness"), "{w}x{h}\n{t}");
            l.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            // workspace, both radio states
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            assert!(text(&term).contains("In place"), "{w}x{h}");
            l.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            assert!(text(&term).contains("axel/fix-login"), "{w}x{h}");
            l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("Enter") && t.contains("launch"), "{w}x{h}\n{t}");
            assert!(t.contains("5/5"), "{w}x{h}\n{t}");
        }
    }

    #[test]
    fn launcher_prompt_colours_mentions_of_known_agents() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut term = Terminal::new(TestBackend::new(80, 6)).unwrap();
        let mut l = launcher();
        for c in "test @nova and @ghost".chars() {
            l.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = (0..buf.area.height).find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "›")).unwrap();
        let line: String = (0..buf.area.width).map(|x| buf[(x, row)].symbol().to_string()).collect();
        let at = |needle: &str| line.find(needle).map(|i| line[..i].chars().count() as u16).unwrap();
        let nova = l.agents.iter().find(|a| a.name == "nova").unwrap().color;
        assert_eq!(buf[(at("@nova"), row)].fg, crate::theme::agent_color(nova), "{line}");
        assert_ne!(buf[(at("@ghost"), row)].fg, crate::theme::agent_color(nova), "unknown names stay plain: {line}");
    }

    #[test]
    fn launcher_is_edge_to_edge_in_a_short_strip() {
        // The Rex floating pane is the frame: no box, just the question with
        // who/where on the right and the input, even at 3 rows.
        for (w, h) in [(80u16, 24u16), (120, 8), (60, 3)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let l = launcher();
            term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
            let t = text(&term);
            let lines: Vec<&str> = t.lines().collect();
            assert!(!t.contains('╭') && !t.contains('╰'), "{w}x{h}\n{t}");
            let input = lines.iter().find(|r| r.contains('›')).expect("input row");
            assert!(input.contains("What are we building?"), "the question is the placeholder: {w}x{h}\n{t}");
            if w >= 60 {
                assert!(input.contains("nova"), "who Enter launches, right of the input: {w}x{h}\n{t}");
            }
            assert!(!t.contains("Enter") && !t.contains("1/5"), "no help or step counter: {w}x{h}\n{t}");
        }
    }

    #[test]
    fn dashboard_renders_in_every_mode_without_panicking() {
        for (w, h) in sizes() {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut d = dashboard();
            d.compact = super::dashboard_view::mode_for(ratatui::layout::Rect::new(0, 0, w, h))
                == super::dashboard_view::Mode::Compact;
            term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("agents"), "{w}x{h}\n{t}");
            assert!(t.contains("Atlas"), "{w}x{h}\n{t}");
        }
        // empty data must not panic in any mode either
        for (w, h) in [(40, 10), (80, 20), (120, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let d = Dashboard::new(DashboardData::default());
            term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
            let t = text(&term);
            if w >= 70 {
                assert!(t.contains("No open threads") && t.contains("No agents yet"), "{w}x{h}\n{t}");
            } else {
                assert!(t.contains("no agents"), "{w}x{h}\n{t}");
            }
        }
    }

    #[test]
    fn dashboard_mode_thresholds() {
        use super::dashboard_view::{Mode, mode_for};
        let r = |w, h| mode_for(ratatui::layout::Rect::new(0, 0, w, h));
        assert_eq!(r(40, 15), Mode::Compact);
        assert_eq!(r(44, 18), Mode::Compact);
        assert_eq!(r(69, 30), Mode::Compact, "narrow is compact however tall");
        assert_eq!(r(120, 15), Mode::Compact, "short is compact however wide");
        assert_eq!(r(70, 16), Mode::Medium);
        assert_eq!(r(80, 20), Mode::Medium);
        assert_eq!(r(99, 40), Mode::Medium);
        assert_eq!(r(100, 24), Mode::Medium);
        assert_eq!(r(100, 25), Mode::Full);
        assert_eq!(r(120, 40), Mode::Full);
    }

    #[test]
    fn dashboard_compact_40x15() {
        let mut term = Terminal::new(TestBackend::new(40, 15)).unwrap();
        let mut d = dashboard();
        d.compact = true;
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        let first = t.lines().next().unwrap();
        // Title carries totals, cost and the pending count; no threads box anywhere.
        assert!(first.contains("agents · 352k · $1.84 · ? 1"), "{t}");
        assert!(!t.contains("threads"), "{t}");
        assert!(!t.contains("tokens (1h)") && !t.contains("╭ inbox"), "{t}");
        // One row per agent with glyph, name and right-aligned tokens; a sparkline fits at 40 cols.
        assert!(t.contains("● Atlas"), "{t}");
        assert!(t.contains("◐ Nova"), "{t}");
        assert!(t.contains("12k"), "{t}");
        let atlas = t.lines().find(|l| l.contains("Atlas")).unwrap();
        assert!(atlas.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "sparkline expected: {atlas}");
        // Single dim footer line.
        assert!(t.contains("n new · i inbox · q"), "{t}");
        // Flash replaces the footer.
        d.set_flash("Atlas has no live session", 5_000);
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("Atlas has no live session") && !t.contains("n new · i inbox"), "{t}");
    }

    #[test]
    fn dashboard_compact_44x18_overflow_and_tiny_sizes() {
        // More agents than rows: most urgent first, "+N more" last.
        let mut d = dashboard();
        let mut agents: Vec<DashAgentRow> = Vec::new();
        for i in 0..30 {
            agents.push(DashAgentRow { name: format!("agent{i:02}"), tokens: 1000 * i, ..Default::default() });
        }
        d.data.agents = agents;
        d.compact = true;
        let mut term = Terminal::new(TestBackend::new(44, 18)).unwrap();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        // 18 rows - footer 1 - borders 2 = 15 inner rows: 14 agents + "+16 more".
        assert!(t.contains("agent00") && t.contains("agent13"), "{t}");
        assert!(!t.contains("agent14"), "{t}");
        assert!(t.contains("+16 more"), "{t}");
        // Selecting a hidden agent scrolls it into view.
        d.agent_sel = 29;
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("agent29"), "{t}");

        // Below 8 rows the footer disappears; a 6-cell sparkline still fits at 30 cols.
        let d = dashboard();
        let mut term = Terminal::new(TestBackend::new(30, 7)).unwrap();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(!t.contains("n new"), "{t}");
        assert!(t.contains("Atlas") && t.contains("12k"), "{t}");
        let atlas = t.lines().find(|l| l.contains("Atlas")).unwrap();
        assert!(atlas.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "sparkline at 30 cols: {atlas}");
        // Narrower: the sparkline goes first, tokens stay.
        let mut term = Terminal::new(TestBackend::new(22, 7)).unwrap();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("Atlas") && t.contains("12k"), "{t}");
        let atlas = t.lines().find(|l| l.contains("Atlas")).unwrap();
        assert!(!atlas.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "no sparkline at 22 cols: {atlas}");
        let mut term = Terminal::new(TestBackend::new(14, 6)).unwrap();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("Atlas") && !t.contains("12k"), "{t}");
    }

    #[test]
    fn dashboard_medium_80x20() {
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let d = dashboard();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("╭ threads") && t.contains("╭ agents"), "{t}");
        assert!(!t.contains("tokens (1h)") && !t.contains("╭ inbox"), "{t}");
        // stacked: the threads title is above the agents title, not on the same row
        let threads_row = t.lines().position(|l| l.contains("threads")).unwrap();
        let agents_row = t.lines().position(|l| l.contains("agents")).unwrap();
        assert!(threads_row < agents_row, "{t}");
        assert!(t.contains("Fix login") && t.contains("Atlas Nova"), "{t}");
        assert!(t.contains("new thread") && t.contains("spawn agent"), "{t}");
    }

    #[test]
    fn dashboard_full_120x40() {
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let d = dashboard();
        term.draw(|f| super::dashboard_view::render(f, &d, 5_000)).unwrap();
        let t = text(&term);
        let first = t.lines().next().unwrap();
        assert!(first.contains("threads") && first.contains("agents"), "{t}");
        assert!(t.contains("tokens (1h)") && t.contains("inbox"), "{t}");
        assert!(t.contains("$1.84 est"), "{t}");
        assert!(t.contains("? Atlas: pick ORM"), "{t}");
        assert!(t.contains("scratch"), "{t}");
        assert!(t.contains("12k"), "{t}");
    }

    #[test]
    fn thread_view_queue_strip_sits_above_the_composer() {
        use crate::model::{ItemBody, ItemKind, ItemStatus};
        use crate::thread::fixtures;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut d = fixtures::data();
        d.agents[0].2 = axel_store::StatusState::Working;
        d.items[7].status = Some(ItemStatus::Allowed);
        for (i, text) in ["check logout too", "then update the docs", "and the changelog", "and bump the version", "and tag it"].iter().enumerate() {
            let mut it = fixtures::item(&format!("q{i}"), ItemKind::UserMessage, ItemBody::Text((*text).into()), 7_000 + i as i64);
            it.agent = None;
            it.status = Some(ItemStatus::Pending);
            d.items.push(it);
        }
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut v = crate::thread::ThreadView::new("T1", d);
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let t = text(&term);
        if std::env::var_os("AXEL_PRINT").is_some() {
            println!("{t}");
        }
        let lines: Vec<&str> = t.lines().collect();
        let next = lines.iter().position(|l| l.contains("⧗ next") && l.contains("check logout too")).expect(&t);
        assert!(lines[next + 1].contains("⧗ queued") && lines[next + 1].contains("then update the docs"), "{t}");
        assert!(lines[next + 3].contains("+2 more"), "collapsed to three rows {t}");
        assert!(lines[next + 4].starts_with("› "), "the composer right below, no rule above it {t}");
        assert!(t.lines().filter(|l| l.contains("check logout too")).all(|l| l.contains('⧗')), "queued prompts are not timeline rows {t}");
        assert!(t.contains("Enter sends into its turn") && t.contains("^S interrupts"), "{t}");
        // Tab focuses the strip: every row shows, with the selection marker.
        v.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        v.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let t = text(&term);
        if std::env::var_os("AXEL_PRINT").is_some() {
            println!("{t}");
        }
        assert!(t.contains("▸ ⧗ queued   then update the docs"), "{t}");
        assert!(t.contains("and tag it") && !t.lines().any(|l| l.contains('⧗') && l.contains("more")), "{t}");
        assert!(t.contains("send now"), "{t}");
    }

    #[test]
    fn thread_view_renders_fixture_at_three_sizes() {
        use crate::thread::fixtures;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for (w, h) in [(80, 24), (120, 40), (60, 18)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut v = crate::thread::ThreadView::new("T1", fixtures::data());
            term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
            let t = text(&term);
            // header
            assert!(t.contains("Fix login") && t.contains("Atlas") && t.contains("12k") && t.contains("$0.42"), "{w}x{h}\n{t}");
            // prose
            assert!(t.contains("Fix the login bug"), "{w}x{h}\n{t}");
            // Your message: "> " flush left with the agent's text, a blank line on each side.
            let col = |needle: &str| t.lines().find_map(|l| l.find(needle).map(|i| l[..i].chars().count())).unwrap();
            assert_eq!(col("> Fix the login bug"), col("Looking at the auth"), "flush left {w}x{h}\n{t}");
            let you = t.lines().position(|l| l.contains("> Fix the login bug")).unwrap();
            assert!(t.lines().nth(you - 1).unwrap().trim().is_empty() && t.lines().nth(you + 1).unwrap().trim().is_empty(), "blank around yours {w}x{h}\n{t}");
            // leading "\n\n" of the assistant chunk is trimmed: the text starts right after the label
            assert!(t.contains("Looking at the auth"), "{w}x{h}\n{t}");
            let after = t.lines().position(|l| l.contains("Looking at the auth")).unwrap() + 1;
            assert!(t.lines().nth(after).unwrap().contains("▸ Read "), "no trailing blank lines {w}x{h}\n{t}");
            // fold line: what the calls did, then how many and how long
            assert!(t.contains("· 5 calls · 3.2s"), "{w}x{h}\n{t}");
            if w >= 100 {
                assert!(t.contains("▸ Read auth.rs, login.rs, search for “session_token”, edit login.rs, run tests · 5 calls · 3.2s"), "{w}x{h}\n{t}");
            }
            assert!(!t.contains("Read src/auth.rs"), "collapsed by default {w}x{h}\n{t}");
            // pending permission card with options, first highlighted
            assert!(t.contains("! Atlas asks permission: Run cargo test"), "{w}x{h}\n{t}");
            assert!(t.contains("▸ Allow once") && t.contains("Always allow") && t.contains("Reject"), "{w}x{h}\n{t}");
            // composer placeholder + help
            // The fixture's permission is pending: a message declines it and redirects.
            assert!(t.contains("send a message to decline"), "{w}x{h}\n{t}");
            assert!(t.contains("Enter") && t.contains("decline & redirect"), "{w}x{h}\n{t}");

            // expand the fold and a tool's output
            v.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            v.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            v.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            v.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("✓ Read auth.rs") && t.contains("Read src/auth.rs"), "its words, then what it ran {w}x{h}\n{t}");
            assert!(t.contains("line 1") && t.contains("line 12") && !t.contains("line 13"), "{w}x{h}\n{t}");
            if h >= 24 {
                // At 18 rows the output's last line sits below the fold (↓ shows it).
                assert!(t.contains("+8 lines"), "{w}x{h}\n{t}");
            }
            assert!(t.contains("fold all"), "timeline help {w}x{h}\n{t}");

            // typing shows in the composer; status shows after a send
            for c in "run it".chars() {
                v.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            v.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("redirecting…"), "{w}x{h}\n{t}");
            v.on_reply(Ok(crate::thread::SendReply::Error { message: "socket closed".into() }));
            term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
            assert!(text(&term).contains("error: socket closed"));
        }
        // Empty thread and tiny area must not panic.
        let mut term = Terminal::new(TestBackend::new(30, 5)).unwrap();
        let v = crate::thread::ThreadView::new("T1", crate::model::ThreadData::default());
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        assert!(text(&term).contains("no agent running"));
    }

    #[test]
    fn thread_view_empty_timeline_hints() {
        use super::thread_view::empty_hint;
        use crate::model::{LiveSession, ThreadData};
        let managed = LiveSession { id: "S1".into(), agent_name: "Atlas".into(), managed: true, started_at: 1, mode: Default::default(), host: None, last_reply_at: None };
        let pane = LiveSession { id: "S2".into(), agent_name: "Nova".into(), managed: false, started_at: 1, mode: Default::default(), host: None, last_reply_at: None };
        let base = ThreadData { last_agent_name: Some("Atlas".into()), ..Default::default() };
        assert_eq!(empty_hint(&ThreadData { live_sessions: vec![managed.clone()], ..base.clone() }), "Waiting for the agent…");
        assert_eq!(
            empty_hint(&base),
            "no agent running on this thread · type a message to wake Atlas · @name to summon someone else"
        );
        assert_eq!(
            empty_hint(&ThreadData::default()),
            "no agent running on this thread · @name to summon an agent"
        );
        assert!(empty_hint(&ThreadData { live_sessions: vec![pane], ..base.clone() }).starts_with("Nova is running in a pane"));
        // Rendered dim and centered, wrapped on a narrow pane, in a thread with no items.
        let mut term = Terminal::new(TestBackend::new(60, 18)).unwrap();
        let v = crate::thread::ThreadView::new("T1", base);
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let t = text(&term);
        // Wrapped over two centered lines at this width.
        assert!(t.contains("no agent running on this thread") && t.contains("Atlas · @name to summon someone else"), "{t}");
        assert!(!t.contains("Waiting for the agent"), "{t}");
        let line = t.lines().find(|l| l.contains("no agent running")).unwrap();
        assert!(line.starts_with("  ") && line.ends_with("  "), "centered: {line:?}");
    }

    #[test]
    fn thread_view_mentions_kept_and_coloured_and_text_cleaned() {
        use super::thread_view::clean_text;
        use crate::model::{ItemBody, ItemKind};
        use crate::thread::fixtures;
        // clean_text
        assert_eq!(clean_text("\n\nhello\n\n\n\n"), "hello");
        assert_eq!(clean_text("a\n\nb"), "a\n\nb", "two blank lines kept");
        assert_eq!(clean_text("a\n\n\n\nb"), "a\n\nb", "3+ blank lines collapse to one");
        assert_eq!(clean_text("  a  \n b "), "a\n b");
        assert_eq!(clean_text("   \n\n"), "");
        // user message keeps the mention as typed; the @name token gets the agent colour
        let mut d = fixtures::data();
        let mut msg = fixtures::item("u2", ItemKind::UserMessage, ItemBody::Text("@Atlas look at this, @nobody too".into()), 7_000);
        msg.agent = None;
        msg.attachments = vec![PathBuf::from("/tmp/shot.png"), PathBuf::from("/tmp/trace.log")];
        d.items.push(msg);
        let mut v = crate::thread::ThreadView::new("T1", d);
        v.attachments = vec![PathBuf::from("/tmp/next.pdf")];
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("@Atlas look at this, @nobody too"), "{t}");
        // attachments of the stored message under it, and the pending chip in the composer
        assert!(t.contains("📎 shot.png  📎 trace.log"), "{t}");
        assert!(t.contains("📎 next.pdf"), "{t}");
        // composer = text row · chips row · rule · footer (no box, no rule above)
        let lines: Vec<&str> = t.lines().collect();
        let chips_row = lines.iter().position(|l| l.contains("📎 next.pdf")).unwrap();
        let text_row = lines.iter().position(|l| l.contains("send a message to decline")).unwrap();
        assert_eq!(chips_row - text_row, 1, "chips right under the text at 80x24: {t}");
        assert!(lines[text_row].starts_with("› "), "{t}");
        assert!(!lines[text_row - 1].starts_with('─') && lines[chips_row + 1].starts_with('─'), "one rule, under the input {t}");
        assert!(lines[chips_row + 2].contains("autopilot on"), "footer under the rule {t}");
        assert!(!t.contains('╭') && !t.contains('╰') && !t.contains('│'), "no box {t}");
        // the same at the 3-row size, and a long lone chip uses the row
        v.attachments = vec![PathBuf::from("/tmp/2D1AF3B4-91C7-4C4A-B359-0123456789AB.png")];
        let mut small = Terminal::new(TestBackend::new(80, 18)).unwrap();
        small.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let ts = text(&small);
        assert!(ts.contains("📎 2D1AF3B4-91C7-4C4A-B359-0123456789AB.png"), "{ts}");
        let lines: Vec<&str> = ts.lines().collect();
        let chips_row = lines.iter().position(|l| l.contains("📎 2D1AF3B4")).unwrap();
        let text_row = lines.iter().position(|l| l.contains("send a message to decline")).unwrap();
        assert_eq!(chips_row - text_row, 1, "{ts}");
        assert!(lines[chips_row + 1].starts_with('─'), "{ts}");
        let buf = term.backend().buffer();
        let y = t.lines().position(|l| l.contains("@Atlas look")).unwrap() as u16;
        let x_at = t.lines().nth(y as usize).unwrap().find("@Atlas").unwrap() as u16;
        let purple = crate::theme::agent_color(Some(axel_store::AgentColor::Purple));
        assert_eq!(buf[(x_at, y)].fg, purple, "mention coloured with the agent colour");
        let x_nobody = t.lines().nth(y as usize).unwrap().find("@nobody").unwrap() as u16;
        assert_ne!(buf[(x_nobody, y)].fg, purple, "unknown names are plain");
        // the header above the agent's reply: square · lowercase name · square · tip, time at the right
        let y_h = t.lines().position(|l| l.contains("atlas")).unwrap() as u16;
        let line_h = t.lines().nth(y_h as usize).unwrap();
        let x_name = line_h[..line_h.find("atlas").unwrap()].chars().count() as u16;
        let (left, name, right) = (&buf[(x_name - 1, y_h)], &buf[(x_name, y_h)], &buf[(x_name + 5, y_h)]);
        assert_eq!((left.bg, name.bg, right.bg), (purple, purple, purple), "name on the agent colour between two squares");
        assert!(name.modifier.contains(ratatui::style::Modifier::BOLD));
        assert_eq!((buf[(x_name + 6, y_h)].symbol(), buf[(x_name + 6, y_h)].fg), (crate::view::thread_view::BADGE_TIP, purple));
        assert!(line_h.trim_end().ends_with(&crate::view::thread_view::clock(2_000)), "time at the right edge: {line_h:?}");
        assert_eq!(t.lines().nth(y_h as usize + 1).map(|l| l.contains("Looking at the auth")), Some(true), "reply starts below the header");
        // header dot keeps the agent colour
        let x_dot = t.lines().next().unwrap().find('◐').unwrap() as u16;
        assert_eq!(buf[(x_dot, 0)].fg, purple);
    }

    #[test]
    fn attachment_chips_selection_renders_inverted_with_hint() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let three = vec![PathBuf::from("/x/a.png"), PathBuf::from("/x/b.txt"), PathBuf::from("/x/c.rs")];
        // thread composer
        let mut v = crate::thread::ThreadView::new("T1", crate::thread::fixtures::data());
        v.attachments = three.clone();
        v.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        v.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(v.focus, crate::thread::Focus::Attachments);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("📎 a.png  📎 b.txt ✕  📎 c.rs"), "{t}");
        assert!(t.contains("←→ select · ⌫ remove · Esc back"), "{t}");
        let buf = term.backend().buffer();
        let y = t.lines().position(|l| l.contains("b.txt ✕")).unwrap() as u16;
        let row = t.lines().nth(y as usize).unwrap();
        // map text column to buffer column: each 📎 before the point occupies one extra cell
        let col = |needle: &str| -> u16 {
            let idx = row.find(needle).unwrap();
            (row[..idx].chars().count() + row[..idx].matches('📎').count()) as u16
        };
        let sel = &buf[(col("b.txt"), y)];
        assert_eq!(sel.bg, crate::theme::ACCENT);
        assert_eq!(sel.fg, ratatui::style::Color::Black);
        let other = &buf[(col("a.png"), y)];
        assert_ne!(other.bg, crate::theme::ACCENT);
        // launcher prompt field
        let mut l = launcher();
        l.attach(three);
        l.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        l.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        // Fresh terminal: the diff renderer skips wide-glyph placeholder cells, so a
        // reused TestBackend would keep stale characters there.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("📎 a.png  📎 b.txt ✕  📎 c.rs"), "{t}");
    }

    #[test]
    fn thread_view_wrap_and_fold_summary() {
        use super::thread_view::{fold_summary, wrap};
        assert_eq!(wrap("a bb ccc dddd", 6), vec!["a bb", "ccc", "dddd"]);
        assert_eq!(wrap("x\ny", 10), vec!["x", "y"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), vec![""]);
        let items = crate::thread::fixtures::items();
        let tools: Vec<&crate::model::ThreadItem> = items[2..7].iter().collect();
        assert_eq!(fold_summary(&tools, 200), "Read auth.rs, login.rs, search for “session_token”, edit login.rs, run tests");
        assert_eq!(fold_summary(&tools[..1], 200), "Read auth.rs", "one call: its own words");
    }

    #[test]
    fn launcher_spawn_and_scratch_summaries_render() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut l = Launcher::for_spawn(PathBuf::from("/work/axel"), vec![agent("atlas")], vec!["claude".into()], "T1", "Fix login");
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("Thread") && t.contains("Fix login") && t.contains("Who?"), "{t}");
        l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("will join") && t.contains("existing thread"), "{t}");
        assert!(!t.contains("runtime:"), "no label until the app sets one: {t}");
        l.runtime_label = Some("managed (claude-code-acp)".into());
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("runtime: managed (claude-code-acp)"), "{t}");

        // attachment chip under the prompt field
        let mut l = launcher();
        l.attach(vec![PathBuf::from("/tmp/design-notes.md")]);
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("📎 design-notes.md"), "{t}");
        l.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        assert!(!text(&term).contains("📎"));

        let mut l = launcher();
        for c in "Idea".chars() {
            l.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        l.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        l.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)); // scratch row
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        assert!(text(&term).contains("No project (scratch thread)"));
        l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        term.draw(|f| super::launcher_view::render(f, &l, 5_000)).unwrap();
        let t = text(&term);
        assert!(t.contains("scratch (no project)"), "{t}");
    }

    #[test]
    fn inbox_renders() {
        let dd = dashboard().data;
        let data = InboxData {
            requests: dd.requests.clone(),
            statuses: vec![StatusRow {
                session_id: "S1".into(),
                thread_id: "T1".into(),
                state: StatusState::Working,
                agent_name: "Atlas".into(),
                thread_title: "Fix login".into(),
                msg: Some("running tests".into()),
                updated_at: 4_500,
            }],
            messages: dd.messages.clone(),
            unread: 1,
        };
        for (w, h) in sizes() {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let ib = Inbox::new(data.clone());
            term.draw(|f| super::inbox_view::render(f, &ib, 5_000)).unwrap();
            let t = text(&term);
            assert!(t.contains("pending"), "{w}x{h}\n{t}");
            if h >= 18 {
                assert!(t.contains("Atlas") && t.contains("question"), "{w}x{h}\n{t}");
                assert!(t.contains("● Atlas"), "{w}x{h}\n{t}");
                assert!(t.contains("1 unread"), "{w}x{h}\n{t}");
            }
        }
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let ib = Inbox::new(InboxData::default());
        term.draw(|f| super::inbox_view::render(f, &ib, 5_000)).unwrap();
        assert!(text(&term).contains("Nothing is waiting on you."));
    }

    #[test]
    fn composer_colours_typed_mentions_of_known_agents() {
        use crate::thread::fixtures;
        use ratatui::style::{Color, Modifier};
        let mut d = fixtures::data();
        d.known_agents = vec![("nova".into(), Some(axel_store::AgentColor::Green))];
        let mut v = crate::thread::ThreadView::new("T1", d);
        v.composer.set_text("hi @atlas and @Nova, not @atl or a@nova or @ghost");
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| super::thread_view::render(f, &v, 5_000)).unwrap();
        let buf = term.backend().buffer();
        let row = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().contains("hi @atlas"))
            .unwrap();
        let line: String = (0..buf.area.width).map(|x| buf[(x, row)].symbol()).collect();
        // Column of `needle`'s first char (cells, not bytes: the prompt glyph is multi-byte).
        let col = |needle: &str| line[..line.find(needle).unwrap()].chars().count() as u16;
        let fg_at = |needle: &str| {
            let x = col(needle);
            let c = &buf[(x, row)];
            (c.fg, c.modifier.contains(Modifier::BOLD))
        };
        assert_eq!(fg_at("@atlas"), (Color::Magenta, true), "thread agent");
        assert_eq!(fg_at("@Nova"), (Color::Green, true), "any local agent, any case");
        assert_eq!(buf[(col("@Nova") + 5, row)].fg, Color::Reset, "comma stays plain");
        for plain in ["@atl ", "a@nova", "@ghost"] {
            assert_eq!(fg_at(plain).0, Color::Reset, "{plain} stays plain");
            assert_eq!(buf[(col(plain) + 2, row)].fg, Color::Reset, "{plain} stays plain");
        }
    }

    /// Plain text of rendered lines.
    fn plain(lines: &[ratatui::text::Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn a_mention_in_your_message_takes_the_agents_colour_even_before_punctuation() {
        use crate::thread::{Row, ThreadView, fixtures};
        let mut d = fixtures::data();
        d.known_agents = vec![("orion".into(), Some(AgentColor::Yellow))];
        let mut msg = fixtures::item("u9", ItemKind::UserMessage, ItemBody::Text("would love to hear more about @orion's take".into()), 9_000);
        msg.agent = None;
        d.items.push(msg);
        let v = ThreadView::new("T1", d);
        let i = v.data.items.len() - 1;
        let lines = super::thread_view::row_lines(&v, &Row::Prose(i), 80, false);
        let mention = lines.iter().flat_map(|l| l.spans.iter()).find(|s| s.content == "@orion").expect("the mention is its own span");
        assert_eq!(mention.style.fg, Some(crate::theme::agent_color(Some(AgentColor::Yellow))), "orion isn't on the thread yet, but is a local agent");
        assert!(plain(&lines).iter().any(|l| l.contains("@orion's take")), "the text is unchanged");
    }

    #[test]
    fn an_agents_message_to_another_and_its_reply_read_as_one() {
        use crate::thread::{Row, ThreadView, fixtures};
        let mut d = fixtures::data();
        d.items.clear();
        let mut relay = fixtures::item("r1", ItemKind::UserMessage, ItemBody::Text("Hi orion, what would you build first?".into()), 1_000);
        relay.relay_to = Some("Orion".into());
        d.items.push(relay);
        d.items.push(fixtures::item("a1", ItemKind::AssistantMessage, ItemBody::Text("I've asked orion.".into()), 2_000));
        d.items.push(fixtures::item("a2", ItemKind::AssistantMessage, ItemBody::Text("Later, on its own.".into()), 3_000));
        d.items[2].agent = Some(("Nova".into(), None));
        let v = ThreadView::new("T1", d);
        let relay = plain(&super::thread_view::row_lines(&v, &Row::Prose(0), 80, false));
        assert!(relay[0].contains(" atlas "), "the sender's badge first: {relay:?}");
        assert_eq!(relay[1], "→ orion");
        assert_eq!(relay[2], "│ Hi orion, what would you build first?");
        let reply = plain(&super::thread_view::row_lines(&v, &Row::Prose(1), 80, false));
        assert_eq!(reply, vec!["I've asked orion.".to_string()], "no second atlas badge");
        let other = plain(&super::thread_view::row_lines(&v, &Row::Prose(2), 80, false));
        assert!(other[0].is_empty() && other[1].contains(" nova "), "a different agent gets its own badge, a blank line above: {other:?}");
    }

    #[test]
    fn the_composer_breaks_rows_at_line_breaks_and_the_width() {
        use super::thread_view::input_rows;
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        // "ab\ncd": two rows, the break itself on neither.
        assert_eq!(input_rows(&c("ab\ncd"), 10, 5), (vec![(0, 2), (3, 5)], (1, 2)));
        assert_eq!(input_rows(&c("ab\ncd"), 10, 2), (vec![(0, 2), (3, 5)], (0, 2)), "before the break: end of the first row");
        // A trailing break: the cursor sits on the new, empty row.
        assert_eq!(input_rows(&c("ab\n"), 10, 3), (vec![(0, 2), (3, 3)], (1, 0)));
        // Wrapping at the width: "abcdef" at 4 is "abcd" / "ef".
        assert_eq!(input_rows(&c("abcdef"), 4, 6), (vec![(0, 4), (4, 6)], (1, 2)));
        assert_eq!(input_rows(&c("abcdef"), 4, 4), (vec![(0, 4), (4, 6)], (1, 0)), "at the wrap: the next row");
        // A full last row: the cursor after it gets a row of its own.
        assert_eq!(input_rows(&c("abcd"), 4, 4), (vec![(0, 4), (4, 4)], (1, 0)));
        assert_eq!(input_rows(&[], 4, 0), (vec![(0, 0)], (0, 0)));
    }
}
