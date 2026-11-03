//! Launcher rendering: a centred, rounded block with breadcrumbs for answered
//! steps, the current step's widget, an error line and a help footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::{chips_spans, field_line, help_line};
use crate::launcher::{Field, Launcher, Step, TitleFocus, WhereMode, WhereRow, WhoMode, Workspace};
use crate::theme;
use crate::util::{display_path, ellipsize, relative_time};

/// Horizontal padding inside the pane.
const PAD: u16 = 2;

pub fn render(frame: &mut Frame, l: &Launcher, now: i64) {
    let full = frame.area();
    frame.render_widget(Clear, full);
    if full.height < 3 || full.width < 10 {
        return;
    }

    // The prompt alone: the question, the input, nothing else.
    if l.step == Step::Title && l.spawn.is_none() {
        render_prompt(frame, l, full);
        return;
    }

    // Edge to edge: the Rex floating pane is the frame. One title line, one help line.
    let [top, body_area, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(full);
    let pad = |r: Rect| Rect { x: r.x + PAD, y: r.y, width: r.width.saturating_sub(PAD * 2), height: r.height };
    let (top, inner, bottom) = (pad(top), pad(body_area), pad(bottom));

    let step_no = format!("{}/{}", l.step.index() + 1, Step::COUNT);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("axel", theme::accent_bold()), Span::raw("  "), Span::styled(l.step_title(), theme::dim())])),
        top,
    );
    // The prompt is one step; the counter only means something in the full form.
    if l.step != Step::Title || l.spawn.is_some() {
        frame.render_widget(Paragraph::new(Line::from(Span::styled(step_no, theme::dim())).right_aligned()), top);
    }
    frame.render_widget(Paragraph::new(help_line(&l.help())), bottom);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Breadcrumbs for answered steps (compact when short on rows).
    let crumbs = l.breadcrumbs();
    let crumb_rows = if inner.height >= 14 { crumbs.len() as u16 } else { 0 };
    let has_gap = inner.height >= 12 && crumb_rows > 0;
    let [crumb_area, gap, body] = Layout::vertical([
        Constraint::Length(crumb_rows),
        Constraint::Length(if has_gap { 1 } else { 0 }),
        Constraint::Min(1),
    ])
    .areas(inner);
    let _ = gap;

    if crumb_rows > 0 {
        let lines: Vec<Line> = crumbs
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!("{k:<10}"), theme::dim()),
                    Span::styled(ellipsize(v, inner.width.saturating_sub(10) as usize), Style::default()),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), crumb_area);
    }

    match l.step {
        Step::Title => render_prompt(frame, l, body),
        Step::Where => render_where(frame, l, body),
        Step::Who => match l.who_mode {
            WhoMode::List => render_who(frame, l, body, now),
            WhoMode::NewAgent => render_new_agent(frame, l, body),
        },
        Step::Workspace => render_workspace(frame, l, body),
        Step::Summary => render_summary(frame, l, body),
    }
}

/// Question header + optional error line; returns the area left for content.
fn header(frame: &mut Frame, area: Rect, question: &str, hint: Option<&str>, error: Option<&str>) -> Rect {
    let [q, content] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let mut spans = vec![Span::styled(question.to_string(), theme::bold())];
    if let Some(e) = error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(e.to_string(), theme::danger()));
    } else if let Some(h) = hint {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(h.to_string(), theme::dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), q);
    content
}

fn input_row(frame: &mut Frame, area: Rect, input: &crate::input::TextInput, placeholder: &str, focused: bool) {
    if area.height == 0 {
        return;
    }
    let [row] = Layout::vertical([Constraint::Length(1)]).areas(area);
    let prompt = Span::styled("› ", if focused { theme::accent() } else { theme::dim() });
    let width = row.width.saturating_sub(2) as usize;
    let (shown, col) = input.view(width);
    let mut line = field_line(&shown, placeholder, focused);
    line.spans.insert(0, prompt);
    frame.render_widget(Paragraph::new(line), row);
    if focused {
        frame.set_cursor_position(Position::new(row.x + 2 + col, row.y));
    }
}

/// The prompt's input: like [`input_row`], with `@name`s of known agents in
/// their colours, as in the thread composer.
fn prompt_input(frame: &mut Frame, area: Rect, l: &Launcher) {
    if area.height == 0 {
        return;
    }
    let focused = l.title_focus == TitleFocus::Text;
    let glyph = Span::styled("› ", if focused { theme::accent() } else { theme::dim() });
    if l.title.is_empty() {
        let line = Line::from(vec![glyph, Span::styled(Step::Title.question(), theme::dim())]);
        frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
        if focused {
            frame.set_cursor_position(Position::new(area.x + 2, area.y));
        }
        return;
    }
    // Rows broken at line breaks and the width, as in the thread composer,
    // scrolled so the cursor's row shows.
    let chars: Vec<char> = l.title.text().chars().collect();
    let styles = super::thread_view::mention_styles(&chars, &l.mention_names());
    let body_w = (area.width as usize).saturating_sub(2).max(1);
    let (rows, (cur_row, cur_col)) = super::thread_view::input_rows(&chars, body_w, l.title.cursor());
    let height = area.height.max(1) as usize;
    // While a growing pane catches up with a new line, keep the top in place
    // (the new line appears as the pane grows); scroll only past the most
    // rows it grows to, or when it can't grow.
    let first = if l.pane_grows && rows.len() <= super::thread_view::MAX_INPUT_ROWS { 0 } else { cur_row.saturating_sub(height - 1) };
    for (r, &(start, end)) in rows.iter().enumerate().skip(first).take(height) {
        let lead = if r == 0 { glyph.clone() } else { Span::raw("  ") };
        let mut spans = vec![lead];
        spans.extend(super::thread_view::styled_run(&chars[start..end], &styles[start..end]));
        frame.render_widget(Paragraph::new(Line::from(spans)), Rect { y: area.y + (r - first) as u16, height: 1, ..area });
    }
    // The cursor's row may be the one the pane is still growing to show.
    if focused && cur_row - first < height {
        frame.set_cursor_position(Position::new(area.x + 2 + cur_col as u16, area.y + (cur_row - first) as u16));
    }
}

/// Rows the prompt pane needs, `width` wide: the gap above, the text (up to
/// [`super::thread_view::MAX_INPUT_ROWS`]) and the attachments row.
pub fn prompt_pane_rows(l: &Launcher, width: u16) -> u16 {
    let field = width.saturating_sub(PAD * 2);
    let text = prompt_rows(l, field).min(super::thread_view::MAX_INPUT_ROWS as u16);
    1 + text + u16::from(!l.attachments.is_empty())
}

/// Rows the prompt's text takes in a field `width` wide.
fn prompt_rows(l: &Launcher, width: u16) -> u16 {
    let chars: Vec<char> = l.title.text().chars().collect();
    let body_w = (width as usize).saturating_sub(2).max(1);
    super::thread_view::input_rows(&chars, body_w, l.title.cursor()).0.len().max(1) as u16
}

/// The first screen: just the input (placeholder `What are we building?`),
/// with who and where dim at the right of it, and any attachments under it.
fn render_prompt(frame: &mut Frame, l: &Launcher, full: Rect) {
    let area = Rect { x: full.x + PAD, y: full.y, width: full.width.saturating_sub(PAD * 2), height: full.height };
    // As many rows as the prompt needs, within the pane (chips keep a row).
    // A pane that grows to fit keeps its blank row (it is asked for), so
    // nothing moves while it grows; one that can't, gives the row up first.
    let chip_row = u16::from(!l.attachments.is_empty());
    let needed = prompt_rows(l, area.width);
    let gap = u16::from(area.height >= 3 && (l.pane_grows || area.height > needed + chip_row));
    let rows = needed.min(area.height.saturating_sub(gap + chip_row).max(1));
    let [_, input, chips] = Layout::vertical([Constraint::Length(gap), Constraint::Length(rows), Constraint::Min(0)]).areas(area);
    let right = match (&l.error, l.quick_target()) {
        (Some(e), _) => Line::from(Span::styled(e.clone(), theme::danger())),
        (None, Some((agent, place))) => Line::from(vec![
            Span::styled(agent.name.clone(), Style::default().fg(theme::agent_color(agent.color))),
            Span::styled(format!(" · {place}"), theme::dim()),
        ]),
        (None, None) => Line::from(Span::styled("no agent yet", theme::dim())),
    };
    // Who/where takes the right of the row while the input leaves it room.
    let right_w = right.width() as u16;
    let typed = l.title.text().chars().count() as u16;
    let room = input.width.saturating_sub(right_w + 2);
    let show_right = l.error.is_some() || (rows == 1 && !l.title.text().contains('\n') && room >= 24 && typed + 3 < room);
    let field = if show_right { Rect { width: room, ..input } } else { input };
    prompt_input(frame, field, l);
    if show_right {
        frame.render_widget(Paragraph::new(right.right_aligned()), Rect { height: 1, ..input });
    }
    if chips.height > 0 && !l.attachments.is_empty() {
        let selected = (l.title_focus == TitleFocus::Attachments).then_some(l.attach_sel);
        let mut spans = vec![Span::raw("  ")];
        spans.extend(chips_spans(&l.attachments, chips.width.saturating_sub(2) as usize, selected));
        frame.render_widget(Paragraph::new(Line::from(spans)), chips);
    }
}

fn render_where(frame: &mut Frame, l: &Launcher, area: Rect) {
    match l.where_mode {
        WhereMode::Path => {
            let content = header(frame, area, "Where?", Some("type a directory path"), l.error.as_deref());
            let [_, input, _] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(content);
            input_row(frame, input, &l.path_input, "~/code/project", true);
        }
        WhereMode::List => {
            let n = l.selected_paths.len();
            let hint = match n {
                0 => "Space toggles, Enter confirms".to_string(),
                1 => "1 project selected".to_string(),
                n => format!("{n} projects selected"),
            };
            let content = header(frame, area, "Where?", Some(&hint), l.error.as_deref());
            let [input, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(content);
            input_row(frame, input, &l.filter, "type to filter", true);

            let rows = l.where_list();
            let width = list_area.width as usize;
            let name_w = (width / 3).clamp(8, 24);
            let items: Vec<ListItem> = rows
                .iter()
                .enumerate()
                .map(|(row, kind)| match kind {
                    WhereRow::Project(idx) => {
                        let p = &l.projects[*idx];
                        let on = l.is_path_selected(&p.path);
                        let is_cwd = p.path == l.cwd;
                        let name = ellipsize(&p.name, name_w);
                        let path = ellipsize(&display_path(&p.path), width.saturating_sub(name_w + 10));
                        let mut spans = vec![
                            Span::styled(if on { "◼ " } else { "◻ " }, if on { theme::accent() } else { theme::dim() }),
                            Span::styled(
                                format!("{name:<name_w$}"),
                                if row == l.where_sel { Style::default() } else { theme::bold() },
                            ),
                            Span::raw("  "),
                            Span::styled(path, theme::dim()),
                        ];
                        if is_cwd {
                            spans.push(Span::styled("  (here)", theme::dim()));
                        }
                        ListItem::new(Line::from(spans))
                    }
                    WhereRow::OtherPath => ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("Other path…", theme::accent()),
                    ])),
                    WhereRow::Scratch => ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("No project (scratch thread)", theme::accent()),
                    ])),
                })
                .collect();

            let list = List::new(items).highlight_style(theme::selected()).highlight_symbol("▸ ");
            let mut state = ListState::default().with_selected(Some(l.where_sel.min(rows.len() - 1)));
            frame.render_stateful_widget(list, list_area, &mut state);
        }
    }
}

fn render_who(frame: &mut Frame, l: &Launcher, area: Rect, now: i64) {
    let content = header(frame, area, "Who?", Some("pick an agent"), l.error.as_deref());
    let width = content.width as usize;
    let name_w = (width / 4).clamp(6, 16);
    let mut items: Vec<ListItem> = Vec::with_capacity(l.agents.len() + 1);
    items.push(ListItem::new(Line::from(Span::styled("+ New agent", theme::accent()))));
    for a in &l.agents {
        let dot = Span::styled("● ", Style::default().fg(theme::agent_color(a.color)));
        let name = Span::styled(format!("{:<name_w$}", ellipsize(&a.name, name_w)), theme::bold());
        let mut meta = a.harness.to_string();
        if let Some(m) = &a.model {
            meta.push_str(" · ");
            meta.push_str(m);
        }
        let seen = match a.last_seen_at {
            Some(ts) => relative_time(ts, now),
            None => "never".to_string(),
        };
        let mut spans = vec![dot, name, Span::raw(" "), Span::styled(ellipsize(&meta, width.saturating_sub(name_w + 24)), theme::dim())];
        let mut right = seen;
        if a.unread > 0 {
            right = format!("{right}  ✉ {}", a.unread);
        }
        // Right-align the trailing info by padding.
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum::<usize>() + 2;
        let pad = width.saturating_sub(used + right.chars().count());
        spans.push(Span::raw(" ".repeat(pad)));
        if a.unread > 0 {
            let seen_only = right.split("  ✉").next().unwrap_or("").to_string();
            spans.push(Span::styled(seen_only, theme::dim()));
            spans.push(Span::styled(format!("  ✉ {}", a.unread), theme::accent_bold()));
        } else {
            spans.push(Span::styled(right, theme::dim()));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    let list = List::new(items).highlight_style(theme::selected()).highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(l.who_sel));
    frame.render_stateful_widget(list, content, &mut state);
}

fn render_new_agent(frame: &mut Frame, l: &Launcher, area: Rect) {
    let content = header(frame, area, "New agent", Some("Ed25519 key + owner attestation"), l.form.error.as_deref());
    let rows: Vec<Constraint> = Field::ALL.iter().map(|_| Constraint::Length(1)).chain([Constraint::Min(0)]).collect();
    let areas = Layout::vertical(rows).split(content);
    let label_w = 9usize;
    for (i, field) in Field::ALL.iter().enumerate() {
        let row = areas[i];
        if row.height == 0 {
            continue;
        }
        let focused = l.form.field == *field;
        let marker = Span::styled(if focused { "› " } else { "  " }, theme::accent());
        let label = Span::styled(format!("{:<label_w$}", field.label()), if focused { theme::accent() } else { theme::dim() });
        let value_x = row.x + 2 + label_w as u16;
        let value_w = row.width.saturating_sub(2 + label_w as u16) as usize;
        let value_line: Line = match field {
            Field::Name => {
                let (shown, col) = l.form.name.view(value_w);
                if focused {
                    frame.set_cursor_position(Position::new(value_x + col, row.y));
                }
                field_line(&shown, "required, used as @name", focused)
            }
            Field::Model => {
                let (shown, col) = l.form.model.view(value_w);
                if focused {
                    frame.set_cursor_position(Position::new(value_x + col, row.y));
                }
                field_line(&shown, "optional, harness default", focused)
            }
            Field::Skills => {
                let (shown, col) = l.form.skills.view(value_w);
                if focused {
                    frame.set_cursor_position(Position::new(value_x + col, row.y));
                }
                field_line(&shown, "* for all, or comma-separated", focused)
            }
            Field::Harness => cycle_line(&l.harnesses, l.form.harness_idx % l.harnesses.len(), focused, None),
            Field::Mode => {
                let names: Vec<String> = crate::model::RuntimeMode::ALL.iter().map(|m| m.to_string()).collect();
                cycle_line(&names, l.form.mode_idx % names.len(), focused, None)
            }
            Field::Color => {
                let names: Vec<String> = crate::model::AgentColor::ALL.iter().map(|c| c.to_string()).collect();
                let color = theme::agent_color(Some(l.form.color()));
                cycle_line(&names, l.form.color_idx % names.len(), focused, Some(color))
            }
        };
        let mut spans = vec![marker, label];
        spans.extend(value_line.spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    }
    if l.form.busy {
        let last = areas[Field::ALL.len()];
        if last.height > 0 {
            frame.render_widget(Paragraph::new(Line::from(Span::styled("creating…", theme::dim()))), last);
        }
    }
}

fn cycle_line(options: &[String], idx: usize, focused: bool, dot: Option<ratatui::style::Color>) -> Line<'static> {
    let mut spans = Vec::new();
    if focused {
        spans.push(Span::styled("‹ ", theme::accent()));
    }
    if let Some(c) = dot {
        spans.push(Span::styled("● ", Style::default().fg(c)));
    }
    spans.push(Span::styled(options[idx].clone(), if focused { theme::bold() } else { Style::default() }));
    if focused {
        spans.push(Span::styled(" ›", theme::accent()));
        let others: Vec<&str> = options.iter().enumerate().filter(|(i, _)| *i != idx).map(|(_, s)| s.as_str()).collect();
        if !others.is_empty() {
            spans.push(Span::styled(format!("   {}", others.join("  ")), theme::dim()));
        }
    }
    Line::from(spans)
}

fn render_workspace(frame: &mut Frame, l: &Launcher, area: Rect) {
    let content = header(frame, area, "Workspace", Some("↑↓ or Space to toggle"), l.error.as_deref());
    let [_, in_place, worktree, branch, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(content);

    let radio = |on: bool, label: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(if on { "◉ " } else { "○ " }, if on { theme::accent() } else { theme::dim() }),
            Span::styled(label.to_string(), if on { theme::bold() } else { Style::default() }),
        ])
    };
    frame.render_widget(Paragraph::new(radio(l.workspace == Workspace::InPlace, "In place")), in_place);
    frame.render_widget(
        Paragraph::new(radio(l.workspace == Workspace::Worktree, "New worktree")),
        worktree,
    );
    if branch.height > 0 {
        let focused = l.workspace == Workspace::Worktree;
        let [_, field] = Layout::horizontal([Constraint::Length(4), Constraint::Min(1)]).areas(branch);
        let label = Span::styled("branch ", if focused { theme::accent() } else { theme::dim() });
        let width = field.width.saturating_sub(7) as usize;
        let (shown, col) = l.branch.view(width);
        let mut line = field_line(&shown, "axel/<slug>", focused);
        line.spans.insert(0, label);
        frame.render_widget(Paragraph::new(line), field);
        if focused {
            frame.set_cursor_position(Position::new(field.x + 7 + col, field.y));
        }
    }
}

fn render_summary(frame: &mut Frame, l: &Launcher, area: Rect) {
    let content = header(frame, area, "Ready", None, l.error.as_deref());
    let agent = l.chosen_agent_row();
    let who = agent.map(|a| a.name.clone()).unwrap_or_else(|| "?".into());
    let harness = agent.map(|a| a.harness.to_string()).unwrap_or_default();
    let verb = if l.spawn.is_some() { " will join " } else { " will work on " };
    let where_line = if l.spawn.is_some() {
        Line::from(Span::styled("as another session on the existing thread.", theme::dim()))
    } else if l.selected_paths.is_empty() {
        Line::from(vec![Span::styled("in a ", theme::dim()), Span::raw("scratch (no project)"), Span::styled(" workspace.", theme::dim())])
    } else {
        Line::from(vec![
            Span::styled("in ", theme::dim()),
            Span::raw(l.selected_paths.iter().map(|p| display_path(p)).collect::<Vec<_>>().join(", ")),
            Span::styled(format!(", {}.", l.workspace_summary()), theme::dim()),
        ])
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(who, Style::default().fg(theme::agent_color(agent.and_then(|a| a.color))).bold()),
            Span::styled(format!(" ({harness})"), theme::dim()),
            Span::raw(verb),
            Span::styled(format!("“{}”", l.title.text().trim()), theme::bold()),
        ]),
        where_line,
        Line::from(match &l.runtime_label {
            Some(r) => vec![Span::styled("runtime: ", theme::dim()), Span::raw(r.clone())],
            None => vec![],
        }),
        Line::from(""),
        Line::from(vec![Span::styled("Enter", theme::accent_bold()), Span::styled(" to launch", theme::dim())]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), content);
}
