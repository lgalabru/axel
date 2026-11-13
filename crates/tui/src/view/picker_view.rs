//! Recent threads pane, edge to edge like the prompt above it: a title line,
//! the filter, one row per thread, a help line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::dashboard_view::{plan_thread_cols, render_thread_row};
use super::{field_line, help_line};
use crate::picker::ThreadPicker;
use crate::theme;

const PAD: u16 = 2;

pub fn render(frame: &mut Frame, p: &ThreadPicker, focused: bool, now: i64) {
    let full = frame.area();
    frame.render_widget(Clear, full);
    if full.height < 3 || full.width < 10 {
        return;
    }
    let [top, filter, list, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(full);
    let pad = |r: Rect| Rect { x: r.x + PAD, y: r.y, width: r.width.saturating_sub(PAD * 2), height: r.height };
    let (top, filter, list, bottom) = (pad(top), pad(filter), pad(list), pad(bottom));

    let visible = p.visible();
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("threads", theme::accent_bold()), Span::styled("  recent", theme::dim())])),
        top,
    );
    let (open, settled) = p.counts();
    let count = match (p.filter.is_empty(), settled) {
        (true, 0) => open.to_string(),
        (true, s) => format!("{open} open · {s} settled"),
        (false, _) => format!("{}/{}", visible.len(), p.threads.len()),
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(count, theme::dim())).right_aligned()), top);
    if let Some(n) = &p.notice {
        frame.render_widget(Paragraph::new(Line::from(Span::styled(n.clone(), theme::danger()))), bottom);
    } else if focused {
        frame.render_widget(Paragraph::new(help_line(&p.help())), bottom);
    } else {
        frame.render_widget(Paragraph::new(help_line(&[("↓", "from the prompt"), ("click", "to pick")])), bottom);
    }

    // Filter row.
    let (shown, col) = p.filter.view(filter.width.saturating_sub(2) as usize);
    let mut line = field_line(&shown, "type to filter", focused);
    line.spans.insert(0, Span::styled("› ", if focused { theme::accent() } else { theme::dim() }));
    frame.render_widget(Paragraph::new(line), filter);
    if focused {
        frame.set_cursor_position(Position::new(filter.x + 2 + col, filter.y));
    }

    if list.height == 0 {
        return;
    }
    if visible.is_empty() {
        let msg = if p.threads.is_empty() { "No open threads yet." } else { "No thread matches." };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(msg, theme::dim()))), list);
        return;
    }
    let cols = plan_thread_cols(list.width as usize);
    let rows = list.height as usize;
    let first = p.sel.saturating_sub(rows.saturating_sub(1));
    for (n, &i) in visible.iter().enumerate().skip(first).take(rows) {
        let row = Rect::new(list.x, list.y + (n - first) as u16, list.width, 1);
        let t = &p.threads[i];
        render_thread_row(frame, t, row, &cols, focused && n == p.sel, now);
        if t.settled {
            // The settled shelf: a check instead of the live glyph, the row dimmed.
            let buf = frame.buffer_mut();
            for x in row.x..row.x + row.width {
                let cell = &mut buf[(x, row.y)];
                cell.set_fg(theme::DIM);
            }
            buf[(row.x, row.y)].set_symbol("✓");
        }
    }
}
