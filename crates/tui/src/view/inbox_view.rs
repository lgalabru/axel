//! Inbox rendering: pending requests, live statuses, recent messages.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Padding, Paragraph};

use super::help_line;
use crate::inbox::{Focus, Inbox};
use crate::model::RequestKind;
use crate::theme;
use crate::util::{ellipsize, relative_time};

pub fn render(frame: &mut Frame, ib: &Inbox, now: i64) {
    let area = frame.area();
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let n_req = ib.data.requests.len() as u16;
    let n_status = ib.data.statuses.len() as u16;
    let req_h = (n_req.max(1) + 2).min(body.height / 2).max(3);
    let status_h = (n_status.max(1) + 2).min(body.height.saturating_sub(req_h) / 2).max(3);
    let [req_area, status_area, msg_area] =
        Layout::vertical([Constraint::Length(req_h), Constraint::Length(status_h), Constraint::Min(3)]).areas(body);

    render_requests(frame, ib, req_area, now);
    render_statuses(frame, ib, status_area, now);
    render_messages(frame, ib, msg_area, now);

    let mut help = help_line(&ib.help());
    if let Some((msg, _)) = &ib.flash {
        help = Line::from(vec![Span::styled(format!(" {msg}"), theme::accent())]);
    }
    frame.render_widget(Paragraph::new(help), footer);
}

fn panel<'a>(title: &'a str, count: Option<String>, focused: bool) -> Block<'a> {
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { theme::accent() } else { theme::dim() })
        .title(Line::from(vec![Span::raw(" "), Span::styled(title, theme::accent_bold()), Span::raw(" ")]))
        .padding(Padding::horizontal(1));
    if let Some(c) = count {
        b = b.title_top(Line::from(Span::styled(format!(" {c} "), theme::dim())).right_aligned());
    }
    b
}

fn render_requests(frame: &mut Frame, ib: &Inbox, area: Rect, now: i64) {
    let block = panel("pending", Some(ib.data.requests.len().to_string()), ib.focus == Focus::Requests);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if ib.data.requests.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("Nothing is waiting on you.", theme::dim()))), inner);
        return;
    }
    let width = inner.width as usize;
    let items: Vec<ListItem> = ib
        .data
        .requests
        .iter()
        .map(|r| {
            let (glyph, style) = match r.kind {
                RequestKind::Permission => ("!", Style::default().fg(theme::DANGER).bold()),
                RequestKind::Question => ("?", Style::default().fg(ratatui::style::Color::Yellow).bold()),
            };
            let age = relative_time(r.created_at, now);
            let thread = r.thread_title.clone().unwrap_or_default();
            let head = format!("{} {} · {}", r.agent_name, r.kind.label(), thread);
            let head_len = head.chars().count() + 2 + age.chars().count() + 2;
            let mut spans = vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(r.agent_name.clone(), theme::bold()),
                Span::styled(format!(" {}", r.kind.label()), theme::dim()),
            ];
            if !thread.is_empty() {
                spans.push(Span::styled(format!(" · {}", ellipsize(&thread, 30)), theme::dim()));
            }
            let pad = width.saturating_sub(head_len.min(width));
            let content = ellipsize(&r.content.replace('\n', " "), pad.saturating_sub(3));
            spans.push(Span::raw("  "));
            spans.push(Span::raw(content));
            let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let fill = width.saturating_sub(used + age.chars().count());
            spans.push(Span::raw(" ".repeat(fill)));
            spans.push(Span::styled(age, theme::dim()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items).highlight_style(theme::selected()).highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(ib.request_sel));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_statuses(frame: &mut Frame, ib: &Inbox, area: Rect, now: i64) {
    let block = panel("threads", Some(ib.data.statuses.len().to_string()), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if ib.data.statuses.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("No live threads.", theme::dim()))), inner);
        return;
    }
    let width = inner.width as usize;
    let lines: Vec<Line> = ib
        .data
        .statuses
        .iter()
        .take(inner.height as usize)
        .map(|s| {
            let age = relative_time(s.updated_at, now);
            let mut spans = vec![
                Span::styled(format!("{} ", s.glyph()), Style::default().fg(theme::state_color(s.state))),
                Span::styled(format!("{:<10}", ellipsize(&s.agent_name, 10)), theme::bold()),
                Span::raw(" "),
                Span::raw(ellipsize(&s.thread_title, 28)),
            ];
            if let Some(m) = &s.msg {
                let used: usize = spans.iter().map(|x| s_len(x)).sum();
                spans.push(Span::styled(format!("  {}", ellipsize(m, width.saturating_sub(used + age.len() + 4))), theme::dim()));
            }
            let used: usize = spans.iter().map(s_len).sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + age.chars().count()))));
            spans.push(Span::styled(age, theme::dim()));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn s_len(s: &Span) -> usize {
    s.content.chars().count()
}

fn render_messages(frame: &mut Frame, ib: &Inbox, area: Rect, now: i64) {
    let count = if ib.data.unread > 0 {
        format!("{} unread", ib.data.unread)
    } else {
        ib.data.messages.len().to_string()
    };
    let block = panel("messages", Some(count), ib.focus == Focus::Messages);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if ib.data.messages.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(Span::styled("No messages yet.", theme::dim()))), inner);
        return;
    }
    let width = inner.width as usize;
    let items: Vec<ListItem> = ib
        .data
        .messages
        .iter()
        .map(|m| {
            let age = relative_time(m.created_at as i64, now);
            let mut spans = vec![
                Span::styled(format!("{:<10}", ellipsize(&m.from_name, 10)), theme::bold()),
                Span::styled(format!(" {:<10}", ellipsize(&m.kind_label, 10)), theme::dim()),
                Span::raw(" "),
            ];
            let used: usize = spans.iter().map(s_len).sum();
            let room = width.saturating_sub(used + age.chars().count() + 2);
            spans.push(Span::raw(ellipsize(&m.content.replace('\n', " "), room)));
            let used: usize = spans.iter().map(s_len).sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + age.chars().count()))));
            spans.push(Span::styled(age, theme::dim()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items).highlight_style(if ib.focus == Focus::Messages { theme::selected() } else { Style::default() });
    let mut state = ListState::default().with_selected(Some(ib.message_sel));
    frame.render_stateful_widget(list, inner, &mut state);
}
