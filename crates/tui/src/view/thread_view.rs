//! Thread view rendering: header · timeline · composer.

use axel_i18n::t;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{chips_spans, help_line};
use crate::model::{ItemBody, ItemKind, ItemStatus, ThreadItem, state_glyph};
use crate::theme;
use crate::thread::{Focus, Row, ThreadView};
use crate::util::{cells, char_width, ellipsize, format_tokens, pad_end, text_width};

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
    cells(n.min(cap + 1))
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
    let mut spans = Vec::new();
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
    spans.push(Span::styled(format!("  ·  {}", t!("${cost} est", cost = format!("{:.2}", d.cost_usd))), theme::dim()));
    // The title takes up to half the row, less when the rest needs it (a
    // CJK title is twice as wide), so what's on the right isn't cut.
    let rest: usize = spans.iter().map(Span::width).sum();
    let room = usize::from(area.width / 2).min(usize::from(area.width).saturating_sub(rest + 1)).max(8);
    spans.splice(0..0, [Span::raw(" "), Span::styled(ellipsize(&d.title, room), theme::accent_bold())]);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Word-wrap `text` to `width` terminal cells, preserving explicit newlines.
/// A word too long for a row starts a row and is broken between characters,
/// never inside a wide one. A word with CJK in it needs no space to break,
/// so it runs on from where the row stands instead of starting a new one.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut len = 0usize;
        for word in para.split(' ') {
            let wl = text_width(word);
            let is_cjk = word.chars().any(|c| char_width(c) > 1);
            let first = word.chars().next().map_or(0, char_width);
            if len > 0 && len + 1 + wl > width && !(is_cjk && len + 1 + first <= width) {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            if len > 0 {
                line.push(' ');
                len += 1;
            }
            if len + wl > width {
                for ch in word.chars() {
                    let cw = char_width(ch);
                    if len > 0 && len + cw > width {
                        out.push(std::mem::take(&mut line));
                        len = 0;
                    }
                    line.push(ch);
                    len += cw;
                }
                continue;
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
/// The marker in front of your own messages (accent, bold).
pub const YOU_MARK: &str = "> ";

/// How far a message's attachment chips sit in: under your text, past the mark.
pub fn chips_indent(it: &ThreadItem) -> usize {
    if it.agent.is_none() && it.author.is_none() { text_width(YOU_MARK) } else { 0 }
}
/// The same accent used by the composer and selections.
pub const YOU_COLOR: Color = theme::ACCENT;

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
    let w = text_width(right);
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
    let indent: usize = prefix.iter().map(|s| s.width()).sum();
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
    let indent: usize = prefix.iter().map(|s| s.width()).sum();
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
/// Tool runs, thoughts and approvals sit flush left, like prose.
#[doc(hidden)]
pub fn row_lines(v: &ThreadView, row: &Row, width: usize, selected: bool) -> Vec<Line<'static>> {
    let paint = RowPaint::new(selected);
    match row {
        Row::Prose(i) => prose_row(v, *i, width, paint),
        Row::Changes(i) => changes_row(&v.data.items[*i], paint),
        Row::Thought(i) => thought_row(v, &v.data.items[*i], width, paint),
        Row::Fold(idxs) => fold_row(v, idxs, width, paint),
        Row::Tool { idx, .. } => tool_row(v, &v.data.items[*idx], width, paint),
        Row::Choice(i) => choice_row(v, &v.data.items[*i], width, paint),
    }
}

/// The styles a row draws with: reversed when it is the selected row.
#[derive(Clone, Copy)]
struct RowPaint {
    selected: bool,
    base: Style,
    dim: Style,
}

impl RowPaint {
    fn new(selected: bool) -> Self {
        let base = if selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
        let dim = if selected { theme::dim().add_modifier(Modifier::REVERSED) } else { theme::dim() };
        RowPaint { selected, base, dim }
    }

    /// The row's marker glyph: the accent when selected.
    fn marker(self) -> Style {
        if self.selected { self.base.fg(theme::ACCENT) } else { self.dim }
    }
}

fn prose_row(v: &ThreadView, i: usize, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { base, dim, .. } = paint;
    let items = &v.data.items;
    let it = &items[i];
    match (&it.kind, &it.body) {
        (ItemKind::Error, ItemBody::Text(t)) => prose_lines(vec![Span::styled("! ", base.fg(theme::DANGER).bold())], t, width, base.fg(theme::DANGER)),
        (ItemKind::Answer, ItemBody::Answered(label)) => vec![Line::from(Span::styled(t!("↩ answered: {answer}", answer = label), dim))],
        (ItemKind::UserMessage, ItemBody::Text(t)) if it.status == Some(ItemStatus::Cancelled) => {
            // Queued but never sent (the session ended first).
            let mut lines = prose_lines(you_prefix(dim, false), t.trim(), width, dim);
            if let Some(first) = lines.first_mut() {
                *first = with_right(std::mem::take(first), &clock(it.at_ms), width, dim);
            }
            lines.push(Line::from(Span::styled(format!("  {}", t!("× not sent · session ended while queued")), dim)));
            lines
        }
        (ItemKind::UserMessage, ItemBody::Text(t)) if it.relay_to.is_some() => relay_lines(v, i, t, width, paint),
        (ItemKind::UserMessage, ItemBody::Text(t)) => user_message_lines(v, i, t, width, paint),
        (_, ItemBody::Text(t)) => {
            // Agent: header line (badge + time), the response starts below
            // it. Right after its own message, the same agent goes on under it.
            let mut lines = header_lines(items, i, base, width);
            // Agents write Markdown: show it styled, its markers dropped.
            lines.extend(super::markdown::md_lines(&clean_text(t), width, base));
            lines
        }
        _ => vec![Line::from(Span::styled("·", dim))],
    }
}

/// One agent to another: under the sender's badge (unless its previous
/// message already shows it), marked and set off.
fn relay_lines(v: &ThreadView, i: usize, text: &str, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { base, dim, .. } = paint;
    let it = &v.data.items[i];
    let mut lines = header_lines(&v.data.items, i, base, width);
    let to = it.relay_to.clone().unwrap_or_default();
    lines.push(Line::from(vec![Span::styled("→ ", dim), Span::styled(to.to_lowercase(), dim.bold())]));
    let gutter = Span::styled(RELAY_GUTTER, dim);
    for l in user_lines(Vec::new(), text.trim(), width.saturating_sub(text_width(RELAY_GUTTER)), dim, &v.mention_names()) {
        let mut spans = vec![gutter.clone()];
        spans.extend(l.spans);
        lines.push(Line::from(spans));
    }
    lines
}

/// Your message, or another participant's under their header.
fn user_message_lines(v: &ThreadView, i: usize, text: &str, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { base, dim, .. } = paint;
    let it = &v.data.items[i];
    let prefix = if speaker_header(it, base, width).is_some() { Vec::new() } else { you_prefix(base, true) };
    let mut lines = user_lines(prefix, text.trim(), width, base, &v.mention_names());
    match speaker_header(it, base, width) {
        // Another participant: header line, text below.
        Some(_) => {
            let head = header_lines(&v.data.items, i, base, width);
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
        let indent = chips_indent(it);
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

fn changes_row(it: &ThreadItem, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { selected, base, dim } = paint;
    let ItemBody::Changes { files, authored, added, removed, between_turns } = &it.body else { return Vec::new() };
    // "3 files  +42 −7"; with other hands in the tree, the agent's own
    // files first: "2 files (+1 not this agent)  +42 −7".
    let (own, other) = if *authored > 0 && authored < files { (*authored, files - authored) } else { (*files, 0) };
    let mut what = if own == 1 { t!("1 file").to_string() } else { t!("{n} files", n = own) };
    if other > 0 {
        what.push(' ');
        what.push_str(&t!("(+{n} not this agent)", n = other));
    }
    if *between_turns {
        what.push(' ');
        what.push_str(t!("incl. changes between turns"));
    }
    let mut spans = vec![Span::styled("  ↗ ", paint.marker()), Span::styled(what, dim), Span::styled("  ", dim)];
    spans.push(Span::styled(format!("+{added}"), if selected { base.fg(theme::OK) } else { Style::default().fg(theme::OK) }));
    spans.push(Span::styled(" ", dim));
    spans.push(Span::styled(format!("−{removed}"), if selected { base.fg(theme::DANGER) } else { Style::default().fg(theme::DANGER) }));
    vec![Line::from(spans)]
}

fn thought_row(v: &ThreadView, it: &ThreadItem, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let dim = paint.dim;
    let mut lines = vec![Line::from(vec![
        Span::styled("· ", dim),
        Span::styled(t!("thinking"), dim),
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

/// A run of tool calls, folded to one line: what they did first, in the
/// room the rest leaves: "Read send.rs, main.rs, search for “x” · 4 calls · 12.4s".
fn fold_row(v: &ThreadView, idxs: &[usize], width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { base, dim, .. } = paint;
    let tools: Vec<&ThreadItem> = idxs.iter().map(|i| &v.data.items[*i]).collect();
    let n = tools.len();
    let duration_ms = tools.last().map(|l| l.at_ms).unwrap_or(0) - tools.first().map(|f| f.at_ms).unwrap_or(0);
    let failed = tools.iter().any(|t| matches!(t.status, Some(ItemStatus::Failed)));
    let running = tools.iter().any(|t| matches!(t.status, Some(ItemStatus::Running | ItemStatus::Pending)));
    let mut tail = String::new();
    // A lone call has no span to time.
    if n > 1 {
        tail.push_str(&format!(" · {} · {}", t!("{n} calls", n = n), duration_label(duration_ms.max(0))));
    }
    if running {
        tail.push_str(&format!(" · {}", t!("running")));
    }
    if failed {
        tail.push_str(&format!(" · {}", t!("failed")));
    }
    let room = width.saturating_sub(2 + text_width(&tail)).max(12);
    let what = fold_summary(&tools, room);
    vec![Line::from(vec![
        Span::styled("↗ ", paint.marker()),
        Span::styled(ellipsize(&what, room), base),
        Span::styled(ellipsize(&tail, width.saturating_sub(2 + text_width(&what).min(room))), dim),
    ])]
}

/// One tool call: what it did, then (dim) what it ran: the command, or its
/// input; expanded, the head of its output.
fn tool_row(v: &ThreadView, it: &ThreadItem, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let RowPaint { base, dim, .. } = paint;
    let status = it.status.unwrap_or(ItemStatus::Completed);
    let glyph_style = base.fg(match status {
        ItemStatus::Failed | ItemStatus::Denied => theme::DANGER,
        ItemStatus::Running | ItemStatus::Pending => theme::ACCENT,
        ItemStatus::Completed | ItemStatus::Allowed | ItemStatus::Answered => theme::OK,
        ItemStatus::Cancelled => theme::DIM,
    });
    let (title, input, output) = match &it.body {
        ItemBody::Tool { title, input, output, summary, .. } => {
            let ran = one_line(title.trim_matches('`'));
            let detail = if summary.as_str() != ran && !ran.is_empty() { ran } else { input.clone() };
            (summary.clone(), detail, output.clone())
        }
        _ => (t!("tool").into(), String::new(), Vec::new()),
    };
    let head = format!("  {} ", status.glyph());
    let room = width.saturating_sub(text_width(&head) + text_width(&title) + 2);
    let mut lines = vec![Line::from(vec![
        Span::styled(head, glyph_style),
        Span::styled(title, base),
        Span::styled(format!("  {}", ellipsize(&input.replace('\n', " "), room)), dim),
    ])];
    if v.is_tool_expanded(&it.id) {
        if output.is_empty() {
            lines.push(Line::from(Span::styled(format!("      {}", t!("(no output)")), dim)));
        }
        for l in output.iter().take(OUTPUT_HEAD) {
            lines.push(Line::from(vec![Span::styled("      ", dim), Span::styled(ellipsize(l, width.saturating_sub(6)), dim)]));
        }
        if output.len() > OUTPUT_HEAD {
            lines.push(Line::from(Span::styled(format!("      {}", t!("… +{n} lines · l for the full log", n = output.len() - OUTPUT_HEAD)), dim)));
        }
    }
    lines
}

/// A permission or question: a card with its options while pending, one
/// dim line with the outcome once resolved.
fn choice_row(v: &ThreadView, it: &ThreadItem, width: usize, paint: RowPaint) -> Vec<Line<'static>> {
    let (prompt, options, chosen, multiple) = match &it.body {
        ItemBody::Choice { prompt, options, chosen, multiple } => (prompt.clone(), options.clone(), chosen.clone(), *multiple),
        _ => (String::new(), Vec::new(), None, None),
    };
    let who = it.agent.as_ref().map(|(n, _)| n.clone()).unwrap_or_else(|| t!("agent").into());
    let (glyph, color) = match it.kind {
        ItemKind::Permission => ("!", theme::DANGER),
        _ => ("?", Color::Yellow),
    };
    if it.status != Some(ItemStatus::Pending) {
        return vec![resolved_choice_line(it, chosen, &who, &prompt, width, paint.dim)];
    }
    let card = Style::default();
    let mut lines = question_header(glyph, color, it.kind, &who, &prompt, width);
    let sel = v.choice_index(&it.id);
    for (oi, o) in options.iter().enumerate() {
        let on = oi == sel;
        let checkbox = if multiple.is_some() {
            if v.choice_checked.get(&it.id).is_some_and(|s|s.contains(&o.id)) {"[x] "} else {"[ ] "}
        } else {""};
        // The whole option, wrapped under its own text; the marker
        // alone carries the accent, the picked text is just bold.
        let indent = 5 + checkbox.len();
        let text = if on { card.bold() } else { card };
        for (li, l) in wrap(&o.label, width.saturating_sub(indent).max(1)).into_iter().enumerate() {
            let lead = match li {
                0 => Span::styled(if on { "   ▸ " } else { "     " }, card.fg(theme::ACCENT)),
                _ => Span::raw("     "),
            };
            let pad = if li == 0 { checkbox.to_string() } else { " ".repeat(checkbox.len()) };
            lines.push(Line::from(vec![lead, Span::styled(format!("{pad}{l}"), text)]));
        }
    }
    let hint = if multiple.is_some() {
        t!("↑↓ move · Space toggle · Enter confirm")
    } else if paint.selected {
        t!("↑↓ pick · Enter answer")
    } else {
        t!("Tab then ↑↓ to this card, Enter answers")
    };
    lines.push(Line::from(Span::styled(format!("     {hint}"), card.fg(theme::DIM))));
    lines
}

fn resolved_choice_line(it: &ThreadItem, chosen: Option<String>, who: &str, prompt: &str, width: usize, dim: Style) -> Line<'static> {
    let outcome = match (it.status, chosen) {
        (Some(ItemStatus::Denied), Some(c)) => t!("denied: {choice}", choice = c),
        (Some(ItemStatus::Denied), None) => t!("denied").into(),
        (Some(ItemStatus::Allowed), Some(c)) => t!("allowed: {choice}", choice = c),
        (Some(ItemStatus::Allowed), None) => t!("allowed").into(),
        (Some(ItemStatus::Answered), Some(c)) => t!("answered: {choice}", choice = c),
        (Some(ItemStatus::Answered), None) => t!("answered").into(),
        (Some(ItemStatus::Cancelled), _) => t!("cancelled").into(),
        (_, c) => c.unwrap_or_else(|| t!("resolved").into()),
    };
    let g = it.status.map(ItemStatus::glyph).unwrap_or("·");
    Line::from(vec![
        Span::styled(format!("{g} "), dim),
        Span::styled(asks(it.kind, who, &ellipsize(prompt, width / 2)), dim),
        Span::styled(format!(" · {outcome}"), dim),
    ])
}

/// `atlas asks: <prompt>`, or `atlas asks permission: <prompt>` for a
/// permission: one sentence, so a language can put the name where it goes.
pub(crate) fn asks(kind: ItemKind, who: &str, prompt: &str) -> String {
    match kind {
        ItemKind::Permission => t!("{who} asks permission: {title}", who = who, title = prompt),
        _ => t!("{who} asks: {title}", who = who, title = prompt),
    }
}

/// `? atlas asks: <prompt>`, the whole prompt wrapped under itself: a question
/// is read in full before it is answered, so it is never cut short.
pub(crate) fn question_header(glyph: &str, color: Color, kind: ItemKind, who: &str, prompt: &str, width: usize) -> Vec<Line<'static>> {
    // The sentence without its prompt is the part shown bold.
    let lead = asks(kind, who, "");
    let mut out = Vec::new();
    for (i, l) in wrap(&asks(kind, who, prompt.trim()), width.saturating_sub(2).max(1)).into_iter().enumerate() {
        let mut spans = vec![if i == 0 { Span::styled(format!("{glyph} "), Style::default().fg(color).bold()) } else { Span::raw("  ") }];
        match l.strip_prefix(&lead) {
            Some(rest) if i == 0 => {
                spans.push(Span::styled(lead.clone(), Style::default().bold()));
                spans.push(Span::raw(rest.to_string()));
            }
            _ => spans.push(Span::raw(l)),
        }
        out.push(Line::from(spans));
    }
    out
}

/// What the timeline says when there is nothing to show yet.
pub fn empty_hint(d: &crate::model::ThreadData) -> String {
    if d.live_sessions.iter().any(|s| s.managed) {
        return t!("Waiting for the agent…").to_string();
    }
    if let Some(pane) = d.live_sessions.first() {
        return t!("{name} is running in a pane · nothing is transcribed here · @name to summon a managed agent", name = pane.agent_name);
    }
    match &d.last_agent_name {
        Some(name) => t!("no agent running on this thread · type a message to wake {name} · @name to summon someone else", name = name),
        None => t!("no agent running on this thread · @name to summon an agent").to_string(),
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
        let top = inner.y + inner.height.saturating_sub(cells(lines.len())) / 2;
        for (i, l) in lines.into_iter().enumerate() {
            let y = top + cells(i);
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
        let style = if selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
        // Sent into the turn: read at the agent's next step. Queued: after the turn.
        let editing = v.editing.as_deref() == Some(it.id.as_str());
        let label = if editing {
            t!("✎ editing")
        } else if it.steering {
            t!("↳ sent")
        } else if idx == 0 {
            t!("⧗ next")
        } else {
            t!("⧗ queued")
        };
        // The labels line up: each padded to the widest.
        let label_width = [t!("✎ editing"), t!("↳ sent"), t!("⧗ next"), t!("⧗ queued")].iter().map(|l| text_width(l)).max().unwrap_or(0);
        let label = pad_end(label, label_width);
        let files = if it.attachments.is_empty() { String::new() } else { format!("  {}", t!("+{n} file(s)", n = it.attachments.len())) };
        let room = width.saturating_sub(text_width(&label) + 2 + text_width(&files));
        let line = Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, style.fg(theme::ACCENT)),
            Span::styled(label, style.fg(theme::ACCENT)),
            Span::styled("  ", style),
            Span::styled(ellipsize(&text, room), if selected { style.bold() } else { theme::dim() }),
            Span::styled(files, theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x + 1, area.y + cells(r), area.width.saturating_sub(1), 1));
    }
    if n > shown {
        let more = Line::from(Span::styled(format!("    {}", t!("+{n} more · Tab to browse", n = n - shown)), theme::dim()));
        frame.render_widget(Paragraph::new(more), Rect::new(area.x + 1, area.y + cells(shown), area.width.saturating_sub(1), 1));
    }
}

/// The prompt's leading glyph; continuation rows are indented by its width.
const PROMPT: &str = "› ";
/// The most rows a message being typed takes before it scrolls.
pub const MAX_INPUT_ROWS: usize = 8;

/// Rows the composer text takes: the input laid out beside the prompt glyph
/// (see [`input_rows`]), with room for the cursor after the last character.
pub fn composer_text_rows(v: &ThreadView, width: u16, max: usize) -> usize {
    let w = (width as usize).saturating_sub(text_width(PROMPT)).max(1);
    let chars: Vec<char> = v.composer.text().chars().collect();
    input_rows(&chars, w, v.composer.cursor()).0.len().clamp(1, max.max(1))
}

/// The composer's text as screen rows of at most `width` characters, broken
/// at line breaks (which no row shows) and at the width; and the cursor's
/// (row, column).  Each row is a `(start, end)` range of `chars`.  A cursor
/// right after a full row gets a row of its own.
pub fn input_rows(chars: &[char], width: usize, cursor: usize) -> (Vec<(usize, usize)>, (usize, usize)) {
    crate::input::input_rows(chars, width, cursor)
}

/// Composer height: text rows · chips row · rule · footer.
pub fn composer_height(v: &ThreadView, area: Rect) -> u16 {
    // The composer pane is sized to its text (see `compose::desired_height`):
    // it always takes up to the most rows, so the size it asks for and the
    // size it draws agree. In the full view, a short screen keeps two.
    let max = if v.compose_only || area.height >= 20 { MAX_INPUT_ROWS } else { 2 };
    let chips = u16::from(!v.attachments.is_empty());
    cells(composer_text_rows(v, area.width, max)) + menu_rows(v) + chips + 2
}

/// Rows the `@name` or `/skill` suggestions take under the composer's text.
fn menu_rows(v: &ThreadView) -> u16 {
    cells(v.mention_options().len().max(v.skill_options().len()).min(crate::mention::MAX_ROWS))
}

/// `run [ axel update ] to get 0.3.139`, the command a pill; only the pill
/// when that's all there's room for.
fn update_line(version: &str, room: usize) -> Line<'static> {
    let pill = Span::styled(" axel update ", theme::pill());
    // `{command}` isn't filled: the pill goes where it stands.
    let sentence = t!("run {command} to get {version}", version = version);
    let (before, after) = sentence.split_once("{command}").unwrap_or(("", ""));
    let full = Line::from(vec![Span::styled(before.to_string(), theme::dim()), pill.clone(), Span::styled(after.to_string(), theme::dim())]);
    if full.width() <= room { full } else { Line::from(pill) }
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
        crate::model::RuntimeMode::Autopilot => (t!("⏵⏵ autopilot on"), theme::ACCENT),
        crate::model::RuntimeMode::Edits => (t!("⏵⏵ accept edits on"), Color::Indexed(141)),
        crate::model::RuntimeMode::Plan => (t!("⏸ plan mode on"), Color::Indexed(73)),
        crate::model::RuntimeMode::Supervised => (t!("⏵ asks before acting"), theme::DIM),
    };
    let mut spans = target.unwrap_or_default();
    spans.extend([Span::styled(label.to_string(), Style::default().fg(color)), Span::styled(cycle_hint(), theme::dim())]);
    if let Some(h) = &s.host {
        spans.push(Span::styled(format!(" · {}", t!("{agent} on {host}", agent = s.agent_name, host = h)), theme::accent()));
    }
    spans
}

/// The footer's "(shift+tab to cycle)", the first thing to go when it's tight.
fn cycle_hint() -> String {
    format!(" {}", t!("(shift+tab to cycle)"))
}

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

/// The input, a rule under it (no box), then a footer: the mode on the left,
/// the status or the key help on the right.
pub(crate) fn render_composer(frame: &mut Frame, v: &ThreadView, area: Rect) {
    if area.height == 0 || area.width < 4 {
        return;
    }
    let focused = v.focus == Focus::Composer;
    let chips_focused = v.focus == Focus::Attachments;
    let width = area.width as usize;
    let layout = ComposerLayout::new(v, area);
    if let Some(bottom) = layout.bottom {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("─".repeat(width), theme::dim()))), bottom);
    }

    let indent = text_width(PROMPT);
    let glyph = Span::styled(PROMPT, if focused || chips_focused { theme::accent() } else { theme::dim() });
    let body_width = width.saturating_sub(indent).max(1);
    if v.composer.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![glyph, Span::styled(ellipsize(composer_hint(v), body_width), theme::dim())])),
            Rect::new(layout.text.x, layout.text.y, layout.text.width, 1),
        );
        if focused {
            frame.set_cursor_position(Position::new(layout.text.x + cells(indent), layout.text.y));
        }
    } else {
        render_composer_text(frame, v, layout.text, glyph, focused);
    }
    if layout.menu.height > 0 {
        let lines = match &v.skills {
            Some((_, skills)) if !v.skill_options().is_empty() => v.skill_menu.lines(&v.composer, skills, indent, width),
            _ => v.mention.lines(&v.composer, &v.mention_names(), indent, width),
        };
        frame.render_widget(Paragraph::new(lines), layout.menu);
    }
    if layout.chips.height > 0 {
        let selected = chips_focused.then_some(v.attach_sel);
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(chips_spans(&v.attachments, body_width, selected));
        frame.render_widget(Paragraph::new(Line::from(spans)), layout.chips);
    }
    if let Some(footer) = layout.footer {
        render_composer_footer(frame, v, footer);
    }
}

/// Where the composer's parts go, top to bottom: text, `@name` menu, chips,
/// the rule and the footer (each of those two only when there is room).
struct ComposerLayout {
    text: Rect,
    menu: Rect,
    chips: Rect,
    bottom: Option<Rect>,
    footer: Option<Rect>,
}

impl ComposerLayout {
    fn new(v: &ThreadView, area: Rect) -> Self {
        assert!(area.height > 0, "the caller skips an empty area");
        // A pane shorter than the full layout (it opened small, or the split
        // hasn't settled yet) still shows the input: the rule goes first, then
        // the footer.
        let chips_height = u16::from(!v.attachments.is_empty()).min(area.height - 1);
        // The suggestions for an `@name` sit right under the text, like a shell's completions.
        let menu_height = menu_rows(v).min(area.height - 1 - chips_height);
        let spare = area.height - 1 - chips_height - menu_height;
        let (has_footer, has_bottom) = (spare >= 1, spare >= 2);
        let chrome = u16::from(has_footer) + u16::from(has_bottom);
        let text_rows = (area.height - chips_height - menu_height - chrome).max(1);
        let text = Rect::new(area.x, area.y, area.width, text_rows);
        let menu = Rect::new(area.x, area.y + text_rows, area.width, menu_height);
        let chips = Rect::new(area.x, menu.y + menu_height, area.width, chips_height);
        let bottom = Rect::new(area.x, chips.y + chips_height, area.width, 1);
        let footer = Rect::new(area.x, bottom.y + u16::from(has_bottom), area.width, 1);
        ComposerLayout { text, menu, chips, bottom: has_bottom.then_some(bottom), footer: has_footer.then_some(footer) }
    }
}

/// What an empty composer says it is for.
fn composer_hint(v: &ThreadView) -> &'static str {
    if v.awaiting_answer() {
        t!("send a message to decline and redirect the agent · Tab to answer")
    } else if v.is_working() {
        t!("agent is working · Enter sends into its turn · ^S interrupts")
    } else if v.compose_only {
        t!("message the thread · @name for someone else")
    } else {
        t!("message the thread · @name for someone else · Esc dashboard")
    }
}

/// The composer's text in `area`, rows broken at line breaks and the width,
/// keeping the cursor row visible.
fn render_composer_text(frame: &mut Frame, v: &ThreadView, area: Rect, glyph: Span<'static>, focused: bool) {
    let indent = text_width(PROMPT);
    let body_width = (area.width as usize).saturating_sub(indent).max(1);
    let text_rows = area.height as usize;
    let chars: Vec<char> = v.composer.text().chars().collect();
    let styles = mention_styles(&chars, &v.mention_names());
    let (rows, (cursor_row, cursor_col)) = input_rows(&chars, body_width, v.composer.cursor());
    // The composer pane grows to fit its text: while it catches up with a
    // new line, keep the top in place rather than scrolling the first line
    // away and back. Scroll past the most rows it grows to, or in the full
    // view, whose layout has no pane to grow.
    let first_row = if v.compose_only && rows.len() <= MAX_INPUT_ROWS { 0 } else { cursor_row.saturating_sub(text_rows - 1) };
    for (r, &(start, end)) in rows.iter().enumerate().skip(first_row).take(text_rows) {
        let lead = if r == 0 { glyph.clone() } else { Span::raw(" ".repeat(indent)) };
        let mut spans = vec![lead];
        spans.extend(styled_run(&chars[start..end], &styles[start..end]));
        frame.render_widget(Paragraph::new(Line::from(spans)), Rect::new(area.x, area.y + cells(r - first_row), area.width, 1));
    }
    // The cursor's row may be the one the pane is still growing to show.
    if focused && cursor_row - first_row < text_rows {
        let cx = cells(indent + cursor_col);
        let cy = cells(cursor_row - first_row);
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    }
}

/// Footer: mode left; status (or as much key help as fits) right.  When the
/// right side would be cut, the "(shift+tab to cycle)" hint goes first.
fn render_composer_footer(frame: &mut Frame, v: &ThreadView, footer: Rect) {
    let width = footer.width as usize;
    let fit = |left: &[Span<'static>]| -> Line<'static> {
        let left_width: usize = left.iter().map(|s| s.width()).sum();
        let room = width.saturating_sub(left_width + 2);
        match &v.status {
            Some(st) => {
                let style = if v.status_is_error { theme::danger() } else { theme::accent() };
                Line::from(Span::styled(ellipsize(st, room), style))
            }
            // A newer Axel, in place of the key help: the command as a pill.
            None if let Some(version) = &v.update_hint => update_line(version, room),
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
    let full = match (&v.status, &v.update_hint) {
        (Some(st), _) => text_width(st),
        (None, Some(version)) => update_line(version, usize::MAX).width(),
        (None, None) => help_line(&v.help()).width(),
    };
    // Tight: drop the "(shift+tab to cycle)" hint and the host, keep who and the mode.
    let hint = cycle_hint();
    if right.width() < full
        && let Some(i) = left.iter().position(|s| s.content == hint.as_str())
    {
        left.truncate(i);
        right = fit(&left);
    }
    frame.render_widget(Paragraph::new(Line::from(left)), footer);
    let right_width = (cells(right.width()) + 1).min(footer.width);
    frame.render_widget(Paragraph::new(right), Rect::new(footer.x + footer.width - right_width, footer.y, right_width.saturating_sub(1), 1));
}
