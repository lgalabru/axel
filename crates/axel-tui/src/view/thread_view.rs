//! Thread view rendering: header · timeline · composer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{chips_spans, help_line};
use crate::model::{ItemBody, ItemKind, ItemStatus, ThreadItem, state_glyph};
use crate::theme;
use crate::thread::{Focus, Row, ThreadView};
use crate::util::{ellipsize, format_tokens};

const SELECTED_BG: Color = Color::Indexed(236);
const OUTPUT_HEAD: usize = 12;
/// Queued prompts shown above the composer before collapsing into "+N more".
const QUEUE_ROWS: usize = 3;

/// Height of the queued-messages strip.
pub fn queue_height(v: &ThreadView) -> u16 {
    let n = v.queued().len();
    if n == 0 {
        return 0;
    }
    // While the strip has focus up to 8 rows show; otherwise collapse to
    // QUEUE_ROWS plus a "+N more" line (never "+1 more": that row fits the message).
    let cap = if v.focus == Focus::Queue { 8 } else { QUEUE_ROWS };
    n.min(cap + 1) as u16
}

pub fn render(frame: &mut Frame, v: &ThreadView, now: i64) {
    let area = frame.area();
    let composer_h = composer_height(v, area);
    let header_h: u16 = u16::from(area.height >= 6);
    let queue_h = if area.height >= 12 { queue_height(v) } else { 0 };
    let [header, timeline, queue, composer] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(1),
        Constraint::Length(queue_h),
        Constraint::Length(composer_h),
    ])
    .areas(area);
    if header_h > 0 {
        render_header(frame, v, header);
    }
    render_timeline(frame, v, timeline, now);
    if queue_h > 0 {
        render_queue(frame, v, queue);
    }
    render_composer(frame, v, composer);
}

fn render_header(frame: &mut Frame, v: &ThreadView, area: Rect) {
    let d = &v.data;
    let mut spans = vec![Span::raw(" "), Span::styled(ellipsize(&d.title, (area.width / 2) as usize), theme::accent_bold())];
    if !d.agents.is_empty() {
        spans.push(Span::styled("  ·  ", theme::dim()));
        for (i, (name, color, state)) in d.agents.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(format!("{} ", state_glyph(*state)), Style::default().fg(theme::agent_color(*color))));
            spans.push(Span::styled(name.clone(), theme::bold()));
        }
    }
    if !d.environments.is_empty() {
        spans.push(Span::styled(format!("  ·  {}", d.environments.join(", ")), theme::dim()));
    }
    spans.push(Span::styled(format!("  ·  {}", format_tokens(d.tokens)), theme::dim()));
    spans.push(Span::styled(format!("  ·  ${:.2} est", d.cost_usd), theme::dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Word-wrap `text` to `width` columns (by chars), preserving explicit newlines.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut len = 0usize;
        for word in para.split(' ') {
            let wl = word.chars().count();
            if len > 0 && len + 1 + wl > width {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            if wl > width {
                // Hard-break very long words.
                for ch in word.chars() {
                    if len >= width {
                        out.push(std::mem::take(&mut line));
                        len = 0;
                    }
                    line.push(ch);
                    len += 1;
                }
                continue;
            }
            if len > 0 {
                line.push(' ');
                len += 1;
            }
            line.push_str(word);
            len += wl;
        }
        out.push(line);
    }
    out
}

/// The badge's tip: a powerline arrow, so square · initial · tip read as one
/// arrow.  Terminals built on Ghostty draw it themselves; swap for `▶` if a
/// font shows a box.
pub const BADGE_TIP: &str = "\u{e0b0}";
/// The marker in front of your own messages (orange, bold).
pub const YOU_MARK: &str = "> ";
/// Orange, as in the agent palette.
pub const YOU_COLOR: Color = Color::Indexed(208);

/// `> ` before your message; dimmed when it never went out.
pub fn you_prefix(base: Style, sent: bool) -> Vec<Span<'static>> {
    let style = if sent { base.fg(YOU_COLOR).bold() } else { base.fg(theme::DIM).bold() };
    vec![Span::styled(YOU_MARK, style)]
}

/// `13:46`, local time.
pub fn clock(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Pad `line` to put `right` (dim) against the right edge, when it fits.
fn with_right(mut line: Line<'static>, right: &str, width: usize, base: Style) -> Line<'static> {
    let used = line.width();
    let w = right.chars().count();
    if !right.is_empty() && used + 1 + w <= width {
        line.spans.push(Span::styled(" ".repeat(width - used - w), base));
        line.spans.push(Span::styled(right.to_string(), base.fg(theme::DIM)));
    }
    line
}

/// The header above an agent's (or another participant's) message: a badge
/// in their colour — square · lowercase name · square · arrow tip — and the
/// time against the right edge.  `None` for your own messages.
/// The left edge of a message one agent sent another.
const RELAY_GUTTER: &str = "│ ";

/// Item `i` is said by the agent whose message comes right before it (turn
/// ends aside), so one badge covers both: an agent's note to another agent
/// and its reply to you read as one message.
fn continues_speaker(items: &[ThreadItem], i: usize) -> bool {
    let speaker = |it: &ThreadItem| match (it.kind, &it.body) {
        (ItemKind::AssistantMessage, ItemBody::Text(_)) => it.agent.as_ref().map(|(n, _)| n.to_lowercase()),
        (ItemKind::UserMessage, ItemBody::Text(_)) if it.relay_to.is_some() => it.agent.as_ref().map(|(n, _)| n.to_lowercase()),
        _ => None,
    };
    let Some(me) = speaker(&items[i]) else { return false };
    items[..i].iter().rev().find(|it| it.kind != ItemKind::TurnEnd).and_then(speaker).is_some_and(|prev| prev == me)
}

/// The badge over item `i`, a blank line above it so each new message
/// stands apart (except right after your own, which ends in one), or
/// nothing when the same speaker goes on.
fn header_lines(items: &[ThreadItem], i: usize, base: Style, width: usize) -> Vec<Line<'static>> {
    if continues_speaker(items, i) {
        return Vec::new();
    }
    let Some(h) = speaker_header(&items[i], base, width) else { return Vec::new() };
    let after_yours = items[..i]
        .iter()
        .rev()
        .find(|it| it.kind != ItemKind::TurnEnd)
        .is_none_or(|it| it.kind == ItemKind::UserMessage && it.agent.is_none() && it.author.is_none() && it.relay_to.is_none());
    if after_yours { vec![h] } else { vec![Line::from(""), h] }
}

pub fn speaker_header(it: &ThreadItem, base: Style, width: usize) -> Option<Line<'static>> {
    let (name, color) = match (&it.agent, &it.author) {
        (Some((name, color)), _) => (name.clone(), theme::agent_color(*color)),
        (None, Some(author)) => (author.clone(), Color::Indexed(244)),
        (None, None) => return None,
    };
    let fill = base.bg(color);
    let line = Line::from(vec![
        Span::styled(" ", fill),
        Span::styled(name.to_lowercase(), fill.fg(Color::Black).bold()),
        Span::styled(" ", fill),
        Span::styled(BADGE_TIP, base.fg(color)),
    ]);
    Some(with_right(line, &clock(it.at_ms), width, base))
}

/// Trim leading/trailing whitespace and blank lines, and collapse runs of three
/// or more blank lines into one. Streamed chunks often start with "\n\n".
pub fn clean_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut blanks = 0usize;
    for l in lines {
        if l.trim().is_empty() {
            blanks += 1;
            continue;
        }
        if !out.is_empty() {
            // 1 or 2 blank lines are kept as they are; 3+ collapse to one.
            let keep = if blanks >= 3 { 1 } else { blanks };
            out.extend(std::iter::repeat_n("", keep));
        }
        blanks = 0;
        out.push(l);
    }
    let joined = out.join("\n");
    joined.trim().to_string()
}

/// Split a wrapped line into spans, colouring `@name` tokens that match a known agent.
/// `@name` in `line`, where `name` (the name characters right after the `@`,
/// so `@nova's` and `@nova,` count) is one of `agents`, in that agent's colour.
fn mention_spans(line: &str, agents: &[(String, Option<axel_store::AgentColor>)], style: Style) -> Vec<Span<'static>> {
    let is_name = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut spans = Vec::new();
    let mut plain = String::new();
    for word in line.split_inclusive(' ') {
        let matched = word.strip_prefix('@').and_then(|rest| {
            let n: String = rest.chars().take_while(|c| is_name(*c)).collect();
            agents.iter().find(|(name, _)| !n.is_empty() && name.eq_ignore_ascii_case(&n)).map(|(_, color)| (n.len(), *color))
        });
        match matched {
            Some((n_len, color)) => {
                if !plain.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut plain), style));
                }
                let (mention, tail) = word.split_at(1 + n_len);
                spans.push(Span::styled(mention.to_string(), style.fg(theme::agent_color(color)).bold()));
                plain.push_str(tail);
            }
            None => plain.push_str(word),
        }
    }
    if !plain.is_empty() {
        spans.push(Span::styled(plain, style));
    }
    spans
}

/// Like [`prose_lines`] but highlights `@name` mentions of known agents.
fn user_lines(prefix: Vec<Span<'static>>, text: &str, width: usize, style: Style, agents: &[(String, Option<axel_store::AgentColor>)]) -> Vec<Line<'static>> {
    let indent: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    let body_w = width.saturating_sub(indent).max(8);
    let mut lines = Vec::new();
    for (i, l) in wrap(text, body_w).into_iter().enumerate() {
        let mut spans = if i == 0 { prefix.clone() } else { vec![Span::styled(" ".repeat(indent), style)] };
        spans.extend(mention_spans(&l, agents, style));
        lines.push(Line::from(spans));
    }
    lines
}

/// Lines for `text` with a styled `prefix` on the first line and continuation indent.
fn prose_lines(prefix: Vec<Span<'static>>, text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let indent: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    let body_w = width.saturating_sub(indent).max(8);
    let wrapped = wrap(text, body_w);
    let mut lines = Vec::with_capacity(wrapped.len());
    for (i, l) in wrapped.into_iter().enumerate() {
        if i == 0 {
            let mut spans = prefix.clone();
            spans.push(Span::styled(l, style));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![Span::styled(" ".repeat(indent), style), Span::styled(l, style)]));
        }
    }
    lines
}

fn duration_label(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// A title on one row: its first line, `…` when there was more.
pub fn one_line(s: &str) -> String {
    let trimmed = s.trim();
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or("").trim_end();
    if lines.next().is_some() { format!("{first} …") } else { first.to_string() }
}

/// What a run of tool calls did, in at most `room` characters: one call's
/// own words, or every call's actions merged by verb ([`crate::tools::summarize`]).
pub fn fold_summary(items: &[&ThreadItem], room: usize) -> String {
    let calls: Vec<(&String, &Vec<crate::tools::Action>)> = items
        .iter()
        .filter_map(|it| match &it.body {
            ItemBody::Tool { summary, actions, .. } => Some((summary, actions)),
            _ => None,
        })
        .collect();
    if let [(summary, _)] = calls[..] {
        return summary.clone();
    }
    let actions: Vec<crate::tools::Action> = calls.iter().flat_map(|(_, a)| a.iter().cloned()).collect();
    crate::tools::summarize(&actions, room)
}

/// Lines for one visible row. `selected` paints the background when the timeline has focus.
pub(crate) fn row_lines(v: &ThreadView, row: &Row, width: usize, selected: bool) -> Vec<Line<'static>> {
    match row {
        Row::Prose(_) => row_body(v, row, width, selected),
        // Tool runs, thoughts and approvals sit flush left too.
        _ => row_body(v, row, width, selected),
    }
}

fn row_body(v: &ThreadView, row: &Row, width: usize, selected: bool) -> Vec<Line<'static>> {
    let items = &v.data.items;
    let base = if selected { Style::default().bg(SELECTED_BG) } else { Style::default() };
    let dim = if selected { theme::dim().bg(SELECTED_BG) } else { theme::dim() };
    match row {
        Row::Prose(i) => {
            let it = &items[*i];
            match (&it.kind, &it.body) {
                (ItemKind::Error, ItemBody::Text(t)) => prose_lines(vec![Span::styled("! ", base.fg(theme::DANGER).bold())], t, width, base.fg(theme::DANGER)),
                (ItemKind::Answer, ItemBody::Answered(label)) => vec![Line::from(Span::styled(format!("↩ answered: {label}"), dim))],
                (ItemKind::UserMessage, ItemBody::Text(t)) if it.status == Some(ItemStatus::Cancelled) => {
                    // Queued but never sent (the session ended first).
                    let mut lines = prose_lines(you_prefix(dim, false), t.trim(), width, dim);
                    if let Some(first) = lines.first_mut() {
                        *first = with_right(std::mem::take(first), &clock(it.at_ms), width, dim);
                    }
                    lines.push(Line::from(Span::styled("  × not sent · session ended while queued", dim)));
                    lines
                }
                (ItemKind::UserMessage, ItemBody::Text(t)) if it.relay_to.is_some() => {
                    // One agent to another: under the sender's badge (unless its
                    // previous message already shows it), marked and set off.
                    let mut lines = header_lines(items, *i, base, width);
                    let to = it.relay_to.clone().unwrap_or_default();
                    lines.push(Line::from(vec![Span::styled("→ ", dim), Span::styled(to.to_lowercase(), dim.bold())]));
                    let gutter = Span::styled(RELAY_GUTTER, dim);
                    for l in user_lines(Vec::new(), t.trim(), width.saturating_sub(RELAY_GUTTER.len()), dim, &mention_names(&v.data)) {
                        let mut spans = vec![gutter.clone()];
                        spans.extend(l.spans);
                        lines.push(Line::from(spans));
                    }
                    lines
                }
                (ItemKind::UserMessage, ItemBody::Text(t)) => {
                    let prefix = if speaker_header(it, base, width).is_some() { Vec::new() } else { you_prefix(base, true) };
                    let mut lines = user_lines(prefix, t.trim(), width, base, &mention_names(&v.data));
                    match speaker_header(it, base, width) {
                        // Another participant: header line, text below.
                        Some(_) => {
                            let head = header_lines(items, *i, base, width);
                            lines.splice(0..0, head);
                        }
                        // You: no badge, the time on your first line.
                        None => {
                            if let Some(first) = lines.first_mut() {
                                *first = with_right(std::mem::take(first), &clock(it.at_ms), width, base);
                            }
                        }
                    }
                    if !it.attachments.is_empty() {
                        let indent = if it.agent.is_none() && it.author.is_none() { YOU_MARK.len() } else { 0 };
                        lines.push(Line::from(vec![
                            Span::styled(" ".repeat(indent), dim),
                            Span::styled(crate::attach::chips_line(&it.attachments, width.saturating_sub(indent)), dim),
                        ]));
                    }
                    if it.agent.is_none() && it.author.is_none() {
                        // Your messages stand apart: a blank line above and below.
                        lines.insert(0, Line::from(""));
                        lines.push(Line::from(""));
                    }
                    lines
                }
                (_, ItemBody::Text(t)) => {
                    // Agent: header line (badge + time), the response starts below
                    // it. Right after its own message, the same agent goes on under it.
                    let mut lines = header_lines(items, *i, base, width);
                    // Agents write Markdown: show it styled, its markers dropped.
                    lines.extend(super::markdown::md_lines(&clean_text(t), width, base));
                    lines
                }
                _ => vec![Line::from(Span::styled("·", dim))],
            }
        }
        Row::Thought(i) => {
            let it = &items[*i];
            let mut lines = vec![Line::from(vec![
                Span::styled("· ", dim),
                Span::styled("thinking", dim),
                Span::styled(if v.is_expanded(&it.id) { "" } else { " …" }, dim),
            ])];
            if v.is_expanded(&it.id)
                && let ItemBody::Text(t) = &it.body
            {
                for l in wrap(&clean_text(t), width.saturating_sub(2).max(8)) {
                    lines.push(Line::from(vec![Span::styled("  ", dim), Span::styled(l, dim)]));
                }
            }
            lines
        }
        Row::Fold(idxs) => {
            let tools: Vec<&ThreadItem> = idxs.iter().map(|i| &items[*i]).collect();
            let n = tools.len();
            let dur = tools.last().map(|l| l.at_ms).unwrap_or(0) - tools.first().map(|f| f.at_ms).unwrap_or(0);
            let failed = tools.iter().any(|t| matches!(t.status, Some(ItemStatus::Failed)));
            let running = tools.iter().any(|t| matches!(t.status, Some(ItemStatus::Running | ItemStatus::Pending)));
            let marker = if selected { base.fg(theme::ACCENT) } else { dim };
            // What the calls did first, in the room the rest leaves:
            // "Read send.rs, main.rs, search for “x” · 4 calls · 12.4s".
            let mut tail = String::new();
            // A lone call has no span to time.
            if n > 1 {
                tail.push_str(&format!(" · {n} calls · {}", duration_label(dur.max(0))));
            }
            if running {
                tail.push_str(" · running");
            }
            if failed {
                tail.push_str(" · failed");
            }
            let room = width.saturating_sub(2 + tail.chars().count()).max(12);
            let what = fold_summary(&tools, room);
            vec![Line::from(vec![
                Span::styled("▸ ", marker),
                Span::styled(ellipsize(&what, room), base),
                Span::styled(ellipsize(&tail, width.saturating_sub(2 + what.chars().count().min(room))), dim),
            ])]
        }
        Row::Tool { idx, .. } => {
            let it = &items[*idx];
            let status = it.status.unwrap_or(ItemStatus::Completed);
            let glyph_style = base.fg(match status {
                ItemStatus::Failed | ItemStatus::Denied => theme::DANGER,
                ItemStatus::Running | ItemStatus::Pending => theme::ACCENT,
                ItemStatus::Completed | ItemStatus::Allowed | ItemStatus::Answered => theme::OK,
                ItemStatus::Cancelled => theme::DIM,
            });
            // What it did, then (dim) what it ran: the command, or its input.
            let (title, input, output) = match &it.body {
                ItemBody::Tool { title, input, output, summary, .. } => {
                    let ran = one_line(title.trim_matches('`'));
                    let detail = if summary.as_str() != ran && !ran.is_empty() { ran } else { input.clone() };
                    (summary.clone(), detail, output.clone())
                }
                _ => ("tool".into(), String::new(), Vec::new()),
            };
            let head = format!("  {} ", status.glyph());
            let room = width.saturating_sub(head.chars().count() + title.chars().count() + 2);
            let mut lines = vec![Line::from(vec![
                Span::styled(head, glyph_style),
                Span::styled(title, base),
                Span::styled(format!("  {}", ellipsize(&input.replace('\n', " "), room)), dim),
            ])];
            if v.is_tool_expanded(&it.id) {
                if output.is_empty() {
                    lines.push(Line::from(Span::styled("      (no output)", dim)));
                }
                for l in output.iter().take(OUTPUT_HEAD) {
                    lines.push(Line::from(vec![Span::styled("      ", dim), Span::styled(ellipsize(l, width.saturating_sub(6)), dim)]));
                }
                if output.len() > OUTPUT_HEAD {
                    lines.push(Line::from(Span::styled(format!("      … +{} lines · l for the full log", output.len() - OUTPUT_HEAD), dim)));
                }
            }
            lines
        }
        Row::Choice(i) => {
            let it = &items[*i];
            let (prompt, options, chosen) = match &it.body {
                ItemBody::Choice { prompt, options, chosen } => (prompt.clone(), options.clone(), chosen.clone()),
                _ => (String::new(), Vec::new(), None),
            };
            let who = it.agent.as_ref().map(|(n, _)| n.clone()).unwrap_or_else(|| "agent".into());
            let (glyph, color, verb) = match it.kind {
                ItemKind::Permission => ("!", theme::DANGER, "asks permission"),
                _ => ("?", Color::Yellow, "asks"),
            };
            if it.status == Some(ItemStatus::Pending) {
                let card = Style::default().bg(SELECTED_BG);
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{glyph} "), card.fg(color).bold()),
                    Span::styled(format!("{who} {verb}: "), card.bold()),
                    Span::styled(ellipsize(&prompt, width.saturating_sub(who.len() + verb.len() + 6)), card),
                ])];
                let sel = v.choice_index(&it.id);
                for (oi, o) in options.iter().enumerate() {
                    let on = oi == sel;
                    lines.push(Line::from(vec![
                        Span::styled(if on { "   ▸ " } else { "     " }, card.fg(theme::ACCENT)),
                        Span::styled(o.label.clone(), if on { card.fg(theme::ACCENT).bold() } else { card }),
                    ]));
                }
                let hint = if selected { "↑↓ pick · Enter answer" } else { "Tab then ↑↓ to this card, Enter answers" };
                lines.push(Line::from(Span::styled(format!("     {hint}"), card.fg(theme::DIM))));
                lines
            } else {
                let outcome = match (it.status, chosen) {
                    (Some(ItemStatus::Denied), c) => format!("denied{}", c.map(|c| format!(": {c}")).unwrap_or_default()),
                    (Some(ItemStatus::Allowed), c) => format!("allowed{}", c.map(|c| format!(": {c}")).unwrap_or_default()),
                    (Some(ItemStatus::Answered), c) => format!("answered{}", c.map(|c| format!(": {c}")).unwrap_or_default()),
                    (Some(ItemStatus::Cancelled), _) => "cancelled".into(),
                    (_, c) => c.unwrap_or_else(|| "resolved".into()),
                };
                let g = it.status.map(ItemStatus::glyph).unwrap_or("·");
                vec![Line::from(vec![
                    Span::styled(format!("{g} "), dim),
                    Span::styled(format!("{who} {verb}: {}", ellipsize(&prompt, width / 2)), dim),
                    Span::styled(format!(" · {outcome}"), dim),
                ])]
            }
        }
    }
}

/// What the timeline says when there is nothing to show yet.
pub fn empty_hint(d: &crate::model::ThreadData) -> String {
    if d.live_sessions.iter().any(|s| s.managed) {
        return "Waiting for the agent…".to_string();
    }
    if let Some(pane) = d.live_sessions.first() {
        return format!(
            "{} is running in a pane · nothing is transcribed here · @name to summon a managed agent",
            pane.agent_name
        );
    }
    match &d.last_agent_name {
        Some(name) => format!(
            "no agent running on this thread · type a message to wake {name} · @name to summon someone else"
        ),
        None => "no agent running on this thread · @name to summon an agent".to_string(),
    }
}

fn render_timeline(frame: &mut Frame, v: &ThreadView, area: Rect, _now: i64) {
    if area.height == 0 || area.width < 4 {
        return;
    }
    let rows = v.rows();
    let inner = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), area.height);
    if rows.is_empty() {
        let msg = empty_hint(&v.data);
        let lines = wrap(&msg, inner.width.saturating_sub(2) as usize);
        let top = inner.y + inner.height.saturating_sub(lines.len() as u16) / 2;
        for (i, l) in lines.into_iter().enumerate() {
            let y = top + i as u16;
            if y >= inner.y + inner.height {
                break;
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(l, theme::dim())).centered()),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
        return;
    }
    let width = inner.width.saturating_sub(1) as usize; // 1 col for the cursor marker
    let focused = v.focus == Focus::Timeline;
    let blocks: Vec<Vec<Line>> =
        rows.iter().enumerate().map(|(i, r)| row_lines(v, r, width, focused && i == v.cursor)).collect();
    let heights: Vec<usize> = blocks.iter().map(|b| b.len()).collect();
    let avail = inner.height as usize;

    // Anchor on the cursor: walk upward from it while rows fit, then fill downward.
    let cursor = v.cursor.min(rows.len() - 1);
    let mut first = cursor;
    let mut used = heights[cursor].min(avail);
    while first > 0 && used + heights[first - 1] <= avail {
        first -= 1;
        used += heights[first];
    }
    // If there is still room below the cursor, use it.
    let mut last = cursor;
    while last + 1 < rows.len() && used + heights[last + 1] <= avail {
        last += 1;
        used += heights[last];
    }

    let mut y = inner.y;
    for (i, block) in blocks.iter().enumerate().take(last + 1).skip(first) {
        for line in block {
            if y >= inner.y + inner.height {
                break;
            }
            let marker = if i == v.cursor { Span::styled("▎", theme::accent()) } else { Span::raw(" ") };
            let mut spans = vec![marker];
            spans.extend(line.spans.iter().cloned());
            frame.render_widget(Paragraph::new(Line::from(spans)), Rect::new(inner.x, y, inner.width, 1));
            y += 1;
        }
    }
    // Scroll indicators.
    if first > 0 {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("↑", theme::dim()))), Rect::new(inner.x + inner.width, inner.y, 1, 1));
    }
    if last + 1 < rows.len() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("↓", theme::dim()))),
            Rect::new(inner.x + inner.width, inner.y + inner.height - 1, 1, 1),
        );
    }
}

/// Prompts not read yet, oldest first: `↳ sent` (into the running turn, read
/// at the agent's next step), `⧗ next` / `⧗ queued` (after the turn),
/// `✎ editing`; with the selection when focused.
pub(crate) fn render_queue(frame: &mut Frame, v: &ThreadView, area: Rect) {
    let q = v.queued();
    let focused = v.focus == Focus::Queue;
    let rows = area.height as usize;
    let n = q.len();
    let shown = if n > rows { rows.saturating_sub(1) } else { n };
    // Keep the selection visible while scrolling a long queue.
    let first = if focused { v.queue_sel.saturating_sub(shown.saturating_sub(1)) } else { 0 };
    let width = area.width.saturating_sub(2) as usize;
    for (r, &i) in q.iter().skip(first).take(shown).enumerate() {
        let idx = first + r;
        let it = &v.data.items[i];
        let text = match &it.body {
            ItemBody::Text(t) => t.trim().replace('\n', " "),
            _ => String::new(),
        };
        let selected = focused && idx == v.queue_sel;
        let style = if selected { Style::default().bg(SELECTED_BG) } else { Style::default() };
        // Sent into the turn: read at the agent's next step. Queued: after the turn.
        let editing = v.editing.as_deref() == Some(it.id.as_str());
        let label = if editing {
            "✎ editing"
        } else if it.steering {
            "↳ sent   "
        } else if idx == 0 {
            "⧗ next   "
        } else {
            "⧗ queued "
        };
        let files = if it.attachments.is_empty() { String::new() } else { format!("  +{} file(s)", it.attachments.len()) };
        let room = width.saturating_sub(label.chars().count() + 2 + files.chars().count());
        let line = Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, style.fg(theme::ACCENT)),
            Span::styled(label, style.fg(theme::ACCENT)),
            Span::styled("  ", style),
            Span::styled(ellipsize(&text, room), if selected { style.bold() } else { theme::dim() }),
            Span::styled(files, theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x + 1, area.y + r as u16, area.width.saturating_sub(1), 1));
    }
    if n > shown {
        let more = Line::from(Span::styled(format!("    +{} more · Tab to browse", n - shown), theme::dim()));
        frame.render_widget(Paragraph::new(more), Rect::new(area.x + 1, area.y + shown as u16, area.width.saturating_sub(1), 1));
    }
}

/// The prompt's leading glyph; continuation rows are indented by its width.
const PROMPT: &str = "› ";
/// The most rows a message being typed takes before it scrolls.
pub const MAX_INPUT_ROWS: usize = 8;

/// Rows the composer text takes: the input laid out beside the prompt glyph
/// (see [`input_rows`]), with room for the cursor after the last character.
pub fn composer_text_rows(v: &ThreadView, width: u16, max: usize) -> usize {
    let w = (width as usize).saturating_sub(PROMPT.chars().count()).max(1);
    let chars: Vec<char> = v.composer.text().chars().collect();
    input_rows(&chars, w, v.composer.cursor()).0.len().clamp(1, max.max(1))
}

/// The composer's text as screen rows of at most `width` characters, broken
/// at line breaks (which no row shows) and at the width; and the cursor's
/// (row, column).  Each row is a `(start, end)` range of `chars`.  A cursor
/// right after a full row gets a row of its own.
pub fn input_rows(chars: &[char], width: usize, cursor: usize) -> (Vec<(usize, usize)>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    loop {
        let line_end = chars[start..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| start + i);
        let mut s = start;
        loop {
            let e = (s + width).min(line_end);
            rows.push((s, e));
            if e >= line_end {
                break;
            }
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
            at = Some((r, cursor - s));
            break;
        }
    }
    let at = match at {
        Some((r, c)) if c >= width => {
            // After a full row: the next row, which may not exist yet.
            rows.insert(r + 1, (cursor, cursor));
            (r + 1, 0)
        }
        Some(p) => p,
        None => (rows.len().saturating_sub(1), 0),
    };
    (rows, at)
}

/// Composer height: text rows · chips row · rule · footer.
pub fn composer_height(v: &ThreadView, area: Rect) -> u16 {
    // The composer pane is sized to its text (see `compose::desired_height`):
    // it always takes up to the most rows, so the size it asks for and the
    // size it draws agree. In the full view, a short screen keeps two.
    let max = if v.compose_only || area.height >= 20 { MAX_INPUT_ROWS } else { 2 };
    let chips = u16::from(!v.attachments.is_empty());
    composer_text_rows(v, area.width, max) as u16 + chips + 2
}

/// `→ atlas · ⏵⏵ autopilot on (shift+tab to cycle)`: who the composer's text
/// would reach, then the mode of the session it talks to.
fn mode_spans(v: &ThreadView) -> Vec<Span<'static>> {
    let target = v.target().map(|t| {
        let color = v
            .data
            .known_agents
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(t.trim_start_matches('+')))
            .map_or(theme::ACCENT, |(_, c)| theme::agent_color(*c));
        vec![Span::styled("→ ".to_string(), theme::dim()), Span::styled(t, Style::default().fg(color)), Span::styled(" · ".to_string(), theme::dim())]
    });
    let Some(s) = v.mode_session() else { return target.unwrap_or_default() };
    let (label, color) = match s.mode {
        crate::model::RuntimeMode::Autopilot => ("⏵⏵ autopilot on", theme::ACCENT),
        crate::model::RuntimeMode::Edits => ("⏵⏵ accept edits on", Color::Indexed(141)),
        crate::model::RuntimeMode::Plan => ("⏸ plan mode on", Color::Indexed(73)),
        crate::model::RuntimeMode::Supervised => ("⏵ asks before acting", theme::DIM),
    };
    let mut spans = target.unwrap_or_default();
    spans.extend([
        Span::styled(label.to_string(), Style::default().fg(color)),
        Span::styled(" (shift+tab to cycle)".to_string(), theme::dim()),
    ]);
    if let Some(h) = &s.host {
        spans.push(Span::styled(format!(" · {} on {h}", s.agent_name), theme::accent()));
    }
    spans
}

/// The input, a rule under it (no box), then a footer: the mode on the left,
/// the status or the key help on the right.
/// Per-character styles for the composer: an `@name` naming a known agent
/// (case-insensitive, the whole name typed) takes that agent's colour.
pub(crate) fn mention_styles(chars: &[char], agents: &[(String, Option<axel_store::AgentColor>)]) -> Vec<Style> {
    let is_name = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut styles = vec![Style::default(); chars.len()];
    let mut i = 0;
    while i < chars.len() {
        let at_start = i == 0 || !is_name(chars[i - 1]);
        if chars[i] == '@' && at_start {
            let mut end = i + 1;
            while end < chars.len() && is_name(chars[end]) {
                end += 1;
            }
            let name: String = chars[i + 1..end].iter().collect();
            if let Some((_, color)) = agents.iter().find(|(n, _)| !name.is_empty() && n.eq_ignore_ascii_case(&name)) {
                let st = Style::default().fg(theme::agent_color(*color)).bold();
                styles[i..end].iter_mut().for_each(|s| *s = st);
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    styles
}

/// Agents an `@name` can name: the thread's, then every other local agent.
fn mention_names(d: &crate::model::ThreadData) -> Vec<(String, Option<axel_store::AgentColor>)> {
    let mut out: Vec<(String, Option<axel_store::AgentColor>)> = d.agents.iter().map(|(n, c, _)| (n.clone(), *c)).collect();
    for (n, c) in &d.known_agents {
        if !out.iter().any(|(o, _)| o.eq_ignore_ascii_case(n)) {
            out.push((n.clone(), *c));
        }
    }
    out
}

/// Group runs of equally styled characters into spans.
pub(crate) fn styled_run(chars: &[char], styles: &[Style]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (c, st) in chars.iter().zip(styles) {
        if cur.is_some_and(|s| s != *st) {
            out.push(Span::styled(std::mem::take(&mut buf), cur.unwrap()));
        }
        cur = Some(*st);
        buf.push(*c);
    }
    if let Some(st) = cur {
        out.push(Span::styled(buf, st));
    }
    out
}

pub(crate) fn render_composer(frame: &mut Frame, v: &ThreadView, area: Rect) {
    if area.height == 0 || area.width < 4 {
        return;
    }
    let focused = v.focus == Focus::Composer;
    let chips_focused = v.focus == Focus::Attachments;
    let width = area.width as usize;
    let rule = Line::from(Span::styled("─".repeat(width), theme::dim()));
    // A pane shorter than the full layout (it opened small, or the split
    // hasn't settled yet) still shows the input: the rule goes first, then
    // the footer.
    let chips_h = u16::from(!v.attachments.is_empty()).min(area.height - 1);
    let spare = area.height - 1 - chips_h;
    let (has_footer, has_bottom) = (spare >= 1, spare >= 2);
    let chrome = u16::from(has_footer) + u16::from(has_bottom);
    let text_rows = (area.height - chips_h - chrome).max(1) as usize;
    let text_area = Rect::new(area.x, area.y, area.width, text_rows as u16);
    let chips_row = Rect::new(area.x, area.y + text_rows as u16, area.width, chips_h);
    let bottom = Rect::new(area.x, chips_row.y + chips_h, area.width, 1);
    let footer = Rect::new(area.x, bottom.y + u16::from(has_bottom), area.width, 1);
    if has_bottom {
        frame.render_widget(Paragraph::new(rule), bottom);
    }

    let indent = PROMPT.chars().count();
    let glyph = Span::styled(PROMPT, if focused || chips_focused { theme::accent() } else { theme::dim() });
    let body_w = width.saturating_sub(indent).max(1);
    if v.composer.is_empty() {
        let hint = if v.awaiting_answer() {
            "send a message to decline and redirect the agent · Tab to answer"
        } else if v.is_working() {
            "agent is working · Enter sends into its turn · ^S interrupts"
        } else {
            if v.compose_only {
                "message the thread · @name for someone else"
            } else {
                "message the thread · @name for someone else · Esc dashboard"
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![glyph, Span::styled(ellipsize(hint, body_w), theme::dim())])),
            Rect::new(text_area.x, text_area.y, text_area.width, 1),
        );
        if focused {
            frame.set_cursor_position(Position::new(text_area.x + indent as u16, text_area.y));
        }
    } else {
        // Rows broken at line breaks and the width, keeping the cursor row visible.
        let chars: Vec<char> = v.composer.text().chars().collect();
        let styles = mention_styles(&chars, &mention_names(&v.data));
        let (rows, (cursor_row, cursor_col)) = input_rows(&chars, body_w, v.composer.cursor());
        // The composer pane grows to fit its text: while it catches up with a
        // new line, keep the top in place rather than scrolling the first line
        // away and back. Scroll past the most rows it grows to, or in the full
        // view, whose layout has no pane to grow.
        let first_row = if v.compose_only && rows.len() <= MAX_INPUT_ROWS { 0 } else { cursor_row.saturating_sub(text_rows - 1) };
        for (r, &(start, end)) in rows.iter().enumerate().skip(first_row).take(text_rows) {
            let lead = if r == 0 { glyph.clone() } else { Span::raw(" ".repeat(indent)) };
            let mut spans = vec![lead];
            spans.extend(styled_run(&chars[start..end], &styles[start..end]));
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(text_area.x, text_area.y + (r - first_row) as u16, text_area.width, 1),
            );
        }
        // The cursor's row may be the one the pane is still growing to show.
        if focused && cursor_row - first_row < text_rows {
            let cx = (indent + cursor_col) as u16;
            let cy = (cursor_row - first_row) as u16;
            frame.set_cursor_position(Position::new(text_area.x + cx, text_area.y + cy));
        }
    }
    if chips_h > 0 {
        let selected = chips_focused.then_some(v.attach_sel);
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(chips_spans(&v.attachments, body_w, selected));
        frame.render_widget(Paragraph::new(Line::from(spans)), chips_row);
    }

    if !has_footer {
        return;
    }
    // Footer: mode left; status (or as much key help as fits) right.  When the
    // right side would be cut, the "(shift+tab to cycle)" hint goes first.
    let fit = |left: &[Span<'static>]| -> Line<'static> {
        let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
        let room = width.saturating_sub(left_w + 2);
        match &v.status {
            Some(st) => {
                let style = if st.starts_with("error") { theme::danger() } else { theme::accent() };
                Line::from(Span::styled(ellipsize(st, room), style))
            }
            None => {
                let mut items = v.help();
                let mut line = help_line(&items);
                while !items.is_empty() && line.width() > room {
                    items.pop();
                    line = help_line(&items);
                }
                line
            }
        }
    };
    let mut left = vec![Span::raw("  ")];
    left.extend(mode_spans(v));
    let mut right = fit(&left);
    let full = match &v.status {
        Some(st) => st.chars().count(),
        None => help_line(&v.help()).width(),
    };
    // Tight: drop the "(shift+tab to cycle)" hint and the host, keep who and the mode.
    if right.width() < full
        && let Some(i) = left.iter().position(|s| s.content.starts_with(" (shift+tab"))
    {
        left.truncate(i);
        right = fit(&left);
    }
    frame.render_widget(Paragraph::new(Line::from(left)), footer);
    let rw = (right.width() as u16 + 1).min(footer.width);
    frame.render_widget(Paragraph::new(right), Rect::new(footer.x + footer.width - rw, footer.y, rw.saturating_sub(1), 1));
}
