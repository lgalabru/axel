//! The token inspector: every request the agents sent to their model, what
//! it cost, and what its context was made of. After the Payment Debugger's
//! web UI: a toolbar, a list of requests, an inspector for the selected one,
//! and a sidebar of totals.
//!
//! The toolbar (scope, search, count, totals, "live") is the top row and the
//! key help the bottom one. Between them, as the pane widens:
//! - under [`SPLIT_MIN_WIDTH`] columns, the list on top and the inspector
//!   under it;
//! - from it, the list on the left and the inspector on the right;
//! - from [`SIDEBAR_MIN_WIDTH`], a sidebar at the right too: totals by agent
//!   and by model, the cache-hit rate, and anomalies.
//!
//! The list's columns join by need, as the usage pane's do. The inspector
//! labels each inference detail on a row of its own when it has
//! [`TALL_INSPECTOR_MIN_ROWS`], else packs them in two lines; the context
//! breakdown follows either way. Under [`MIN_WIDTH`] × [`MIN_HEIGHT`] it's
//! one line of totals.

use axel_i18n::t;
use chrono::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::inspect::{
    Anomaly, BarRow, ContextKind, Entry, Focus, Inspector, ListView, ModelProvider, ModelRequest, RequestOutcome, Scope, Totals,
    bar_rows, estimate, is_rebuild, largest, share_pct,
};
use crate::theme;
use crate::util::{ellipsize, format_tokens, pad_end, pad_start, text_width};

/// Narrower than this, or shorter than [`MIN_HEIGHT`], the pane is one line.
pub const MIN_WIDTH: u16 = 30;
pub const MIN_HEIGHT: u16 = 6;
/// From this width the inspector sits beside the list instead of under it.
pub const SPLIT_MIN_WIDTH: u16 = 110;
/// From this width the sidebar joins.
pub const SIDEBAR_MIN_WIDTH: u16 = 160;
/// The sidebar's width: an agent's name and its three numbers.
const SIDEBAR_WIDTH: u16 = 36;
/// The list's share of what the sidebar leaves, side by side.
const LIST_SHARE_PCT: u16 = 55;
/// The list's most share of the body, stacked: the inspector needs the rest.
const LIST_STACKED_SHARE_PCT: u16 = 45;
/// From this many rows the inspector labels each detail on its own row.
pub const TALL_INSPECTOR_MIN_ROWS: u16 = 22;
/// Widest an agent's name gets in the list.
const AGENT_MAX: usize = 10;
/// The model badge's width, its padding included: `gpt-5.2-codex` whole.
const MODEL_WIDTH: usize = 15;
/// The endpoint column's narrowest and widest.
const ENDPOINT_MIN: usize = 10;
const ENDPOINT_MAX: usize = 28;
/// The widest a breakdown bar's name gets.
const BAR_NAME_MAX: usize = 26;
/// The narrowest bar worth drawing.
const BAR_MIN: usize = 4;
/// Widest an inference label gets.
const LABEL_MAX: usize = 14;
/// Most of a turn's requests charted under the breakdown: the latest ones,
/// which a 40-row pane shows whole.
const TURN_REQUESTS_SHOWN: usize = 16;

/// A column of the request list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Time,
    Agent,
    Model,
    Endpoint,
    Input,
    Cached,
    Output,
    Cost,
    Latency,
    Status,
}

/// The order columns join in as the list widens.
const BY_NEED: [Col; 10] = [Col::Status, Col::Model, Col::Cost, Col::Cached, Col::Agent, Col::Output, Col::Latency, Col::Input, Col::Time, Col::Endpoint];
/// The order they're shown in.
const SHOWN: [Col; 10] = [Col::Time, Col::Agent, Col::Model, Col::Endpoint, Col::Input, Col::Cached, Col::Output, Col::Cost, Col::Latency, Col::Status];

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Time => t!("time"),
            Col::Agent => t!("agent"),
            Col::Model => t!("model"),
            Col::Endpoint => t!("endpoint"),
            Col::Input => t!("in"),
            Col::Cached => t!("cached"),
            Col::Output => t!("out"),
            Col::Cost => "~$",
            Col::Latency => t!("took"),
            Col::Status => "",
        }
    }

    /// Its width; the endpoint's is its narrowest.
    fn width(self, agent: usize) -> usize {
        match self {
            Col::Time => 8,
            Col::Agent => agent,
            Col::Model => MODEL_WIDTH,
            Col::Endpoint => ENDPOINT_MIN,
            Col::Input | Col::Output => 5,
            Col::Cached | Col::Cost | Col::Latency => 6,
            Col::Status => 1,
        }
    }

    /// Numbers line up on the right.
    fn right(self) -> bool {
        matches!(self, Col::Input | Col::Cached | Col::Output | Col::Cost | Col::Latency)
    }
}

/// The columns that fit in `width` (after the selection gutter), in the
/// order shown, and the endpoint's width (what's left, up to its widest).
fn columns(width: usize, agent: usize) -> (Vec<Col>, usize) {
    let mut used = 1;
    let mut chosen = Vec::new();
    for c in BY_NEED {
        if used + 1 + c.width(agent) <= width {
            used += 1 + c.width(agent);
            chosen.push(c);
        }
    }
    let endpoint = if chosen.contains(&Col::Endpoint) { (width - used + ENDPOINT_MIN).min(ENDPOINT_MAX) } else { 0 };
    (SHOWN.into_iter().filter(|c| chosen.contains(c)).collect(), endpoint)
}

pub fn render(frame: &mut Frame, ins: &Inspector, area: Rect) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let view = ins.list();
        put(frame, vec![summary(ins, &view.totals, usize::from(area.width))], Rect { height: area.height.min(1), ..area });
        return;
    }
    let view = ins.list();
    let toolbar_area = Rect { height: 1, ..area };
    let help_area = Rect { y: area.y + area.height - 1, height: 1, ..area };
    let body = Rect { y: area.y + 1, height: area.height - 2, ..area };
    let sidebar_shown = body.width >= SIDEBAR_MIN_WIDTH;
    put(frame, vec![toolbar(ins, &view, usize::from(area.width), sidebar_shown)], toolbar_area);
    put(frame, vec![super::help_line_fit(&ins.help(), usize::from(area.width))], help_area);
    if ins.data.requests.is_empty() {
        empty_state(frame, ins, body);
    } else if body.width >= SPLIT_MIN_WIDTH {
        side_by_side(frame, ins, &view, body, sidebar_shown);
    } else {
        stacked(frame, ins, &view, body);
    }
    if ins.help_open {
        help_overlay(frame, area);
    }
}

/// The list left, the inspector right, the sidebar at the far right when shown.
fn side_by_side(frame: &mut Frame, ins: &Inspector, view: &ListView, body: Rect, sidebar_shown: bool) {
    let mut main = body;
    if sidebar_shown {
        main.width = body.width - SIDEBAR_WIDTH - 1;
        let side = Rect { x: main.x + main.width + 2, width: SIDEBAR_WIDTH - 1, ..body };
        vertical_rule(frame, Rect { x: main.x + main.width, width: 1, ..body });
        put(frame, sidebar_lines(ins, view, usize::from(side.width), usize::from(side.height)), side);
    }
    let list_width = main.width * LIST_SHARE_PCT / 100;
    let list = Rect { width: list_width, ..main };
    let inspector = Rect { x: main.x + list_width + 2, width: main.width - list_width - 2, ..main };
    vertical_rule(frame, Rect { x: main.x + list_width, width: 1, ..main });
    put(frame, list_lines(ins, view, usize::from(list.width), usize::from(list.height)), list);
    put(frame, inspector_lines(ins, usize::from(inspector.width), inspector.height), inspector);
}

/// The list on top, as tall as it needs up to its share; the inspector under it.
fn stacked(frame: &mut Frame, ins: &Inspector, view: &ListView, body: Rect) {
    let want = u16::try_from(view.entries.len() + 1).unwrap_or(u16::MAX);
    let list_height = want.clamp(3, (body.height * LIST_STACKED_SHARE_PCT / 100).max(3)).min(body.height);
    let list = Rect { height: list_height, ..body };
    put(frame, list_lines(ins, view, usize::from(list.width), usize::from(list.height)), list);
    if body.height <= list_height + 1 {
        return;
    }
    let rule = Rect { y: list.y + list_height, height: 1, ..body };
    put(frame, vec![Line::styled("─".repeat(usize::from(rule.width)), theme::dim())], rule);
    let inspector = Rect { y: rule.y + 1, height: body.height - list_height - 1, ..body };
    put(frame, inspector_lines(ins, usize::from(inspector.width), inspector.height), inspector);
}

fn vertical_rule(frame: &mut Frame, area: Rect) {
    let lines = (0..area.height).map(|_| Line::styled("│", theme::dim())).collect();
    put(frame, lines, area);
}

/// Draw `lines` in `area`, which each must fit: every builder here measures
/// what it writes, so a line too wide is a bug, not something to clip.
fn put(frame: &mut Frame, lines: Vec<Line<'static>>, area: Rect) {
    for line in &lines {
        debug_assert!(line.width() <= usize::from(area.width), "a line of {} cells in {}: {line:?}", line.width(), area.width);
    }
    debug_assert!(lines.len() <= usize::from(area.height) || area.height == 0, "{} lines in {} rows", lines.len(), area.height);
    frame.render_widget(Paragraph::new(lines), area);
}

/// `spans` cut to `width` cells, the span at the edge ending in `…`.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let cut = ellipsize(&span.content, width - used);
        used += text_width(&cut);
        out.push(Span::styled(cut, span.style));
        break;
    }
    debug_assert!(used <= width, "fitted spans fit");
    Line::from(out)
}

/// One line for a sliver of a pane: the count, the cost, and whether it's live.
fn summary(ins: &Inspector, totals: &Totals, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(t!("{n} requests", n = totals.requests), theme::bold()),
        Span::styled(" · ", theme::dim()),
        Span::styled(cost(totals.cost_usd), theme::dim()),
    ];
    if ins.is_live() {
        spans.push(Span::styled(format!(" · ● {}", t!("live")), theme::accent()));
    }
    fit(spans, width)
}

/// The scope in words.
fn scope_label(ins: &Inspector) -> String {
    match &ins.scope {
        Scope::Thread(id) => t!("thread {title}", title = ins.data.threads.get(id).cloned().unwrap_or_else(|| id.chars().take(8).collect())),
        Scope::Agent(pk) => t!("agent {name}", name = ins.agent(pk).0),
        Scope::All => t!("all threads").to_string(),
    }
}

/// Scope, search, count and totals; "live" at the right while a request
/// runs. What doesn't fit goes, least needed first.
fn toolbar(ins: &Inspector, view: &ListView, width: usize, sidebar_shown: bool) -> Line<'static> {
    let sep = || Span::styled(" · ", theme::dim());
    let totals = &view.totals;
    // (need: lower stays longer, spans) in the order shown.
    let mut parts: Vec<(u8, Vec<Span<'static>>)> = vec![
        (0, vec![Span::styled("Axel", theme::accent_bold()), Span::styled(format!(" {}", t!("token inspector")), theme::bold())]),
        (1, vec![sep(), Span::styled(ellipsize(&scope_label(ins), 32), Style::default()), Span::styled(" · 24h", theme::dim())]),
    ];
    if ins.searching || !ins.search.is_empty() {
        let cursor = if ins.searching { "▏" } else { "" };
        parts.push((1, vec![sep(), Span::styled("/", theme::accent()), Span::raw(format!("{}{cursor}", ellipsize(&ins.search, 20)))]));
    }
    parts.push((2, vec![sep(), Span::styled(t!("{n} requests", n = totals.requests), theme::bold())]));
    parts.push((4, vec![sep(), Span::raw(format!("{} ", format_tokens(tokens_i64(totals.context_tokens)))), Span::styled(t!("context"), theme::dim())]));
    if let Some(pct) = totals.cache_hit_pct() {
        parts.push((5, vec![sep(), Span::styled(format!("{pct}% "), hit_style(pct)), Span::styled(t!("cached"), theme::dim())]));
    }
    parts.push((3, vec![sep(), Span::styled(cost(totals.cost_usd), theme::bold())]));
    let flagged = crate::inspect::anomalies(&ins.data.requests, &view.groups).len();
    if !sidebar_shown && flagged > 0 {
        parts.push((6, vec![sep(), Span::styled(t!("{n} flagged", n = flagged), theme::danger())]));
    }
    // At the right: "live" while a request runs, else that recording is off.
    let live = if ins.is_live() {
        Some(Span::styled(format!("● {}", t!("live")), theme::accent()))
    } else {
        (!ins.is_recording()).then(|| Span::styled(format!("○ {}", t!("recording off")), theme::dim()))
    };
    let room = width.saturating_sub(live.as_ref().map_or(0, |l| l.width() + 1));
    // Drop the least needed until the rest fits; bounded by the parts.
    for _ in 0..parts.len() {
        let used: usize = parts.iter().flat_map(|(_, s)| s).map(Span::width).sum();
        if used <= room {
            break;
        }
        let Some(worst) = parts.iter().enumerate().max_by_key(|(_, (need, _))| *need).map(|(i, _)| i) else { break };
        if parts[worst].0 == 0 {
            break;
        }
        parts.remove(worst);
    }
    let mut spans: Vec<Span<'static>> = parts.into_iter().flat_map(|(_, s)| s).collect();
    if let Some(live) = live {
        let used: usize = spans.iter().map(Span::width).sum();
        if used + live.width() < width {
            spans.push(Span::raw(" ".repeat(width - used - live.width())));
            spans.push(live);
        }
    }
    fit(spans, width)
}

/// The column labels, then the requests (and turn headings) around the
/// selection, as many as `rows` holds.
fn list_lines(ins: &Inspector, view: &ListView, width: usize, rows: usize) -> Vec<Line<'static>> {
    if rows == 0 {
        return Vec::new();
    }
    if view.entries.is_empty() {
        return vec![fit(vec![Span::styled(t!("no requests match “{query}”", query = ins.search), theme::dim())], width)];
    }
    let longest = view.shown.iter().map(|&i| text_width(&ins.agent(&ins.data.requests[i].agent_pubkey).0)).max().unwrap_or(0);
    let agent_width = longest.clamp(3, AGENT_MAX);
    let (cols, endpoint_width) = columns(width, agent_width);
    let mut out = vec![label_line(&cols, agent_width, endpoint_width, width)];
    let room = rows - 1;
    let selected = ins.selected_entry(view);
    // Keep the selection in sight: scrolled just far enough.
    let first = selected.map_or(0, |s| (s + 1).saturating_sub(room));
    for (at, entry) in view.entries.iter().enumerate().skip(first).take(room) {
        let is_selected = selected == Some(at);
        out.push(match *entry {
            Entry::Header { group, folded } => header_line(ins, view, group, folded, is_selected, width),
            Entry::Request { index } => fit(request_line(ins, &ins.data.requests[index], &cols, agent_width, endpoint_width, is_selected).spans, width),
        });
    }
    out
}

fn label_line(cols: &[Col], agent_width: usize, endpoint_width: usize, width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for &c in cols {
        let w = if c == Col::Endpoint { endpoint_width } else { c.width(agent_width) };
        let label = ellipsize(c.label(), w);
        let text = if c.right() { pad_start(&label, w) } else { pad_end(&label, w) };
        spans.push(Span::styled(format!(" {text}"), theme::dim()));
    }
    fit(spans, width)
}

/// `▾ terra · turn 7 · 12 requests · 380k context · ~$0.41 · 94% cached`.
fn header_line(ins: &Inspector, view: &ListView, group: usize, folded: bool, is_selected: bool, width: usize) -> Line<'static> {
    let g = &view.groups[group];
    let (name, color) = ins.agent(&g.agent_pubkey);
    let sep = || Span::styled(" · ", theme::dim());
    let turn = g.key.turn.map_or_else(|| t!("no turn").to_string(), |n| t!("turn {n}", n = n));
    let mut spans = vec![
        Span::styled(if is_selected { "›" } else { " " }, theme::accent_bold()),
        Span::styled(if folded { "▸ " } else { "▾ " }, theme::dim()),
        Span::styled(name, Style::default().fg(theme::agent_color(color))),
        sep(),
        Span::styled(turn, theme::bold()),
        sep(),
        Span::styled(t!("{n} requests", n = g.totals.requests), theme::dim()),
        sep(),
        Span::raw(format!("{} ", format_tokens(tokens_i64(g.totals.context_tokens)))),
        Span::styled(t!("context"), theme::dim()),
        // The cost before the cache share: it's what a narrow list keeps.
        sep(),
        Span::styled(cost(g.totals.cost_usd), Style::default()),
    ];
    if let Some(pct) = g.totals.cache_hit_pct() {
        spans.extend([sep(), Span::styled(format!("{pct}% "), hit_style(pct)), Span::styled(t!("cached"), theme::dim())]);
    }
    if g.totals.errors > 0 {
        spans.extend([sep(), Span::styled(t!("{n} failed", n = g.totals.errors), theme::danger())]);
    }
    let mut line = fit(spans, width);
    if is_selected {
        line.spans.iter_mut().skip(1).for_each(|s| s.style = s.style.add_modifier(Modifier::BOLD));
    }
    line
}

fn request_line(ins: &Inspector, r: &ModelRequest, cols: &[Col], agent_width: usize, endpoint_width: usize, is_selected: bool) -> Line<'static> {
    let gutter = match (is_selected, ins.focus) {
        (true, Focus::List) => Span::styled("›", theme::accent_bold()),
        (true, Focus::Inspector) => Span::styled("›", theme::dim()),
        (false, _) => Span::raw(" "),
    };
    let mut spans = vec![gutter];
    for &c in cols {
        spans.push(Span::raw(" "));
        spans.extend(cell(ins, r, c, agent_width, endpoint_width));
    }
    let mut line = Line::from(spans);
    if is_selected {
        line.spans.iter_mut().skip(1).for_each(|s| s.style = s.style.add_modifier(Modifier::BOLD));
    }
    line
}

/// One column of a request's row, exactly its width.
fn cell(ins: &Inspector, r: &ModelRequest, c: Col, agent_width: usize, endpoint_width: usize) -> Vec<Span<'static>> {
    let number = |n: Option<u64>, w: usize| match n {
        Some(n) => Span::raw(pad_start(&format_tokens(tokens_i64(n)), w)),
        None => Span::styled(pad_start("·", w), theme::dim()),
    };
    match c {
        Col::Time => vec![Span::styled(clock(r.started_at_ms), theme::dim())],
        Col::Agent => {
            let (name, color) = ins.agent(&r.agent_pubkey);
            vec![Span::styled(pad_end(&ellipsize(&name, agent_width), agent_width), Style::default().fg(theme::agent_color(color)))]
        }
        Col::Model => badge(r, Some(MODEL_WIDTH)),
        Col::Endpoint => vec![Span::styled(pad_end(&ellipsize(&r.endpoint, endpoint_width), endpoint_width), theme::dim())],
        Col::Input => vec![number(r.input_tokens, 5)],
        Col::Cached if is_rebuild(r) => {
            // A rebuilt cache: what was written, flagged, instead of what was read.
            let written = format!("↻{}", format_tokens(tokens_i64(r.cache_write_tokens.unwrap_or(0))));
            vec![Span::styled(pad_start(&written, 6), theme::danger())]
        }
        Col::Cached => {
            let pct = share_pct(r.cache_read_tokens.unwrap_or(0), r.context_tokens());
            let mut span = number(r.cache_read_tokens, 6);
            if r.context_tokens() > 0 {
                span.style = hit_style(pct);
            }
            vec![span]
        }
        Col::Output => vec![number(r.output_tokens, 5)],
        Col::Cost if r.outcome == RequestOutcome::InFlight => vec![Span::styled(pad_start("·", 6), theme::dim())],
        Col::Cost if r.context_tokens() == 0 && r.output_tokens.unwrap_or(0) == 0 => vec![Span::styled(pad_start("·", 6), theme::dim())],
        Col::Cost => vec![Span::raw(pad_start(&dollars(r.cost_usd_est()), 6))],
        Col::Latency => vec![latency(ins, r, 6)],
        Col::Status => vec![status_dot(r.outcome)],
    }
}

/// The model on its provider's colour: `width` cells with the padding
/// after, or as wide as the name with `None`.
fn badge(r: &ModelRequest, width: Option<usize>) -> Vec<Span<'static>> {
    let name = r.model.as_deref().map_or_else(|| r.provider.as_str().to_string(), short_model);
    let name = match width {
        Some(w) => ellipsize(&name, w.saturating_sub(2)),
        None => name,
    };
    let text = format!(" {name} ");
    let pad = width.map_or(0, |w| w.saturating_sub(text_width(&text)));
    vec![Span::styled(text, Style::default().fg(Color::Black).bg(provider_color(r.provider))), Span::raw(" ".repeat(pad))]
}

/// `claude-sonnet-5-5` reads as `sonnet-5-5`: the family says the provider.
pub(crate) fn short_model(model: &str) -> String {
    model.strip_prefix("claude-").unwrap_or(model).to_string()
}

fn provider_color(p: ModelProvider) -> Color {
    match p {
        ModelProvider::Anthropic => Color::Rgb(217, 119, 87),
        ModelProvider::OpenAi | ModelProvider::ChatGpt => Color::Rgb(16, 163, 127),
    }
}

fn status_dot(outcome: RequestOutcome) -> Span<'static> {
    match outcome {
        RequestOutcome::InFlight => Span::styled("●", theme::accent()),
        RequestOutcome::Completed => Span::styled("●", theme::ok()),
        RequestOutcome::HttpError | RequestOutcome::Failed => Span::styled("●", theme::danger()),
        RequestOutcome::Cancelled => Span::styled("●", theme::dim()),
    }
}

fn outcome_text(outcome: RequestOutcome) -> &'static str {
    match outcome {
        RequestOutcome::InFlight => t!("in flight"),
        RequestOutcome::Completed => t!("ok"),
        RequestOutcome::HttpError => t!("error"),
        RequestOutcome::Failed => t!("failed"),
        RequestOutcome::Cancelled => t!("cancelled"),
    }
}

/// How long it took, or (dim, with an ellipsis) has been running.
fn latency(ins: &Inspector, r: &ModelRequest, width: usize) -> Span<'static> {
    match (r.duration_ms, r.outcome) {
        (Some(ms), _) => Span::raw(pad_start(&duration(u64::from(ms)), width)),
        (None, RequestOutcome::InFlight) => {
            let ran = u64::try_from(ins.data.now_ms.saturating_sub(r.started_at_ms)).unwrap_or(0);
            Span::styled(pad_start(&format!("{}…", duration(ran)), width), theme::accent())
        }
        (None, _) => Span::styled(pad_start("·", width), theme::dim()),
    }
}

/// The inspector for the selected request: its inference details, then
/// what its context was made of, scrolled as asked.
fn inspector_lines(ins: &Inspector, width: usize, rows: u16) -> Vec<Line<'static>> {
    let Some(r) = ins.selected_request() else {
        return vec![fit(vec![Span::styled(t!("select a request to inspect it"), theme::dim())], width)];
    };
    let tall = rows >= TALL_INSPECTOR_MIN_ROWS;
    let mut out = if tall { inference_rows(ins, r, width) } else { inference_compact(ins, r, width) };
    out.push(Line::raw(""));
    out.extend(breakdown_lines(ins, r, width));
    if tall {
        out.push(Line::raw(""));
        out.extend(turn_lines(ins, r, width));
    }
    let skip = usize::from(ins.scroll).min(out.len().saturating_sub(1));
    out.into_iter().skip(skip).take(usize::from(rows)).collect()
}

/// pay's inference panel: a label and a value a row.
fn inference_rows(ins: &Inspector, r: &ModelRequest, width: usize) -> Vec<Line<'static>> {
    let sep = || Span::styled(" · ", theme::dim());
    let mut heading = vec![Span::styled(t!("inference"), theme::bold())];
    if r.outcome == RequestOutcome::InFlight {
        heading.push(Span::styled(format!("  ● {}", t!("live")), theme::accent()));
    }
    let (name, color) = ins.agent(&r.agent_pubkey);
    let turn = r.turn.map_or_else(|| t!("no turn").to_string(), |n| t!("turn {n}", n = n));
    let thread = ins.data.threads.get(&r.thread_id).cloned().unwrap_or_else(|| r.thread_id.chars().take(8).collect());
    let mut status = vec![status_dot(r.outcome), Span::raw(format!(" {}", outcome_text(r.outcome)))];
    if let Some(code) = r.http_status {
        status.push(Span::styled(format!(" {code}"), theme::dim()));
    }
    if let Some(stop) = &r.stop_reason {
        status.extend([sep(), Span::raw(stop.clone())]);
    }
    // Slowed on purpose (`AXEL_THROTTLE`): its timings are the throttle's.
    if r.throttled {
        status.extend([sep(), Span::styled(t!("network throttled"), theme::accent())]);
    }
    let mut rows: Vec<(&'static str, Vec<Span<'static>>)> = vec![
        (t!("model"), [badge(r, None), vec![Span::styled(format!(" {}", r.provider.as_str()), theme::dim())]].concat()),
        (t!("endpoint"), vec![Span::raw(r.endpoint.clone()), sep(), Span::styled(if r.streamed { t!("streamed") } else { t!("not streamed") }, theme::dim())]),
        (t!("status"), status),
        (t!("agent"), vec![Span::styled(name, Style::default().fg(theme::agent_color(color))), sep(), Span::raw(turn), sep(), Span::styled(thread, theme::dim())]),
        (t!("started"), vec![Span::raw(clock(r.started_at_ms))]),
        (t!("first token"), vec![Span::raw(r.first_byte_ms.map_or_else(|| "·".to_string(), |ms| duration(u64::from(ms))))]),
        (t!("took"), vec![latency(ins, r, 0)]),
    ];
    if reported(r) {
        let n = |v: Option<u64>| v.map_or_else(|| "·".to_string(), |v| format_tokens(tokens_i64(v)));
        rows.push((t!("tokens"), vec![Span::raw(t!("{input} in · {output} out", input = n(r.input_tokens), output = n(r.output_tokens)))]));
        let mut cache = vec![Span::raw(t!("{read} read · {written} written", read = n(r.cache_read_tokens), written = n(r.cache_write_tokens)))];
        if r.context_tokens() > 0 {
            let pct = share_pct(r.cache_read_tokens.unwrap_or(0), r.context_tokens());
            cache.extend([sep(), Span::styled(t!("{pct}% hit", pct = pct), hit_style(pct))]);
        }
        rows.push((t!("cache"), cache));
    } else {
        rows.push((t!("tokens"), token_spans(r)));
    }
    if let Some(reasoning) = r.reasoning_tokens {
        rows.push((t!("reasoning"), vec![Span::raw(format_tokens(tokens_i64(reasoning)))]));
    }
    if let Some(rate) = throughput(r) {
        rows.push((t!("throughput"), vec![Span::raw(t!("{n} tokens/s", n = rate))]));
    }
    rows.push((t!("cost"), vec![Span::styled(cost(r.cost_usd_est()), theme::bold()), sep(), Span::styled(t!("{sent} sent · {received} received", sent = bytes(r.request_bytes), received = bytes(r.response_bytes)), theme::dim())]));
    if let Some(error) = &r.error {
        rows.push((t!("error"), vec![Span::styled(error.clone(), theme::danger())]));
    }
    let label_width = rows.iter().map(|(l, _)| text_width(l)).max().unwrap_or(0).min(LABEL_MAX);
    let mut out = vec![fit(heading, width)];
    for (label, value) in rows {
        let mut spans = vec![Span::styled(format!("{} ", pad_end(&ellipsize(label, label_width), label_width)), theme::dim())];
        spans.extend(value);
        out.push(fit(spans, width));
    }
    out
}

/// The inference details in two lines (three with an error), for a short inspector.
fn inference_compact(ins: &Inspector, r: &ModelRequest, width: usize) -> Vec<Line<'static>> {
    let sep = || Span::styled(" · ", theme::dim());
    let mut first = badge(r, None);
    first.extend([Span::raw(" "), Span::styled(r.endpoint.clone(), theme::dim()), sep(), status_dot(r.outcome), Span::raw(format!(" {}", outcome_text(r.outcome)))]);
    if let Some(code) = r.http_status {
        first.push(Span::styled(format!(" {code}"), theme::dim()));
    }
    if r.throttled {
        first.extend([sep(), Span::styled(t!("network throttled"), theme::accent())]);
    }
    first.extend([sep(), latency(ins, r, 0)]);
    if let Some(ms) = r.first_byte_ms {
        first.push(Span::styled(format!(" ({})", t!("first token {time}", time = duration(u64::from(ms)))), theme::dim()));
    }
    first.extend([sep(), Span::styled(cost(r.cost_usd_est()), theme::bold())]);
    let mut second = token_spans(r);
    if let Some(rate) = throughput(r) {
        second.extend([sep(), Span::styled(t!("{n} tokens/s", n = rate), theme::dim())]);
    }
    let mut out = vec![fit(first, width), fit(second, width)];
    if let Some(error) = &r.error {
        out.push(fit(vec![Span::styled(error.clone(), theme::danger())], width));
    }
    out
}

/// `1.2k in · 92k cached · 2.1k written · 310 out`.
fn token_spans(r: &ModelRequest) -> Vec<Span<'static>> {
    if !reported(r) {
        let why = if r.outcome == RequestOutcome::InFlight { t!("counted when the response ends") } else { t!("the provider reported no usage") };
        return vec![Span::styled(why, theme::dim())];
    }
    let n =|v: Option<u64>| v.map_or_else(|| "·".to_string(), |v| format_tokens(tokens_i64(v)));
    let text = t!(
        "{input} in · {cached} cached · {written} written · {output} out",
        input = n(r.input_tokens),
        cached = n(r.cache_read_tokens),
        written = n(r.cache_write_tokens),
        output = n(r.output_tokens)
    );
    vec![Span::raw(text)]
}

/// The context of each request in `r`'s turn, oldest first, on one scale:
/// a context that climbs request after request is what makes a turn dear.
fn turn_lines(ins: &Inspector, r: &ModelRequest, width: usize) -> Vec<Line<'static>> {
    let key = crate::inspect::GroupKey::of(r);
    let mut turn: Vec<&ModelRequest> = ins.data.requests.iter().filter(|x| crate::inspect::GroupKey::of(x) == key).collect();
    turn.reverse();
    let skipped = turn.len().saturating_sub(TURN_REQUESTS_SHOWN);
    let turn = &turn[skipped..];
    let peak = turn.iter().map(|x| x.context_tokens()).max().unwrap_or(0);
    let name = r.turn.map_or_else(|| t!("no turn").to_string(), |n| t!("turn {n}", n = n));
    let mut out = vec![fit(vec![Span::styled(name, theme::bold()), Span::styled(format!("  {}", t!("context per request, oldest first")), theme::dim())], width)];
    if skipped > 0 {
        out.push(fit(vec![Span::styled(t!("{n} earlier", n = skipped), theme::dim())], width));
    }
    const LEAD: usize = 8 + 1;
    // A space, the tokens, the rebuild mark and the selection's.
    const NUMBERS: usize = 1 + 6 + 2 + 1;
    let bar_width = width.saturating_sub(LEAD + NUMBERS);
    for x in turn {
        let marker = if x.id == r.id { Span::styled("‹", theme::accent_bold()) } else { Span::raw(" ") };
        let mut spans = vec![Span::styled(format!("{} ", clock(x.started_at_ms)), theme::dim())];
        if bar_width >= BAR_MIN {
            let style = if is_rebuild(x) { theme::danger() } else { Style::default().fg(kind_color(ContextKind::ToolResult)) };
            spans.push(Span::styled(bar(x.context_tokens(), peak, bar_width), style));
        }
        let size = if x.context_tokens() == 0 { "·".to_string() } else { format_tokens(tokens_i64(x.context_tokens())) };
        spans.push(Span::raw(format!(" {}", pad_start(&size, 6))));
        spans.push(Span::styled(if is_rebuild(x) { " ↻" } else { "  " }, theme::danger()));
        spans.push(marker);
        out.push(fit(spans, width));
    }
    out
}

/// The provider said how many tokens the request took.
fn reported(r: &ModelRequest) -> bool {
    [r.input_tokens, r.cache_read_tokens, r.cache_write_tokens, r.output_tokens].iter().any(Option::is_some)
}

/// Output tokens a second, over the time after the first byte.
fn throughput(r: &ModelRequest) -> Option<u64> {
    let output = r.output_tokens.filter(|n| *n > 0)?;
    let generating_ms = u64::from(r.duration_ms?.checked_sub(r.first_byte_ms?)?);
    (generating_ms > 0).then(|| output.saturating_mul(1000) / generating_ms)
}

/// What filled the context, a bar a part, and the part that filled it most.
fn breakdown_lines(ins: &Inspector, r: &ModelRequest, width: usize) -> Vec<Line<'static>> {
    let sep = || Span::styled(" · ", theme::dim());
    let Some(breakdown) = &r.breakdown else {
        return vec![
            fit(vec![Span::styled(t!("context"), theme::bold())], width),
            fit(vec![Span::styled(t!("no breakdown was recorded for this request"), theme::dim())], width),
        ];
    };
    let est = estimate(breakdown, r.context_tokens());
    let mut out = vec![fit(
        vec![
            Span::styled(t!("context"), theme::bold()),
            Span::raw(format!("  {} ", format_tokens(tokens_i64(est.tokens)))),
            Span::styled(t!("tokens"), theme::dim()),
            sep(),
            Span::styled(t!("{n} messages", n = breakdown.messages), theme::dim()),
            sep(),
            Span::styled(t!("{n} tools", n = breakdown.tools), theme::dim()),
        ],
        width,
    )];
    let note = if est.scaled { t!("estimated by size, scaled to the billed tokens") } else { t!("estimated by size: about 4 bytes a token") };
    out.push(fit(vec![Span::styled(note, theme::dim())], width));
    // The answer before the evidence: the part that made it big, then the
    // rebuild if there was one, then a bar a part.
    if let Some((top, pct)) = largest(&est) {
        let what = t!("largest: {part}", part = largest_name(&top));
        let share = t!("{pct}% of this request", pct = pct);
        let one = format!("▲ {what}, {share}");
        if text_width(&one) <= width {
            out.push(Line::styled(one, theme::accent()));
        } else {
            out.push(fit(vec![Span::styled(format!("▲ {what}"), theme::accent())], width));
            out.push(fit(vec![Span::styled(format!("  {share}"), theme::accent())], width));
        }
    }
    if is_rebuild(r) {
        let text = t!("the cache was rebuilt: {n} tokens written, not read", n = format_tokens(tokens_i64(r.cache_write_tokens.unwrap_or(0))));
        out.push(fit(vec![Span::styled("↻ ", theme::danger()), Span::styled(text, theme::danger())], width));
    }
    let rows = bar_rows(&est, ins.tools_open);
    let peak = rows.iter().map(|b| b.tokens).max().unwrap_or(0);
    // The names whole before a long bar: a bar's length only compares.
    let name_width = rows.iter().map(|b| text_width(&bar_name(b))).max().unwrap_or(0).min(BAR_NAME_MAX).min(width / 2);
    for b in &rows {
        out.push(bar_line(b, est.tokens, peak, name_width, width));
    }
    out
}

/// `tool results · Read   ██████▍        38k  41%`: the name, the bar on one
/// scale for every part (the largest fills it), the tokens and the share.
fn bar_line(b: &BarRow, total: u64, peak: u64, name_width: usize, width: usize) -> Line<'static> {
    const NUMBERS: usize = 6 + 5;
    let name = pad_end(&ellipsize(&bar_name(b), name_width), name_width);
    let bar_width = width.saturating_sub(name_width + 2 + NUMBERS);
    let name_style = if b.nested { theme::dim() } else { Style::default() };
    let mut spans = vec![Span::styled(name, name_style), Span::raw(" ")];
    if bar_width >= BAR_MIN {
        spans.push(Span::styled(bar(b.tokens, peak, bar_width), Style::default().fg(kind_color(b.kind))));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::raw(pad_start(&format_tokens(tokens_i64(b.tokens)), 6)));
    spans.push(Span::styled(pad_start(&format!("{}%", share_pct(b.tokens, total)), 5), theme::dim()));
    fit(spans, width)
}

/// `value` of `peak` as a bar `width` cells wide, to an eighth of a cell, padded.
fn bar(value: u64, peak: u64, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = if peak == 0 { 0 } else { u128::from(value) * (width as u128) * 8 / u128::from(peak) };
    let eighths = usize::try_from(eighths).unwrap_or(width * 8).min(width * 8);
    // Anything at all shows as a sliver.
    let eighths = if value > 0 { eighths.max(1) } else { 0 };
    let mut out = "█".repeat(eighths / 8);
    out.push_str(EIGHTHS[eighths % 8]);
    let pad = width - text_width(&out);
    out.push_str(&" ".repeat(pad));
    debug_assert_eq!(text_width(&out), width, "a bar is its width");
    out
}

fn kind_name(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::System => t!("system prompt"),
        ContextKind::ToolDefinition => t!("tool definitions"),
        ContextKind::UserText => t!("user text"),
        ContextKind::AssistantText => t!("assistant text"),
        ContextKind::ToolCall => t!("tool calls"),
        ContextKind::ToolResult => t!("tool results"),
        ContextKind::Image => t!("images"),
        ContextKind::Reasoning => t!("reasoning"),
        ContextKind::Other => t!("other"),
    }
}

fn bar_name(b: &BarRow) -> String {
    match b.kind {
        _ if b.nested => format!("  {}", b.label),
        ContextKind::ToolResult if !b.label.is_empty() => format!("{} · {}", kind_name(b.kind), b.label),
        ContextKind::ToolDefinition => format!("{} ({})", kind_name(b.kind), b.count),
        kind => kind_name(kind).to_string(),
    }
}

pub(crate) fn largest_name(b: &BarRow) -> String {
    match b.kind {
        ContextKind::ToolResult if !b.label.is_empty() => t!("tool results from {tool}", tool = b.label),
        kind => kind_name(kind).to_string(),
    }
}

fn kind_color(kind: ContextKind) -> Color {
    match kind {
        ContextKind::System => Color::Magenta,
        ContextKind::ToolDefinition => Color::Yellow,
        ContextKind::UserText => Color::Green,
        ContextKind::AssistantText => Color::Cyan,
        ContextKind::ToolCall => Color::Indexed(208),
        ContextKind::ToolResult => theme::ACCENT,
        ContextKind::Image => Color::Blue,
        ContextKind::Reasoning => Color::Gray,
        ContextKind::Other => theme::DIM,
    }
}

/// Totals by agent and by model, the cache-hit rate, and the anomalies.
fn sidebar_lines(ins: &Inspector, view: &ListView, width: usize, rows: usize) -> Vec<Line<'static>> {
    let requests = &ins.data.requests;
    let heading = |text: &'static str| fit(vec![Span::styled(text, theme::bold())], width);
    let mut out = vec![heading(t!("by agent"))];
    let mut by_agent: Vec<(String, Totals)> = Vec::new();
    let mut by_model: Vec<(String, Totals)> = Vec::new();
    for &i in &view.shown {
        let r = &requests[i];
        let model = r.model.as_deref().map_or_else(|| r.provider.as_str().to_string(), short_model);
        for (list, key) in [(&mut by_agent, r.agent_pubkey.clone()), (&mut by_model, model)] {
            match list.iter_mut().find(|(k, _)| *k == key) {
                Some((_, t)) => t.add(r),
                None => list.push((key, Totals::of([r]))),
            }
        }
    }
    for list in [&mut by_agent, &mut by_model] {
        list.sort_by(|a, b| b.1.cost_usd.total_cmp(&a.1.cost_usd));
    }
    for (pk, totals) in &by_agent {
        let (name, color) = ins.agent(pk);
        out.push(total_line(Span::styled(name, Style::default().fg(theme::agent_color(color))), totals, width));
    }
    out.push(Line::raw(""));
    out.push(heading(t!("by model")));
    for (model, totals) in &by_model {
        out.push(total_line(Span::raw(model.clone()), totals, width));
    }
    out.push(Line::raw(""));
    let hit = view.totals.cache_hit_pct().map_or_else(|| "·".to_string(), |p| format!("{p}%"));
    let label = t!("cache hit");
    let gap = width.saturating_sub(text_width(label) + text_width(&hit));
    out.push(fit(vec![Span::styled(label, theme::bold()), Span::raw(" ".repeat(gap)), Span::styled(hit, view.totals.cache_hit_pct().map_or(theme::dim(), hit_style))], width));
    out.push(Line::raw(""));
    out.push(heading(t!("anomalies")));
    let found = crate::inspect::anomalies(requests, &view.groups);
    if found.is_empty() {
        out.push(fit(vec![Span::styled(t!("nothing unusual"), theme::dim())], width));
    }
    for a in &found {
        out.extend(anomaly_lines(ins, view, a, width));
    }
    out.truncate(rows);
    out
}

/// `terra        12 · 380k · ~$0.41`: the requests, context and cost at the right.
fn total_line(name: Span<'static>, t: &Totals, width: usize) -> Line<'static> {
    let numbers = format!("{:>3} · {:>5} · {:>6}", t.requests, format_tokens(tokens_i64(t.context_tokens)), cost(t.cost_usd));
    let name_width = width.saturating_sub(text_width(&numbers) + 1);
    let name = Span::styled(pad_end(&ellipsize(&name.content, name_width), name_width), name.style);
    fit(vec![name, Span::raw(" "), Span::styled(numbers, theme::dim())], width)
}

/// An anomaly in two lines: where, then what.
fn anomaly_lines(ins: &Inspector, view: &ListView, a: &Anomaly, width: usize) -> Vec<Line<'static>> {
    let requests = &ins.data.requests;
    let place = |pk: &str, turn: Option<u32>| -> Vec<Span<'static>> {
        let (name, color) = ins.agent(pk);
        let turn = turn.map_or_else(|| t!("no turn").to_string(), |n| t!("turn {n}", n = n));
        vec![Span::styled(name, Style::default().fg(theme::agent_color(color))), Span::styled(format!(" · {turn}"), theme::dim())]
    };
    let (mark, style, where_, what) = match *a {
        Anomaly::Errors { count } => ("✕ ", theme::danger(), vec![Span::styled(t!("{n} failed", n = count), theme::danger())], t!("see the red dots in the list").to_string()),
        Anomaly::CacheRebuilt { request, written } => {
            let r = &requests[request];
            ("↻ ", theme::danger(), place(&r.agent_pubkey, r.turn), t!("cache rebuilt, {n} written", n = format_tokens(tokens_i64(written))))
        }
        Anomaly::ContextClimb { group, from, to, requests: n } => {
            let g = &view.groups[group];
            let what = t!("context {from} → {to} over {n} requests", from = format_tokens(tokens_i64(from)), to = format_tokens(tokens_i64(to)), n = n);
            ("↗ ", theme::accent(), place(&g.agent_pubkey, g.key.turn), what)
        }
    };
    let mut first = vec![Span::styled(mark, style)];
    first.extend(where_);
    vec![fit(first, width), fit(vec![Span::styled(format!("  {what}"), theme::dim())], width)]
}

/// Nothing recorded: what the inspector is, and what it needs.
fn empty_state(frame: &mut Frame, ins: &Inspector, area: Rect) {
    let mut lines = vec![
        Line::from(""),
        Line::styled(t!("Token inspector"), theme::accent_bold()),
        Line::from(""),
        Line::from(t!("Every request your agents send to their model: what it cost, and what filled its context.")),
        Line::from(""),
        Line::styled(t!("Nothing was recorded here in the last 24 hours."), theme::dim()),
    ];
    let on_thread = matches!(ins.scope, Scope::Thread(_));
    match (ins.is_recording(), on_thread) {
        (true, true) => lines.push(Line::styled(t!("This thread's new managed sessions are recorded as they work."), theme::dim())),
        (true, false) => lines.push(Line::styled(t!("Inspected threads' new managed sessions are recorded as they work."), theme::dim())),
        (false, true) => {
            lines.push(Line::styled(t!("Recording is off for this thread: nothing goes through the inspector until you ask."), theme::dim()));
            lines.push(Line::from(vec![
                Span::styled(t!("Press"), theme::dim()),
                Span::styled(" r ", theme::pill()),
                Span::styled(t!("or run"), theme::dim()),
                Span::styled(" axel inspect on ", theme::pill()),
                Span::styled(t!("then start a managed session."), theme::dim()),
            ]));
        }
        (false, false) => {
            lines.push(Line::styled(t!("Recording is off for every thread: nothing goes through the inspector until you ask."), theme::dim()));
            lines.push(Line::from(vec![
                Span::styled(t!("Inside a thread, run"), theme::dim()),
                Span::styled(" axel inspect on ", theme::pill()),
                Span::styled(t!("then start a managed session."), theme::dim()),
            ]));
        }
    }
    if ins.scope != Scope::All {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled("f", theme::accent()), Span::styled(format!(" {}", t!("shows every thread")), theme::dim())]));
    }
    // Wrapped, so these may take more rows than lines: drawn without `put`.
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }), area);
}

/// The keys, in a box over the middle.
fn help_overlay(frame: &mut Frame, area: Rect) {
    let keys: [(&str, &str); 11] = [
        ("↑↓ j k", t!("move")),
        ("Enter Tab", t!("inspect the request")),
        ("Esc q", t!("back to the list, then quit")),
        ("f", t!("filter: thread, agent, all")),
        ("/", t!("search model, agent and endpoint")),
        ("g", t!("group by turn")),
        ("space ← →", t!("fold or unfold a turn")),
        ("t", t!("each tool's definition")),
        ("r", t!("record this thread, or stop")),
        ("PgUp PgDn", t!("page")),
        ("?", t!("close this help")),
    ];
    let key_width = keys.iter().map(|(k, _)| text_width(k)).max().unwrap_or(0);
    let want = keys.iter().map(|(_, w)| key_width + 2 + text_width(w)).max().unwrap_or(0) + 4;
    let width = u16::try_from(want).unwrap_or(u16::MAX).min(area.width.saturating_sub(2));
    let height = (u16::try_from(keys.len()).unwrap_or(u16::MAX) + 2).min(area.height.saturating_sub(2));
    let rect = super::centered(area, width, height);
    let inner = usize::from(rect.width.saturating_sub(4));
    let lines: Vec<Line<'static>> = keys
        .iter()
        .take(usize::from(rect.height.saturating_sub(2)))
        .map(|(k, what)| fit(vec![Span::styled(pad_end(k, key_width), theme::accent()), Span::styled(format!("  {what}"), theme::dim())], inner))
        .collect();
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).border_style(theme::dim()).title(Span::styled(format!(" {} ", t!("keys")), theme::bold()));
    let body = block.inner(rect);
    frame.render_widget(block, rect);
    let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    put(frame, lines, body);
}

/// The cache-hit share's colour: green when most is read from the cache.
fn hit_style(pct: u64) -> Style {
    if pct >= 80 {
        theme::ok()
    } else if pct >= 50 {
        Style::default()
    } else {
        theme::accent()
    }
}

/// Dollars with the estimate mark, to the cent below $100.
fn cost(usd: f64) -> String {
    if usd < 100.0 { format!("~${usd:.2}") } else { format!("~${usd:.0}") }
}

/// Dollars in a 6-cell column: `<$0.01`, `$0.41`, `$12.3`, `$140`.
fn dollars(usd: f64) -> String {
    if usd <= 0.0 {
        "$0".to_string()
    } else if usd < 0.01 {
        "<$0.01".to_string()
    } else if usd < 10.0 {
        format!("${usd:.2}")
    } else if usd < 100.0 {
        format!("${usd:.1}")
    } else {
        format!("${usd:.0}")
    }
}

/// `820ms`, `4.2s`, `3m05s`.
fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60)
    }
}

/// `108 KB`, `1.2 MB`.
fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KB", (n + 512) / 1024)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Local wall-clock time of a Unix millisecond.
fn clock(ms: i64) -> String {
    chrono::Local.timestamp_millis_opt(ms).single().map_or_else(|| "--:--:--".to_string(), |t| t.format("%H:%M:%S").to_string())
}

/// For [`format_tokens`], which takes the usage pane's signed counts.
fn tokens_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::fixtures;
    use crate::model::InspectData;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const NOW: i64 = 1_800_000_000_000;

    fn screen(w: u16, h: u16, ins: &Inspector) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, ins, f.area())).unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect()
    }

    fn inspector() -> Inspector {
        Inspector::new(None, fixtures::data(NOW))
    }

    const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];

    #[test]
    fn every_size_draws_the_list_the_inspector_and_the_bars_within_the_pane() {
        for (w, h) in SIZES {
            // `put` asserts every line fits; this draws them all.
            let s = screen(w, h, &inspector());
            let all = s.join("\n");
            assert!(s[0].contains("token inspector") && s[0].contains("12 requests") && s[0].contains("live"), "toolbar at {w}×{h}:\n{all}");
            assert!(all.contains("turn 7") && all.contains("terra"), "grouped by turn at {w}×{h}:\n{all}");
            assert!(all.contains("sonnet-5-5"), "the model badge at {w}×{h}:\n{all}");
            assert!(all.contains("tool results · Read") && all.contains('█'), "the breakdown's bars at {w}×{h}:\n{all}");
            assert!(s[usize::from(h) - 1].contains("filter"), "the key help at the bottom at {w}×{h}:\n{all}");
        }
    }

    #[test]
    fn the_layout_follows_the_width() {
        let narrow = screen(80, 24, &inspector());
        assert!(narrow.iter().any(|l| l.starts_with("────")), "stacked: a rule between list and inspector:\n{}", narrow.join("\n"));
        let mid = screen(120, 40, &inspector());
        assert!(mid[2].contains('│') && !mid.join("\n").contains("by agent"), "side by side, no sidebar:\n{}", mid.join("\n"));
        assert!(mid.iter().any(|l| l.contains("first token")), "the tall inspector labels each detail:\n{}", mid.join("\n"));
        let wide = screen(200, 60, &inspector());
        let all = wide.join("\n");
        assert!(all.contains("by agent") && all.contains("by model") && all.contains("cache hit"), "the sidebar:\n{all}");
        assert!(all.contains("cache rebuilt") && all.contains("context 52k → 141k"), "anomalies flagged:\n{all}");
    }

    #[test]
    fn the_largest_part_is_called_out() {
        let mut ins = inspector();
        // The in-flight request has no usage yet: bytes over four.
        let s = screen(120, 40, &ins).join("\n");
        assert!(s.contains("about 4 bytes a token"), "{s}");
        ins.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let s = screen(120, 40, &ins).join("\n");
        assert!(s.contains("scaled to the billed tokens"), "{s}");
        assert!(s.contains("largest: tool results from Read"), "{s}");
    }

    #[test]
    fn tools_open_under_their_definitions() {
        let mut ins = inspector();
        assert!(!screen(120, 40, &ins).iter().any(|l| l.contains("  mcp__axel__send")));
        ins.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(screen(120, 40, &ins).iter().any(|l| l.contains("  mcp__axel__send")), "each tool's own bar");
    }

    #[test]
    fn nothing_recorded_says_what_the_inspector_is_and_needs() {
        for (w, h) in SIZES {
            let s = screen(w, h, &Inspector::new(Some("T1".into()), InspectData::default())).join("\n");
            assert!(s.contains("Token inspector") && s.contains("axel inspect on"), "at {w}×{h}:\n{s}");
            assert!(s.contains("shows every thread"), "{s}");
        }
    }

    #[test]
    fn a_sliver_is_one_line_and_help_fits() {
        let s = screen(28, 3, &inspector());
        assert!(s[0].contains("12 requests"), "{s:?}");
        let mut ins = inspector();
        ins.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        for (w, h) in SIZES {
            assert!(screen(w, h, &ins).join("\n").contains("group by turn"));
        }
    }

    #[test]
    fn bars_fill_to_an_eighth() {
        assert_eq!(bar(8, 8, 4), "████");
        assert_eq!(bar(1, 8, 4), "▌   ");
        assert_eq!(bar(1, 1_000_000, 4), "▏   ", "anything shows");
        assert_eq!(bar(0, 8, 4), "    ");
    }

    #[test]
    fn columns_join_by_need() {
        let (cols, _) = columns(40, 5);
        assert!(cols.contains(&Col::Model) && cols.contains(&Col::Cost) && !cols.contains(&Col::Endpoint), "{cols:?}");
        let (cols, endpoint) = columns(100, 5);
        assert_eq!(cols.len(), 10);
        assert_eq!(endpoint, ENDPOINT_MAX.min(100 - 78 + ENDPOINT_MIN));
    }
}
