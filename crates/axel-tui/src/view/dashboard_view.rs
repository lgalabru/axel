//! Dashboard rendering: bashtop-style boxed widgets, chosen by area.
//!
//! - **Full** (`>= FULL_MIN_WIDTH` x `>= FULL_MIN_HEIGHT`): 2x2 grid — threads | agents
//!   over tokens | inbox — plus a help footer.
//! - **Medium** (anything between): stacked threads + agents, help footer.
//! - **Compact** (`< COMPACT_MAX_WIDTH` or `< COMPACT_MAX_HEIGHT`, e.g. a 300x300 px
//!   floating pane): one box titled `agents · <tokens> · $cost [· ? n]`, one row per
//!   agent (glyph, name, sparkline, tokens), a one-line dim footer when there is room.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Sparkline};

use crate::dashboard::{Dashboard, Widget};
use crate::model::{DashAgentRow, RequestKind, ThreadRow, state_glyph};
use crate::theme;
use crate::util::{ellipsize, format_tokens, relative_time};

const SELECTED_BG: Color = Color::Indexed(236);
const SPARK_W: u16 = 10;

/// Below this width the dashboard is compact.
pub const COMPACT_MAX_WIDTH: u16 = 70;
/// Below this height the dashboard is compact.
pub const COMPACT_MAX_HEIGHT: u16 = 16;
/// The 2x2 grid needs at least this width…
pub const FULL_MIN_WIDTH: u16 = 100;
/// …and this height.
pub const FULL_MIN_HEIGHT: u16 = 25;
/// Compact mode drops its footer below this height.
pub const COMPACT_FOOTER_MIN_HEIGHT: u16 = 8;
/// Smallest sparkline worth drawing in compact rows.
const COMPACT_SPARK_MIN: u16 = 6;

/// Which layout an area gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Compact,
    Medium,
    Full,
}

pub fn mode_for(area: Rect) -> Mode {
    if area.width < COMPACT_MAX_WIDTH || area.height < COMPACT_MAX_HEIGHT {
        Mode::Compact
    } else if area.width >= FULL_MIN_WIDTH && area.height >= FULL_MIN_HEIGHT {
        Mode::Full
    } else {
        Mode::Medium
    }
}

pub fn render(frame: &mut Frame, d: &Dashboard, now: i64) {
    let area = frame.area();
    match mode_for(area) {
        Mode::Compact => render_compact(frame, d, area),
        Mode::Medium => {
            let [body, footer] = Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(area);
            let [threads, agents] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
            render_threads(frame, d, threads, now);
            render_agents(frame, d, agents, now);
            render_footer(frame, d, footer);
        }
        Mode::Full => {
            let [body, footer] = Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(area);
            let [top, bottom] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
            let [threads, agents] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(top);
            let [tokens, inbox] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);
            render_threads(frame, d, threads, now);
            render_agents(frame, d, agents, now);
            render_tokens(frame, d, tokens);
            render_inbox(frame, d, inbox, now);
            render_footer(frame, d, footer);
        }
    }
}

fn render_footer(frame: &mut Frame, d: &Dashboard, footer: Rect) {
    let line = match &d.flash {
        Some((msg, _)) => Line::from(Span::styled(format!(" {msg}"), theme::accent())),
        None => footer_help(&d.help()),
    };
    frame.render_widget(Paragraph::new(line), footer);
}

// ---------------------------------------------------------------- compact

/// Column plan for a compact agent row.
struct CompactCols {
    name: usize,
    spark: u16,
    tokens: Option<usize>,
}

fn plan_compact_cols(width: usize) -> CompactCols {
    const GLYPH: usize = 2;
    const TOKENS: usize = 5;
    let mut c = CompactCols { name: 4, spark: 0, tokens: None };
    let mut used = GLYPH + c.name;
    if used + 1 + TOKENS <= width {
        c.tokens = Some(TOKENS);
        used += 1 + TOKENS;
    }
    // Grow the name a little before spending on a sparkline.
    let grow = width.saturating_sub(used).min(4);
    c.name += grow;
    used += grow;
    let spare = width.saturating_sub(used);
    if spare > COMPACT_SPARK_MIN as usize {
        c.spark = ((spare - 1) as u16).min(SPARK_W);
        used += 1 + c.spark as usize;
    }
    c.name += width.saturating_sub(used).min(10);
    c
}

fn render_compact(frame: &mut Frame, d: &Dashboard, area: Rect) {
    let has_footer = area.height >= COMPACT_FOOTER_MIN_HEIGHT;
    let [boxed, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(u16::from(has_footer))]).areas(area);

    let t = &d.data.tokens;
    let mut title = format!("agents · {} · ${:.2}", format_tokens(t.input + t.output), t.cost_usd);
    if !d.data.requests.is_empty() {
        title.push_str(&format!(" · ? {}", d.data.requests.len()));
    }
    let title = ellipsize(&title, boxed.width.saturating_sub(4) as usize);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim())
        .title(Line::from(vec![Span::raw(" "), Span::styled(title, theme::accent_bold()), Span::raw(" ")]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(boxed);
    frame.render_widget(block, boxed);

    if inner.height > 0 && inner.width > 0 {
        if d.data.agents.is_empty() {
            frame.render_widget(Paragraph::new(Line::from(Span::styled("no agents · n to create", theme::dim()))), inner);
        } else {
            let rows = inner.height as usize;
            let total = d.data.agents.len();
            // Agents are already ordered most urgent / most recent first by the glue.
            let shown = if total > rows { rows.saturating_sub(1) } else { total };
            // Keep the selection visible by scrolling the shown window.
            let first = d.agent_sel.saturating_sub(shown.saturating_sub(1)).min(total.saturating_sub(shown));
            let cols = plan_compact_cols(inner.width as usize);
            for (row_i, (i, a)) in d.data.agents.iter().enumerate().skip(first).take(shown).enumerate() {
                let row = Rect::new(inner.x, inner.y + row_i as u16, inner.width, 1);
                render_compact_row(frame, a, row, &cols, i == d.agent_sel);
            }
            if total > shown {
                let hidden = total - shown;
                let row = Rect::new(inner.x, inner.y + shown as u16, inner.width, 1);
                frame.render_widget(Paragraph::new(Line::from(Span::styled(format!("+{hidden} more"), theme::dim()))), row);
            }
        }
    }

    if has_footer {
        let line = match &d.flash {
            Some((msg, _)) => Line::from(Span::styled(ellipsize(msg, footer.width as usize), theme::accent())),
            None => Line::from(Span::styled("n new · i inbox · q", theme::dim())),
        };
        frame.render_widget(Paragraph::new(line), footer);
    }
}

fn render_compact_row(frame: &mut Frame, a: &DashAgentRow, row: Rect, cols: &CompactCols, selected: bool) {
    let (base, _dim) = row_styles(selected);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{} ", state_glyph(a.state)), base.fg(theme::state_color(a.state))),
        Span::styled(format!("{:<w$}", ellipsize(&a.name, cols.name), w = cols.name), base.fg(theme::agent_color(a.color)).bold()),
    ];
    let spark_x = if cols.spark > 0 {
        let x = row.x + span_len(&spans) as u16 + 1;
        spans.push(Span::styled(" ".repeat(cols.spark as usize + 1), base));
        Some(x)
    } else {
        None
    };
    if let Some(w) = cols.tokens {
        spans.push(Span::styled(format!(" {:>w$}", format_tokens(a.tokens), w = w), base));
    }
    let used = span_len(&spans);
    if used < row.width as usize {
        spans.push(Span::styled(" ".repeat(row.width as usize - used), base));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), row);
    if let Some(x) = spark_x {
        // Take the tail of the series so the newest buckets are shown.
        frame.render_widget(Paragraph::new(Line::from(chart(&a.spark, cols.spark as usize, base))), Rect::new(x, row.y, cols.spark, 1));
    }
}

fn footer_help(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (k, v) in items {
        spans.push(Span::styled(format!(" {k} "), theme::accent_bold()));
        spans.push(Span::styled(format!("{v}  "), theme::dim()));
    }
    Line::from(spans)
}

fn panel<'a>(title: &'a str, right: Option<String>, focused: bool) -> Block<'a> {
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { theme::accent() } else { theme::dim() })
        .title(Line::from(vec![Span::raw(" "), Span::styled(title, theme::accent_bold()), Span::raw(" ")]))
        .padding(Padding::horizontal(1));
    if let Some(r) = right {
        b = b.title_top(Line::from(Span::styled(format!(" {r} "), theme::dim())).right_aligned());
    }
    b
}

fn row_styles(selected: bool) -> (Style, Style) {
    if selected {
        (Style::default().bg(SELECTED_BG), theme::dim().bg(SELECTED_BG))
    } else {
        (Style::default(), theme::dim())
    }
}

fn span_len(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Render a row's spans, padded to the row width, and an optional sparkline
/// at `spark_x` (the caller leaves a blank gap for it).
fn finish_row(frame: &mut Frame, row: Rect, mut spans: Vec<Span<'static>>, base: Style, spark: Option<(u16, &[u64])>) {
    let used = span_len(&spans);
    if used < row.width as usize {
        spans.push(Span::styled(" ".repeat(row.width as usize - used), base));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), row);
    if let Some((x, data)) = spark
        && x + SPARK_W <= row.x + row.width
    {
        frame.render_widget(Paragraph::new(Line::from(chart(data, SPARK_W as usize, base))), Rect::new(x, row.y, SPARK_W, 1));
    }
}

/// A row's chart: `series` in braille across `width` cells, on `base`'s background.
fn chart(series: &[u64], width: usize, base: Style) -> Vec<Span<'static>> {
    let series: Vec<i64> = series.iter().map(|&v| v.min(i64::MAX as u64) as i64).collect();
    // The quiet floor comes dim with no background: give it the row's.
    let bg = Style { bg: base.bg, ..Style::default() };
    super::chart::braille(&series, width, 0, base.fg(theme::ACCENT)).into_iter().map(|s| s.patch_style(bg)).collect()
}

// ---------------------------------------------------------------- threads

pub(crate) struct ThreadCols {
    title: usize,
    agents: Option<usize>,
    projects: Option<usize>,
    env: Option<usize>,
    sessions: bool,
    spark: bool,
    tokens: usize,
    age: usize,
}

pub(crate) fn plan_thread_cols(width: usize) -> ThreadCols {
    let mut c = ThreadCols { title: 14, agents: None, projects: None, env: None, sessions: false, spark: false, tokens: 6, age: 7 };
    let mut used = 2 + c.title + 1 + c.tokens + 1 + c.age;
    let try_add = |cost: usize, used: &mut usize| -> bool {
        if *used + cost <= width {
            *used += cost;
            true
        } else {
            false
        }
    };
    if try_add(1 + 12, &mut used) {
        c.agents = Some(12);
    }
    if try_add(1 + 12, &mut used) {
        c.projects = Some(12);
    }
    if try_add(1 + SPARK_W as usize, &mut used) {
        c.spark = true;
    }
    if try_add(1 + 3, &mut used) {
        c.sessions = true;
    }
    if try_add(1 + 8, &mut used) {
        c.env = Some(8);
    }
    let mut spare = width.saturating_sub(used);
    let grow = spare.min(16);
    c.title += grow;
    spare -= grow;
    if let Some(a) = c.agents.as_mut() {
        let g = spare.min(8);
        *a += g;
        spare -= g;
    }
    if let Some(p) = c.projects.as_mut() {
        *p += spare.min(8);
    }
    c
}

fn render_threads(frame: &mut Frame, d: &Dashboard, area: Rect, now: i64) {
    let focused = d.focus == Widget::Threads;
    let block = panel("threads", Some(d.data.threads.len().to_string()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    if d.data.threads.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("No open threads — press n to start one.", theme::dim()))),
            inner,
        );
        return;
    }
    let cols = plan_thread_cols(inner.width as usize);
    let rows = inner.height as usize;
    let first = d.thread_sel.saturating_sub(rows.saturating_sub(1));
    for (i, t) in d.data.threads.iter().enumerate().skip(first).take(rows) {
        let row = Rect::new(inner.x, inner.y + (i - first) as u16, inner.width, 1);
        render_thread_row(frame, t, row, &cols, i == d.thread_sel && focused, now);
    }
}

pub(crate) fn render_thread_row(frame: &mut Frame, t: &ThreadRow, row: Rect, cols: &ThreadCols, selected: bool, now: i64) {
    let (base, dim) = row_styles(selected);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(format!("{} ", state_glyph(t.state)), base.fg(theme::state_color(t.state))));
    spans.push(Span::styled(format!("{:<w$}", ellipsize(&t.title, cols.title), w = cols.title), base.bold()));
    if let Some(w) = cols.agents {
        // Agents in their own colours, truncated as a whole.
        let mut used = 1usize;
        spans.push(Span::styled(" ", base));
        if t.agents.is_empty() {
            spans.push(Span::styled(format!("{:<w$}", "—", w = w), dim));
        } else {
            for (i, (name, color)) in t.agents.iter().enumerate() {
                let sep = if i > 0 { " " } else { "" };
                let piece = format!("{sep}{name}");
                if used + piece.chars().count() > w + 1 {
                    if used < w + 1 {
                        spans.push(Span::styled("…".to_string(), dim));
                        used += 1;
                    }
                    break;
                }
                used += piece.chars().count();
                spans.push(Span::styled(piece, base.fg(theme::agent_color(*color))));
            }
            if used < w + 1 {
                spans.push(Span::styled(" ".repeat(w + 1 - used), base));
            }
        }
    }
    let spark_x = if cols.spark {
        let x = row.x + span_len(&spans) as u16 + 1;
        spans.push(Span::styled(" ".repeat(SPARK_W as usize + 1), base));
        Some(x)
    } else {
        None
    };
    if let Some(w) = cols.projects {
        let p = if t.projects.is_empty() { "scratch".to_string() } else { t.projects.join(", ") };
        let style = if t.projects.is_empty() { dim } else { base };
        spans.push(Span::styled(format!(" {:<w$}", ellipsize(&p, w), w = w), style));
    }
    if cols.sessions {
        spans.push(Span::styled(format!(" {:>3}", t.sessions_total), dim));
    }
    if let Some(w) = cols.env {
        let e = if t.environments.is_empty() { "—".to_string() } else { t.environments.join(",") };
        spans.push(Span::styled(format!(" {:<w$}", ellipsize(&e, w), w = w), dim));
    }
    spans.push(Span::styled(format!(" {:>w$}", format_tokens(t.tokens), w = cols.tokens), base));
    spans.push(Span::styled(format!(" {:>w$}", ellipsize(&relative_time(t.updated_at, now), cols.age), w = cols.age), dim));
    finish_row(frame, row, spans, base, spark_x.map(|x| (x, t.spark.as_slice())));
}

// ---------------------------------------------------------------- agents

struct AgentCols {
    name: usize,
    harness: Option<usize>,
    thread: Option<usize>,
    env: Option<usize>,
    spark: bool,
    tokens: usize,
}

fn plan_agent_cols(width: usize) -> AgentCols {
    let mut c = AgentCols { name: 8, harness: None, thread: None, env: None, spark: false, tokens: 6 };
    let mut used = 2 + c.name + 1 + c.tokens;
    if used + 1 + 6 <= width {
        c.harness = Some(6);
        used += 7;
    }
    if used + 1 + 10 <= width {
        c.thread = Some(10);
        used += 11;
    }
    if used + 1 + SPARK_W as usize <= width {
        c.spark = true;
        used += 1 + SPARK_W as usize;
    }
    if used + 1 + 7 <= width {
        c.env = Some(7);
        used += 8;
    }
    let mut spare = width.saturating_sub(used);
    let g = spare.min(4);
    c.name += g;
    spare -= g;
    if let Some(t) = c.thread.as_mut() {
        *t += spare;
    }
    c
}

fn render_agents(frame: &mut Frame, d: &Dashboard, area: Rect, now: i64) {
    let focused = d.focus == Widget::Agents;
    let block = panel("agents", Some(d.data.agents.len().to_string()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    if d.data.agents.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("No agents yet — press n to create one.", theme::dim()))),
            inner,
        );
        return;
    }
    let cols = plan_agent_cols(inner.width as usize);
    let rows = inner.height as usize;
    let first = d.agent_sel.saturating_sub(rows.saturating_sub(1));
    for (i, a) in d.data.agents.iter().enumerate().skip(first).take(rows) {
        let row = Rect::new(inner.x, inner.y + (i - first) as u16, inner.width, 1);
        render_agent_row(frame, a, row, &cols, i == d.agent_sel && focused, now);
    }
}

fn render_agent_row(frame: &mut Frame, a: &DashAgentRow, row: Rect, cols: &AgentCols, selected: bool, _now: i64) {
    let (base, dim) = row_styles(selected);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(format!("{} ", state_glyph(a.state)), base.fg(theme::state_color(a.state))));
    spans.push(Span::styled(
        format!("{:<w$}", ellipsize(&a.name, cols.name), w = cols.name),
        base.fg(theme::agent_color(a.color)).bold(),
    ));
    if let Some(w) = cols.harness {
        spans.push(Span::styled(format!(" {:<w$}", ellipsize(a.harness.as_str(), w), w = w), dim));
    }
    if let Some(w) = cols.thread {
        let t = a.thread_title.clone().unwrap_or_else(|| "—".into());
        let style = if a.thread_title.is_some() { base } else { dim };
        spans.push(Span::styled(format!(" {:<w$}", ellipsize(&t, w), w = w), style));
    }
    let spark_x = if cols.spark {
        let x = row.x + span_len(&spans) as u16 + 1;
        spans.push(Span::styled(" ".repeat(SPARK_W as usize + 1), base));
        Some(x)
    } else {
        None
    };
    if let Some(w) = cols.env {
        spans.push(Span::styled(format!(" {:<w$}", ellipsize(&a.environment, w), w = w), dim));
    }
    spans.push(Span::styled(format!(" {:>w$}", format_tokens(a.tokens), w = cols.tokens), base));
    finish_row(frame, row, spans, base, spark_x.map(|x| (x, a.spark.as_slice())));
}

// ---------------------------------------------------------------- tokens

fn render_tokens(frame: &mut Frame, d: &Dashboard, area: Rect) {
    let block = panel("tokens (1h)", None, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let t = &d.data.tokens;
    let [spark, totals] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    if spark.height > 0 {
        let w = spark.width as usize;
        let data: Vec<u64> = if t.series.len() > w { t.series[t.series.len() - w..].to_vec() } else { t.series.clone() };
        let color = if data.iter().all(|v| *v == 0) { theme::DIM } else { theme::ACCENT };
        frame.render_widget(Sparkline::default().data(&data).style(Style::default().fg(color)), spark);
    }
    let line = Line::from(vec![
        Span::styled("in ", theme::dim()),
        Span::styled(format_tokens(t.input), theme::bold()),
        Span::styled("  out ", theme::dim()),
        Span::styled(format_tokens(t.output), theme::bold()),
        Span::styled("  cache ", theme::dim()),
        Span::styled(format_tokens(t.cache_read + t.cache_write), theme::bold()),
        Span::styled(format!("  ${:.2} est", t.cost_usd), theme::accent()),
    ]);
    frame.render_widget(Paragraph::new(line), totals);
}

// ---------------------------------------------------------------- inbox

fn render_inbox(frame: &mut Frame, d: &Dashboard, area: Rect, now: i64) {
    let right = if d.data.unread > 0 { Some(format!("{} unread", d.data.unread)) } else { None };
    let block = panel("inbox", right, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for r in &d.data.requests {
        let (glyph, style) = match r.kind {
            RequestKind::Permission => ("!", theme::danger().bold()),
            RequestKind::Question => ("?", Style::default().fg(Color::Yellow).bold()),
        };
        let age = relative_time(r.created_at, now);
        let head = format!("{}: ", r.agent_name);
        let room = width.saturating_sub(2 + head.chars().count() + age.len() + 1);
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), style),
            Span::styled(head, theme::bold()),
            Span::raw(format!("{:<room$}", ellipsize(&r.content.replace('\n', " "), room))),
            Span::styled(format!(" {age}"), theme::dim()),
        ]));
    }
    for m in &d.data.messages {
        if lines.len() >= inner.height as usize {
            break;
        }
        let glyph = match m.kind_label.as_str() {
            "handoff" => "→",
            "answer" | "permission_reply" => "↩",
            _ => "·",
        };
        let age = relative_time(m.created_at as i64, now);
        let head = format!("{}: ", m.from_name);
        let room = width.saturating_sub(2 + head.chars().count() + age.len() + 1);
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), theme::dim()),
            Span::styled(head, theme::bold()),
            Span::raw(format!("{:<room$}", ellipsize(&m.content.replace('\n', " "), room))),
            Span::styled(format!(" {age}"), theme::dim()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("Inbox zero.", theme::dim())));
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}
