//! The usage pane: what each agent spent, laid out for the room it has.
//!
//! One row per agent. Columns join as the pane widens, most needed first:
//! the last hour, the last 24 hours, today's cost, how full its context is,
//! all time, the all-time cost, then a sparkline of the last hour taking
//! what's left. Rows join as it grows taller: column labels, a line for
//! everyone together, each harness's plan limits (how much of its five-hour
//! and weekly windows the account has used, with a bar and the time to the
//! reset as width allows), then the threads that spent today. Under
//! [`BOX_MIN_HEIGHT`] rows or [`BOX_MIN_WIDTH`] columns it's one line:
//! the last hour's tokens, today's cost and the fullest plan window.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};

use crate::model::{AgentSpend, LimitRow, PlanRow, UsageData};
use crate::theme;
use crate::util::{ellipsize, format_tokens};

/// Below this many rows there's no box: a summary line, then a row per
/// agent (one row is the summary alone).
pub const BOX_MIN_HEIGHT: u16 = 6;
/// …or below this many columns.
pub const BOX_MIN_WIDTH: u16 = 20;
/// Narrowest sparkline worth drawing.
const SPARK_MIN: usize = 6;
/// Widest an agent's name gets.
const NAME_MAX: usize = 10;

/// A column after the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Hour,
    Day,
    DayCost,
    Context,
    Total,
    TotalCost,
    Spark,
}

/// The order columns join in as the pane widens.
const BY_NEED: [Col; 7] = [Col::Hour, Col::Day, Col::DayCost, Col::Context, Col::Total, Col::TotalCost, Col::Spark];
/// The order they're shown in.
const SHOWN: [Col; 7] = [Col::Hour, Col::Day, Col::Total, Col::DayCost, Col::TotalCost, Col::Context, Col::Spark];

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Hour => "1h",
            Col::Day => "24h",
            Col::DayCost => "$ 24h",
            Col::Context => "ctx",
            Col::Total => "all",
            Col::TotalCost => "$ all",
            Col::Spark => "last hour",
        }
    }

    /// Its width; the sparkline's is its narrowest.
    fn width(self) -> usize {
        match self {
            Col::Hour | Col::Day | Col::Total => 6,
            Col::DayCost | Col::TotalCost => 7,
            Col::Context => 4,
            Col::Spark => SPARK_MIN,
        }
    }
}

/// The columns that fit in `width` after a name of `name` columns, in the
/// order shown, and the sparkline's width (0 when there's none).
fn columns(width: usize, name: usize) -> (Vec<Col>, usize) {
    let mut used = name;
    let mut chosen = Vec::new();
    for c in BY_NEED {
        if used + 1 + c.width() <= width {
            used += 1 + c.width();
            chosen.push(c);
        }
    }
    let spark = if chosen.contains(&Col::Spark) { width - used + SPARK_MIN } else { 0 };
    (SHOWN.into_iter().filter(|c| chosen.contains(c)).collect(), spark)
}

pub fn render(frame: &mut Frame, data: &UsageData, area: Rect) {
    if area.height == 1 || area.width < BOX_MIN_WIDTH {
        frame.render_widget(Paragraph::new(summary(data, area.width)), area);
        return;
    }
    // Short (beside the composer): no box, the summary on top, then a row
    // per agent and, with rows to spare, the plan limits.
    if area.height < BOX_MIN_HEIGHT {
        let mut out = vec![summary(data, area.width)];
        out.extend(lines(data, area.width as usize, area.height as usize - 1, true));
        frame.render_widget(Paragraph::new(out), area);
        return;
    }
    let today = format!(" {} today ", cost(data.last_day.cost_usd));
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::dim())
        .title(Line::from(vec![Span::raw(" "), Span::styled("tokens", theme::accent_bold()), Span::raw(" ")]))
        .padding(Padding::horizontal(1));
    if usize::from(area.width) >= " tokens ".len() + today.len() + 4 {
        block = block.title_top(Line::from(Span::styled(today, theme::dim())).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines(data, inner.width as usize, inner.height as usize, false)), inner);
}

/// One line: the last hour's tokens, today's cost and, when it fits, the
/// fullest plan window.
fn summary(data: &UsageData, width: u16) -> Line<'static> {
    {
        let mut spans = vec![
            Span::styled(format_tokens(data.last_hour.tokens()), theme::bold()),
            Span::styled(" 1h · ", theme::dim()),
            Span::styled(cost(data.last_day.cost_usd), theme::bold()),
            Span::styled(" 24h", theme::dim()),
        ];
        // The window closest to its limit, when it fits.
        let fullest = data.limits.iter().flat_map(|p| p.windows.iter().filter_map(|w| Some((w.window, w.used_pct?)))).max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((window, pct)) = fullest {
            let more = format!(" · {} {:.0}%", window.short(), pct);
            if Line::from(spans.clone()).width() + more.chars().count() <= usize::from(width) {
                spans.push(Span::styled(more, pct_style(pct)));
            }
        }
        Line::from(spans)
    }
}

/// The pane's lines for an inside of `width` × `rows`. `tight` (under the
/// summary line, beside the composer): no labels, no line for everyone (the
/// summary is that), the plan limits without their heading, no threads.
fn lines(data: &UsageData, width: usize, rows: usize, tight: bool) -> Vec<Line<'static>> {
    if data.agents.is_empty() {
        let mut out = if tight { Vec::new() } else { vec![Line::styled("no tokens spent yet", theme::dim())] };
        let name_w = data.limits.iter().map(|p| p.harness.chars().count()).max().unwrap_or(0).min(NAME_MAX);
        let left = rows.saturating_sub(out.len());
        plans(&mut out, &data.limits, width, left, name_w, tight);
        return out;
    }
    let longest = data.agents.iter().map(|a| a.name.chars().count()).chain(data.limits.iter().map(|p| p.harness.chars().count())).max().unwrap_or(0);
    let name_w = longest.clamp(3, NAME_MAX).min(width.saturating_sub(7).max(3));
    let (cols, spark_w) = columns(width, name_w);
    let n = data.agents.len();
    let mut out = Vec::new();
    // Labels when there's a row to spare for them.
    let labels = !tight && rows > n && !cols.is_empty();
    if labels {
        let mut spans = vec![Span::raw(" ".repeat(name_w))];
        for &c in &cols {
            let w = if c == Col::Spark { spark_w } else { c.width() };
            let text = if c == Col::Spark { format!(" {:<w$}", ellipsize(c.label(), w)) } else { format!(" {:>w$}", c.label()) };
            spans.push(Span::styled(text, theme::dim()));
        }
        out.push(Line::from(spans));
    }
    let room = rows - usize::from(labels);
    // Agents first; one that doesn't fit is counted on the last row.
    let shown = if n > room { room.saturating_sub(1) } else { n };
    for a in &data.agents[..shown] {
        out.push(agent_line(a, &cols, name_w, spark_w));
    }
    if shown < n {
        out.push(Line::styled(format!("+{} more", n - shown), theme::dim()));
        return out;
    }
    let mut left = room - shown;
    // Everyone together, when there's more than one.
    if !tight && n > 1 && left >= 1 {
        let all = AgentSpend {
            name: "all".into(),
            last_hour: data.last_hour.clone(),
            last_day: data.last_day.clone(),
            total: data.total.clone(),
            ..Default::default()
        };
        let mut line = agent_line(&all, &cols, name_w, 0);
        line.spans.iter_mut().for_each(|s| s.style = s.style.add_modifier(Modifier::DIM));
        out.push(line);
        left -= 1;
    }
    left -= plans(&mut out, &data.limits, width, left, name_w, tight);
    // The threads that spent today: a gap, a label, at least one.
    if !tight && !data.threads.is_empty() && left >= 3 {
        out.push(Line::raw(""));
        out.push(Line::styled("threads today", theme::dim()));
        left -= 2;
        let money = width >= 24;
        let title_w = width.saturating_sub(7 + if money { 8 } else { 0 }).max(4);
        let fits = data.threads.len().min(left);
        for t in &data.threads[..fits] {
            let mut spans = vec![
                Span::raw(format!("{:<title_w$}", ellipsize(&t.title, title_w))),
                Span::styled(format!(" {:>6}", format_tokens(t.last_day.tokens())), theme::bold()),
            ];
            if money {
                spans.push(Span::styled(format!(" {:>7}", cost(t.last_day.cost_usd)), theme::dim()));
            }
            out.push(Line::from(spans));
        }
    }
    out
}

/// The plan limits section (a gap, a label, a row per harness; `tight`, the
/// rows alone) when it all fits in `left` rows; the rows it took.
fn plans(out: &mut Vec<Line<'static>>, plans: &[PlanRow], width: usize, left: usize, name_w: usize, tight: bool) -> usize {
    let heading = if tight { 0 } else { 2 };
    if plans.is_empty() || left < plans.len() + heading {
        return 0;
    }
    if !tight {
        out.push(Line::raw(""));
        out.push(Line::styled("plan limits", theme::dim()));
    }
    for p in plans {
        out.push(plan_line(p, width, name_w));
    }
    plans.len() + heading
}

/// One harness's windows, as much of each as fits: a bar, the share used
/// and the time to the reset, then without the bar, then the share alone.
fn plan_line(p: &PlanRow, width: usize, name_w: usize) -> Line<'static> {
    let name = Span::styled(format!("{:<name_w$}", ellipsize(&p.harness, name_w)), theme::dim());
    for detail in [Detail::Bar, Detail::Reset, Detail::Share] {
        let mut spans = vec![name.clone()];
        for w in &p.windows {
            spans.extend(window_spans(w, detail));
        }
        let line = Line::from(spans);
        if line.width() <= width || detail == Detail::Share {
            return line;
        }
    }
    unreachable!("the last detail always returns")
}

/// How much of a plan window to show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    Bar,
    Reset,
    Share,
}

fn window_spans(w: &LimitRow, detail: Detail) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!("  {} ", w.window.short()), theme::dim())];
    let Some(pct) = w.used_pct else {
        // Started over since it was reported: unknown until the next report.
        spans.push(Span::styled(" new", theme::dim()));
        return spans;
    };
    if detail == Detail::Bar {
        const CELLS: usize = 5;
        let full = ((pct / 100.0 * CELLS as f64).round() as usize).min(CELLS);
        spans.push(Span::styled("▰".repeat(full), pct_style(pct)));
        spans.push(Span::styled(format!("{} ", "▱".repeat(CELLS - full)), theme::dim()));
    }
    spans.push(Span::styled(format!("{:>3.0}%", pct), pct_style(pct)));
    if detail != Detail::Share
        && let Some(s) = w.resets_in
    {
        spans.push(Span::styled(format!(" ↻{:<3}", until(s)), theme::dim()));
    }
    spans
}

/// Closer to the limit, louder.
fn pct_style(pct: f64) -> Style {
    if pct >= 80.0 {
        theme::danger()
    } else if pct >= 60.0 {
        theme::accent()
    } else {
        Style::default()
    }
}

/// `secs` as its largest unit: `45m`, `3h`, `5d`.
fn until(secs: i64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn agent_line(a: &AgentSpend, cols: &[Col], name_w: usize, spark_w: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{:<name_w$}", ellipsize(&a.name, name_w)), Style::default().fg(theme::agent_color(a.color)))];
    for &c in cols {
        spans.push(match c {
            Col::Hour => tokens(a.last_hour.tokens(), true),
            Col::Day => tokens(a.last_day.tokens(), false),
            Col::Total => tokens(a.total.tokens(), false),
            Col::DayCost => Span::styled(format!(" {:>7}", cost(a.last_day.cost_usd)), theme::dim()),
            Col::TotalCost => Span::styled(format!(" {:>7}", cost(a.total.cost_usd)), theme::dim()),
            Col::Context => match a.context {
                Some((used, size)) if size > 0 => {
                    let pct = (used * 100 / size).clamp(0, 100);
                    let style = if pct >= 80 {
                        theme::danger()
                    } else if pct >= 60 {
                        theme::accent()
                    } else {
                        theme::dim()
                    };
                    Span::styled(format!(" {:>4}", format!("{pct}%")), style)
                }
                _ => Span::raw(" ".repeat(5)),
            },
            Col::Spark => Span::styled(format!(" {}", spark(&a.spark, spark_w)), Style::default().fg(theme::agent_color(a.color))),
        });
    }
    Line::from(spans)
}

fn tokens(n: i64, bold: bool) -> Span<'static> {
    let text = format!(" {:>6}", if n == 0 { "·".to_string() } else { format_tokens(n) });
    Span::styled(text, if bold && n > 0 { theme::bold() } else if n == 0 { theme::dim() } else { Style::default() })
}

/// Dollars, to the cent below $100.
fn cost(usd: f64) -> String {
    if usd < 100.0 { format!("${usd:.2}") } else { format!("${:.0}", usd) }
}

/// `series` (oldest first) squeezed into `width` bars, the newest at the right.
fn spark(series: &[i64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if width == 0 || series.is_empty() {
        return String::new();
    }
    let per = series.len().div_ceil(width);
    let mut buckets: Vec<i64> = series.rchunks(per).map(|c| c.iter().sum()).collect();
    buckets.reverse();
    let max = buckets.iter().copied().max().unwrap_or(0);
    let bars: String = buckets
        .iter()
        .map(|&v| if v == 0 || max == 0 { ' ' } else { BARS[((v * 7 + max - 1) / max).clamp(0, 7) as usize] })
        .collect();
    format!("{bars:>width$}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axel_store::UsageTotals;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn totals(tokens: i64, cost_usd: f64) -> UsageTotals {
        UsageTotals { input_tokens: tokens, cost_usd, turns: 1, ..Default::default() }
    }

    fn data() -> UsageData {
        let mut spark = vec![0; 60];
        spark[59] = 900;
        spark[30] = 300;
        let terra = AgentSpend { name: "terra".into(), last_hour: totals(12_400, 0.4), last_day: totals(310_000, 4.2), total: totals(2_300_000, 31.0), spark, context: Some((150_000, 200_000)), ..Default::default() };
        let opus = AgentSpend { name: "opus".into(), last_hour: totals(0, 0.0), last_day: totals(88_000, 1.1), total: totals(400_000, 6.0), spark: vec![0; 60], ..Default::default() };
        UsageData {
            last_hour: totals(12_400, 0.4),
            last_day: totals(398_000, 5.3),
            total: totals(2_700_000, 37.0),
            agents: vec![terra, opus],
            threads: vec![
                crate::model::ThreadSpend { title: "webhook retries".into(), last_day: totals(300_000, 4.0), total: totals(300_000, 4.0) },
                crate::model::ThreadSpend { title: "review".into(), last_day: totals(98_000, 1.3), total: totals(98_000, 1.3) },
            ],
            limits: vec![],
        }
    }

    fn screen(w: u16, h: u16, d: &UsageData) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, d, f.area())).unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn a_sliver_of_a_pane_is_one_line() {
        assert_eq!(screen(30, 1, &data()), vec!["12k 1h · $5.30 24h"]);
        assert_eq!(screen(18, 6, &data())[0], "12k 1h · $5.30 24h", "too narrow for the box too");
    }

    #[test]
    fn a_small_pane_shows_each_agents_last_hour_and_day() {
        let s = screen(24, 6, &data());
        assert!(s[0].contains("tokens"), "{s:?}");
        assert_eq!(s[2], "│ terra    12k   310k  │");
        assert_eq!(s[3], "│ opus       ·    88k  │", "nothing this hour is a dot");
    }

    #[test]
    fn a_wider_pane_adds_cost_context_and_all_time() {
        let s = screen(60, 8, &data());
        assert!(s[0].contains("$5.30 today"), "{s:?}");
        assert!(s[1].contains("1h") && s[1].contains("24h") && s[1].contains("all") && s[1].contains("ctx"), "labels: {s:?}");
        assert!(s[2].contains("terra") && s[2].contains("2.3M") && s[2].contains("$4.20") && s[2].contains("75%"), "{s:?}");
        assert!(s[4].contains("all") && s[4].contains("2.7M"), "everyone together: {s:?}");
    }

    #[test]
    fn the_widest_pane_draws_the_last_hour() {
        let s = screen(90, 6, &data());
        assert!(s[1].contains("last hour"), "{s:?}");
        assert!(s[2].ends_with("█ │"), "the newest minute at the right: {s:?}");
    }

    #[test]
    fn a_tall_pane_lists_todays_threads() {
        let s = screen(40, 10, &data());
        let at = s.iter().position(|l| l.contains("threads today")).expect("threads section");
        assert!(s[at + 1].contains("webhook retries") && s[at + 1].contains("300k"), "{s:?}");
        assert!(s[at + 2].contains("review"), "{s:?}");
    }

    #[test]
    fn agents_that_dont_fit_are_counted() {
        let mut d = data();
        for i in 0..5 {
            d.agents.push(AgentSpend { name: format!("a{i}"), ..Default::default() });
        }
        let s = screen(30, 6, &d);
        assert_eq!(s[4], "│ +4 more                    │", "{s:?}");
    }

    #[test]
    fn nothing_spent_says_so() {
        assert!(screen(30, 6, &UsageData::default())[1].contains("no tokens spent yet"));
    }

    #[test]
    fn beside_the_composer_its_a_summary_then_a_row_per_agent() {
        // 300px or so wide, as tall as the composer: no box.
        let s = screen(36, 4, &with_limits(data()));
        assert_eq!(s[0], "12k 1h · $5.30 24h · 5h 84%");
        assert!(s[1].starts_with("terra") && s[1].contains("310k"), "{s:?}");
        assert!(s[2].starts_with("opus"), "{s:?}");
        assert_eq!(s[3], "", "the plan limits need both their rows");
        let s = screen(36, 5, &with_limits(data()));
        assert!(s[3].starts_with("claude") && s[3].contains("84%") && s[4].starts_with("codex"), "with room, the limits, without a heading: {s:?}");
    }

    fn with_limits(mut d: UsageData) -> UsageData {
        use axel_store::LimitWindow;
        d.limits = vec![
            PlanRow { harness: "claude".into(), windows: vec![
                LimitRow { window: LimitWindow::FiveHour, used_pct: Some(84.0), resets_in: Some(2 * 3600 + 60) },
                LimitRow { window: LimitWindow::Week, used_pct: Some(9.0), resets_in: Some(5 * 86_400) },
            ] },
            PlanRow { harness: "codex".into(), windows: vec![LimitRow { window: LimitWindow::FiveHour, used_pct: None, resets_in: None }] },
        ];
        d
    }

    #[test]
    fn plan_limits_show_as_much_as_the_width_allows() {
        let d = with_limits(data());
        let wide = screen(60, 12, &d);
        let at = wide.iter().position(|l| l.contains("plan limits")).expect("limits section");
        assert!(wide[at + 1].contains("claude") && wide[at + 1].contains("5h ▰▰▰▰▱  84% ↻2h") && wide[at + 1].contains("wk ▱▱▱▱▱   9% ↻5d"), "{wide:?}");
        assert!(wide[at + 2].contains("codex") && wide[at + 2].contains("5h  new"), "a window that started over: {wide:?}");
        let mid = screen(40, 12, &d);
        let at = mid.iter().position(|l| l.contains("plan limits")).unwrap();
        assert!(mid[at + 1].contains("5h  84% ↻2h") && !mid[at + 1].contains('▰'), "no room for bars: {mid:?}");
        let narrow = screen(26, 12, &d);
        let at = narrow.iter().position(|l| l.contains("plan limits")).unwrap();
        assert!(narrow[at + 1].contains("5h  84%") && !narrow[at + 1].contains('↻'), "{narrow:?}");
        // Too short for the section: left out whole.
        assert!(!screen(60, 7, &d).iter().any(|l| l.contains("plan limits")));
    }

    #[test]
    fn one_line_names_the_fullest_window() {
        assert_eq!(screen(40, 1, &with_limits(data())), vec!["12k 1h · $5.30 24h · 5h 84%"]);
        assert_eq!(screen(22, 1, &with_limits(data())), vec!["12k 1h · $5.30 24h"], "when it fits");
    }

    #[test]
    fn limits_show_before_anything_is_spent() {
        let s = screen(50, 8, &with_limits(UsageData::default()));
        assert!(s[1].contains("no tokens spent yet") && s.iter().any(|l| l.contains("84%")), "{s:?}");
    }

    #[test]
    fn the_sparkline_squeezes_an_hour_into_its_width() {
        let mut s = vec![0; 60];
        s[0] = 10;
        s[59] = 10;
        assert_eq!(spark(&s, 6), "█    █");
        assert_eq!(spark(&[0; 60], 4), "    ");
    }
}
