//! The usage pane: what each agent spent, laid out for the room it has.
//!
//! One row per agent. Columns join as the pane widens, most needed first:
//! the last 24 hours, a chart of the last hour, today's cost, then how full
//! its context is. The chart takes what's left, up to a bar a minute (two
//! to a cell, in braille), on one scale for every agent so their bars
//! compare. Rows join as it grows taller: column labels, a line for
//! everyone together, each harness's plan limits (how much of its five-hour
//! and weekly windows the account has used, with a bar and the time to the
//! reset as width allows), then the threads that spent today. Under
//! [`BOX_MIN_HEIGHT`] rows it's the same table without the box or threads,
//! the plan limits folded onto one line. One row high, or under
//! [`BOX_MIN_WIDTH`] columns, it's one line: the last hour's tokens, today's
//! cost and the fullest plan window.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::{AgentSpend, LimitRow, PlanRow, UsageData};
use crate::theme;
use crate::util::{ellipsize, format_tokens};

/// Below this many rows there's no box: a summary line, then a row per
/// agent (one row is the summary alone).
pub const BOX_MIN_HEIGHT: u16 = 6;
/// …or below this many columns.
pub const BOX_MIN_WIDTH: u16 = 20;
/// Narrowest chart worth drawing.
const SPARK_MIN: usize = 6;
/// Minutes in the chart: at most a bar each, two bars to a cell.
const SPARK_MINUTES: usize = 60;
/// Widest an agent's name gets.
const NAME_MAX: usize = 10;

/// A column after the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Day,
    DayCost,
    Context,
    Spark,
}

/// The order columns join in as the pane widens.
const BY_NEED: [Col; 4] = [Col::Day, Col::Spark, Col::DayCost, Col::Context];
/// The order they're shown in.
const SHOWN: [Col; 4] = [Col::Day, Col::DayCost, Col::Context, Col::Spark];

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Day => "24h",
            Col::DayCost => "$ 24h",
            Col::Context => "ctx",
            Col::Spark => "last hour",
        }
    }

    /// Its width; the sparkline's is its narrowest.
    fn width(self) -> usize {
        match self {
            Col::Day => 6,
            Col::DayCost => 7,
            Col::Context => 4,
            Col::Spark => SPARK_MIN,
        }
    }
}

/// The columns that fit in `width` after a name of `name` columns, in the
/// order shown, and the chart's width (0 when there's none): what's left,
/// up to a bar a minute.
fn columns(width: usize, name: usize) -> (Vec<Col>, usize) {
    let mut used = name;
    let mut chosen = Vec::new();
    for c in BY_NEED {
        if used + 1 + c.width() <= width {
            used += 1 + c.width();
            chosen.push(c);
        }
    }
    let spark = if chosen.contains(&Col::Spark) {
        (width - used + SPARK_MIN).min(SPARK_MINUTES / 2)
    } else {
        0
    };
    (SHOWN.into_iter().filter(|c| chosen.contains(c)).collect(), spark)
}

pub fn render(frame: &mut Frame, data: &UsageData, area: Rect) {
    if area.height == 1 || area.width < BOX_MIN_WIDTH {
        frame.render_widget(Paragraph::new(summary(data, area.width)), area);
        return;
    }
    // Short (beside the composer): no box, the labelled table, then the
    // plan limits with rows to spare.
    if area.height < BOX_MIN_HEIGHT {
        frame.render_widget(Paragraph::new(lines(data, area.width as usize, area.height as usize, true)), area);
        return;
    }
    // No box of its own: the Rex pane is the frame. A header line, "tokens"
    // at the left and today's cost at the right, then the table, one column in.
    let inner = Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height };
    let today = format!("{} today", cost(data.last_day.cost_usd));
    let mut header = vec![Span::styled("tokens", theme::accent_bold())];
    let room = usize::from(inner.width);
    if room >= "tokens".len() + today.len() + 2 {
        header.push(Span::raw(" ".repeat(room - "tokens".len() - today.len())));
        header.push(Span::styled(today, theme::dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), Rect { height: 1, ..inner });
    let body = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    frame.render_widget(Paragraph::new(lines(data, body.width as usize, body.height as usize, false)), body);
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

/// The pane's lines for an inside of `width` × `rows`. `tight` (beside the
/// composer): the plan limits on one line (the label row's end, else a row
/// of their own), no threads.
fn lines(data: &UsageData, width: usize, rows: usize, tight: bool) -> Vec<Line<'static>> {
    if data.agents.is_empty() {
        let mut out = vec![Line::styled("no tokens spent yet", theme::dim())];
        if tight {
            if let Some(limits) = folded_limits(&data.limits, width).filter(|_| rows > 1) {
                out.push(Line::from(limits));
            }
        } else {
            let name_w = data.limits.iter().map(|p| p.harness.chars().count()).max().unwrap_or(0).min(NAME_MAX);
            plans(&mut out, &data.limits, width, rows.saturating_sub(1), name_w);
        }
        return out;
    }
    // Harnesses name rows only when the plan limits get rows of their own.
    let harnesses = data.limits.iter().filter(|_| !tight).map(|p| p.harness.chars().count());
    let longest = data.agents.iter().map(|a| a.name.chars().count()).chain(harnesses).max().unwrap_or(0);
    let name_w = longest.clamp(3, NAME_MAX).min(width.saturating_sub(7).max(3));
    let (cols, spark_w) = columns(width, name_w);
    let n = data.agents.len();
    let mut out = Vec::new();
    // Labels when there's a row to spare for them.
    let labels = rows > n && !cols.is_empty();
    let mut folded = false;
    if labels {
        let mut spans = vec![Span::raw(" ".repeat(name_w))];
        for &c in &cols {
            let w = if c == Col::Spark { spark_w } else { c.width() };
            let text = match c {
                Col::Spark if w < c.label().len() => format!(" {:<w$}", "1h"),
                Col::Spark => format!(" {:<w$}", c.label()),
                _ => format!(" {:>w$}", c.label()),
            };
            spans.push(Span::styled(text, theme::dim()));
        }
        // Beside the composer the plan limits ride on the label row, right
        // of the labels, when they fit.
        if tight {
            let used = Line::from(spans.clone()).width();
            if let Some(limits) = folded_limits(&data.limits, width.saturating_sub(used + 2)) {
                let gap = width - used - Line::from(limits.clone()).width();
                spans.push(Span::raw(" ".repeat(gap)));
                spans.extend(limits);
                folded = true;
            }
        }
        out.push(Line::from(spans));
    }
    // One scale for every agent's chart.
    let spark_max = data.agents.iter().map(|a| super::chart::peak(&hour(&a.spark), spark_w)).max().unwrap_or(0);
    let room = rows - usize::from(labels);
    // Agents first; one that doesn't fit is counted on the last row.
    let shown = if n > room { room.saturating_sub(1) } else { n };
    for a in &data.agents[..shown] {
        out.push(agent_line(a, &cols, name_w, spark_w, spark_max));
    }
    if shown < n {
        out.push(Line::styled(format!("+{} more", n - shown), theme::dim()));
        return out;
    }
    let mut left = room - shown;
    let all_row = (!tight && n > 1).then(|| {
        let all = AgentSpend {
            name: "all".into(),
            last_hour: data.last_hour.clone(),
            last_day: data.last_day.clone(),
            total: data.total.clone(),
            ..Default::default()
        };
        let mut line = agent_line(&all, &cols, name_w, 0, 0);
        line.spans.iter_mut().for_each(|s| s.style = s.style.add_modifier(Modifier::DIM));
        line
    });
    // The plan limits: a section of their own on a tall pane (a gap, a
    // heading, a row per harness), else one centered strip under the agents,
    // a blank line before it when there's a row to spare.  They come before
    // the "all" row: knowing a window is used up matters more than the sum.
    let section_rows = data.limits.len() + 2;
    let full = !tight && !data.limits.is_empty() && left >= section_rows + usize::from(all_row.is_some());
    if full {
        if let Some(all) = all_row {
            out.push(all);
            left -= 1;
        }
        left -= plans(&mut out, &data.limits, width, left, name_w);
    } else {
        let strip = (!folded).then(|| limits_strip(&data.limits, width)).flatten();
        match (strip, all_row) {
            (Some(strip), Some(all)) if left >= 3 => {
                out.push(all);
                out.push(Line::raw(""));
                out.push(strip);
                left -= 3;
            }
            (Some(strip), _) if left >= 2 => {
                out.push(Line::raw(""));
                out.push(strip);
                left -= 2;
            }
            (Some(strip), _) if left >= 1 => {
                out.push(strip);
                left -= 1;
            }
            (None, Some(all)) if left >= 1 => {
                out.push(all);
                left -= 1;
            }
            _ => {}
        }
    }
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

/// The plan limits section (a gap, a label, a row per harness) when it all
/// fits in `left` rows; the rows it took.
fn plans(out: &mut Vec<Line<'static>>, plans: &[PlanRow], width: usize, left: usize, name_w: usize) -> usize {
    const HEADING: usize = 2;
    if plans.is_empty() || left < plans.len() + HEADING {
        return 0;
    }
    out.push(Line::raw(""));
    out.push(Line::styled("plan limits", theme::dim()));
    for p in plans {
        out.push(plan_line(p, width, name_w));
    }
    plans.len() + HEADING
}

/// Every plan window on one centered line: each with as much detail as the
/// width allows (`5h ▰▰▰▰▱ 84% ↻2h   wk ▱▱▱▱▱ 9% ↻5d`), the harness named
/// when there's more than one; else the fullest window alone.
fn limits_strip(plans: &[PlanRow], width: usize) -> Option<Line<'static>> {
    if plans.is_empty() {
        return None;
    }
    let centered = |mut spans: Vec<Span<'static>>| -> Option<Line<'static>> {
        // The window spans start with two spaces of their own.
        if let Some(first) = spans.first_mut()
            && let Some(rest) = first.content.strip_prefix("  ")
        {
            first.content = rest.to_string().into();
        }
        let w = Line::from(spans.clone()).width();
        if w > width {
            return None;
        }
        spans.insert(0, Span::raw(" ".repeat((width - w) / 2)));
        Some(Line::from(spans))
    };
    for detail in [Detail::Bar, Detail::Reset] {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, p) in plans.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·", theme::dim()));
            }
            if plans.len() > 1 {
                spans.push(Span::styled(format!("  {}", p.harness), theme::dim()));
            }
            for w in &p.windows {
                spans.extend(window_spans(w, detail));
            }
        }
        if let Some(line) = centered(spans) {
            return Some(line);
        }
    }
    // Shares alone, packed tight; else the fullest window with its reset.
    if let Some(folded) = folded_limits(plans, width) {
        let w = Line::from(folded.clone()).width();
        let mut spans = vec![Span::raw(" ".repeat((width - w) / 2))];
        spans.extend(folded);
        return Some(Line::from(spans));
    }
    None
}

/// Every plan window on one line in `width` (`5h 31%  wk 12%`, each
/// harness named when there's more than one), else the fullest alone.
fn folded_limits(plans: &[PlanRow], width: usize) -> Option<Vec<Span<'static>>> {
    let window = |w: &LimitRow| -> Vec<Span<'static>> {
        match w.used_pct {
            Some(pct) => vec![Span::styled(format!("{} ", w.window.short()), theme::dim()), Span::styled(format!("{pct:.0}%"), pct_style(pct))],
            None => vec![Span::styled(format!("{} new", w.window.short()), theme::dim())],
        }
    };
    let mut all = Vec::new();
    for p in plans {
        if !all.is_empty() {
            all.push(Span::styled(" · ", theme::dim()));
        }
        if plans.len() > 1 {
            all.push(Span::styled(format!("{} ", p.harness), theme::dim()));
        }
        for (i, w) in p.windows.iter().enumerate() {
            if i > 0 {
                all.push(Span::raw("  "));
            }
            all.extend(window(w));
        }
    }
    if !all.is_empty() && Line::from(all.clone()).width() <= width {
        return Some(all);
    }
    let fullest = plans.iter().flat_map(|p| &p.windows).filter(|w| w.used_pct.is_some()).max_by(|a, b| a.used_pct.unwrap_or(0.0).total_cmp(&b.used_pct.unwrap_or(0.0)))?;
    let one = window(fullest);
    (Line::from(one.clone()).width() <= width).then_some(one)
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
    // A window used up stops every agent on that plan: when it comes back is
    // the one thing worth knowing, so it stays even when nothing else fits.
    if (detail != Detail::Share || pct >= 100.0)
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

fn agent_line(a: &AgentSpend, cols: &[Col], name_w: usize, spark_w: usize, spark_max: i64) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{:<name_w$}", ellipsize(&a.name, name_w)), Style::default().fg(theme::agent_color(a.color)))];
    for &c in cols {
        if c == Col::Spark {
            spans.push(Span::raw(" "));
            spans.extend(super::chart::braille(&hour(&a.spark), spark_w, spark_max, Style::default().fg(theme::agent_color(a.color))));
            continue;
        }
        spans.push(match c {
            Col::Day => tokens(a.last_day.tokens(), true),
            Col::DayCost => Span::styled(format!(" {:>7}", cost(a.last_day.cost_usd)), theme::dim()),
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
            Col::Spark => unreachable!("drawn above"),
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

/// The last hour, a minute a sample: a short series is quiet minutes before.
fn hour(series: &[i64]) -> Vec<i64> {
    let mut h = vec![0; SPARK_MINUTES.saturating_sub(series.len())];
    h.extend(series.iter().rev().take(SPARK_MINUTES).rev());
    h
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
    fn a_small_pane_shows_each_agents_day_and_hour() {
        let s = screen(24, 6, &data());
        assert_eq!(s[0], " tokens     $5.30 today", "no box: a header line, today's cost at its right");
        assert_eq!(s[1], "          24h last hour");
        assert_eq!(s[2], " terra   310k ⣀⣀⣀⣀⣠⣀⣀⣀⣸", "two more columns than the box left, so a wider chart");
        assert_eq!(s[3], " opus     88k ⣀⣀⣀⣀⣀⣀⣀⣀⣀", "a quiet hour is a dotted line");
        assert_eq!(screen(20, 6, &data())[0], " tokens", "too narrow for the cost: the header waits for room");
    }

    #[test]
    fn a_wider_pane_adds_cost_and_context() {
        let s = screen(90, 8, &data());
        assert!(s[0].contains("$5.30 today"), "{s:?}");
        assert!(s[1].contains("24h") && s[1].contains("$ 24h") && s[1].contains("ctx") && s[1].contains("last hour") && !s[1].contains(" all"), "labels: {s:?}");
        assert!(s[2].contains("terra") && s[2].contains("310k") && s[2].contains("$4.20") && s[2].contains("75%"), "{s:?}");
        assert!(s[4].contains("all") && s[4].contains("398k"), "everyone together: {s:?}");
    }

    #[test]
    fn the_chart_starts_under_its_label() {
        let s = screen(90, 6, &data());
        let at = s[1].find("last hour").expect("label");
        assert_eq!(s[2][at..].chars().next(), Some('⣀'), "{s:?}");
        assert!(s[2].trim_end_matches(['│', ' ']).ends_with('⣸'), "the newest minute at the chart's right: {s:?}");
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
        assert_eq!(s[5], " +3 more", "{s:?}");
    }

    #[test]
    fn a_short_pane_puts_the_limits_on_one_centered_strip_after_a_gap() {
        let d = with_limits(data());
        // Header, labels, two agents, a blank line, the strip: no room for the
        // "all" row or the section heading.
        let s = screen(60, 6, &d);
        assert_eq!(s[4], "", "a gap after the last agent: {s:?}");
        assert!(s[5].contains("5h  84% ↻2h") && s[5].contains("wk   9% ↻5d") && s[5].contains("codex"), "every window with its reset: {s:?}");
        let lead = s[5].chars().take_while(|c| *c == ' ').count();
        assert!(lead >= 2, "centered: {:?}", s[5]);
        // Wider: the bars come too.
        assert!(screen(80, 6, &d)[5].contains("5h ▰▰▰▰▱  84% ↻2h"), "{:?}", screen(80, 6, &d)[5]);
        // One row more: the "all" row comes back above the gap.
        let s = screen(60, 7, &d);
        assert!(s[4].trim_start().starts_with("all") && s[5].is_empty() && s[6].contains("84%"), "{s:?}");
        // One row less: the strip alone, no gap.
        let s = screen(60, 5, &d);
        assert!(s[4].contains("84%") && !s.iter().any(|l| l.trim_start().starts_with("all")), "{s:?}");
        // Tall: the section with its heading, as before.
        assert!(screen(60, 12, &d).iter().any(|l| l.contains("plan limits")));
    }

    #[test]
    fn nothing_spent_says_so() {
        assert!(screen(30, 6, &UsageData::default())[1].contains("no tokens spent yet"));
    }

    #[test]
    fn beside_the_composer_its_a_labelled_table() {
        // 300px or so wide, as tall as the composer: no box, no summary line.
        let s = screen(36, 4, &with_limits(data()));
        assert_eq!(s[0], "         24h   $ 24h  ctx last hour", "every number has a label");
        assert_eq!(s[1], "terra   310k   $4.20  75% ⣀⣀⣀⣀⣀⣄⣀⣀⣀⣸");
        assert!(s[2].starts_with("opus"), "{s:?}");
        assert_eq!(s[3], "claude 5h 84%  wk 9% · codex 5h new", "the plan limits on one line: {s:?}");
        let s = screen(150, 3, &with_limits(data()));
        assert!(s[0].ends_with("claude 5h 84%  wk 9% · codex 5h new"), "on the label row when it fits: {s:?}");
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
        // Used up: the reset time stays however narrow the pane.
        let mut gone = d.clone();
        gone.limits[0].windows[0].used_pct = Some(100.0);
        let narrow = screen(26, 12, &gone);
        let at = narrow.iter().position(|l| l.contains("plan limits")).unwrap();
        assert!(narrow[at + 1].contains("5h 100% ↻2h"), "{narrow:?}");
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
    fn the_chart_takes_whats_left_up_to_a_bar_a_minute() {
        assert_eq!(columns(200, 6).1, 30, "a bar a minute, two to a cell, no wider");
        assert_eq!(columns(48, 5).1, 22, "otherwise all that's left");
    }

    #[test]
    fn every_agent_is_charted_on_one_scale() {
        let mut d = data();
        d.agents[1].spark[59] = 9_000;
        let s = screen(60, 6, &d);
        assert!(s[3].contains('⣸') && !s[2].contains('⣸'), "the busier agent sets the scale: {s:?}");
    }
}
