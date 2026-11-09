use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{DiffView, Draft, Layout, LineKind, Side, paint};
use crate::theme;

struct Row {
    file: usize,
    header: bool,
    line: Option<Line<'static>>,
    code: Option<super::SideRow>,
    sources: Vec<usize>,
}

fn draw(frame: &mut Frame, x: u16, y: u16, width: u16, line: Line<'_>) {
    if width > 0 { frame.render_widget(Paragraph::new(line), Rect::new(x, y, width, 1)); }
}

fn plain(text: &str, width: usize, style: Style) -> Vec<Span<'static>> {
    paint::code(text, &[], &[], 0, width, style, None)
}

fn tail(text: &str, width: usize) -> String {
    if text.width() <= width { return text.to_string() }
    let mut end = String::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width { break }
        end.insert(0, c);
        used += w;
    }
    format!("…{end}")
}

fn sidebar(frame: &mut Frame, v: &mut DiffView, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let mut rows = Vec::new();
    let mut parent = String::new();
    let grouped = v.data.files.iter().any(|f| f.authored) && v.data.files.iter().any(|f| !f.authored);
    let mut others = false;
    for i in v.file_order() {
        let file = &v.data.files[i];
        if grouped && !file.authored && !others {
            rows.push((None, Line::from(plain("also changed", width, Style::default().fg(v.palette.muted)))));
            others = true;
            parent.clear();
        }
        let (dir, name) = file.label.rsplit_once('/').unwrap_or(("", &file.label));
        if dir != parent {
            rows.push((None, Line::from(plain(&tail(&format!("{dir}/"), width), width, Style::default().fg(v.palette.muted)))));
            parent = dir.into();
        }
        let selected = i == v.file;
        let style = Style::default().fg(v.palette.text).bg(if selected { v.palette.panel } else { Color::Reset });
        let stats = match (file.added, file.removed) {
            (Some(a), Some(r)) => format!("+{a} −{r}"), _ => "binary".into(),
        };
        let notes = v.drafts.iter().filter(|d| d.path == file.path).count();
        let suffix = if notes > 0 { format!(" ✎{notes} {stats}") } else { format!(" {stats}") };
        let label_width = width.saturating_sub(suffix.width() + 1);
        let mut spans = vec![Span::styled(if selected { "▎" } else { " " }, theme::accent().bg(style.bg.unwrap_or(Color::Reset)))];
        spans.extend(plain(&format!("{} {}", file.status, tail(name, label_width.saturating_sub(2))), label_width, style));
        spans.extend(plain(&suffix, width.saturating_sub(label_width + 1), style.fg(v.palette.muted)));
        rows.push((Some(i), Line::from(spans)));
    }
    let selected = rows.iter().position(|(i, _)| *i == Some(v.file)).unwrap_or(0);
    let first = selected.saturating_sub(area.height.saturating_sub(3) as usize);
    for (n, (file, line)) in rows.into_iter().skip(first).take(area.height as usize).enumerate() {
        draw(frame, area.x + 1, area.y + n as u16, area.width.saturating_sub(2), line);
        if let Some(i) = file { v.hits.push((Rect::new(area.x, area.y + n as u16, area.width.saturating_sub(1), 1), super::Hit::File(i))); }
    }
    for y in area.y..area.bottom() {
        draw(frame, area.right() - 1, y, 1, Line::styled("│", Style::default().fg(v.palette.border)));
    }
}

fn cell(v: &DiffView, file: usize, idx: Option<usize>, side: Side, width: usize, gutter: usize, unified: bool) -> Vec<Span<'static>> {
    let Some(i) = idx else { return plain("", width, Style::default()) };
    let line = &v.lines_for(file)[i];
    let color = match line.kind { LineKind::Add => v.palette.add_sign, LineKind::Del => v.palette.remove_sign, _ => v.palette.muted };
    let current = file == v.file && i == v.cursor && (unified || side == v.side || line.kind == LineKind::Hunk);
    let numbered = if current { theme::accent().add_modifier(Modifier::BOLD) } else { Style::default().fg(color) };
    let no = |n: Option<usize>| n.map(|n| format!("{n:>gutter$}")).unwrap_or_else(|| " ".repeat(gutter));
    let numbers = if unified { format!("{} {}", no(line.old), no(line.new)) } else { no(if side == Side::Old { line.old } else { line.new }) };
    let sign = match line.kind { LineKind::Add => '+', LineKind::Del => '−', _ => ' ' };
    if width <= numbers.width() + 4 { return plain(&sign.to_string(), width, numbered) }
    let mut spans = vec![Span::styled(if matches!(line.kind, LineKind::Add | LineKind::Del) { "▎" } else { " " }, Style::default().fg(color).bg(v.palette.row(line.kind))), Span::styled(numbers.clone(), numbered.bg(v.palette.row(line.kind))), Span::styled(format!(" {sign} "), Style::default().fg(color).bg(v.palette.row(line.kind)))];
    let body_width = width.saturating_sub(numbers.width() + 4);
    let mut base = Style::default().fg(v.palette.text).bg(v.palette.row(line.kind));
    if file == v.file && v.marked().is_some_and(|(a, b)| (a..=b).contains(&i)) { base = base.add_modifier(Modifier::REVERSED); }
    let path = v.data.files.get(file).map(|f| f.path.as_str()).unwrap_or("");
    let syntax = v.data.syntax.get(path).and_then(|lines| lines.get(i)).map(Vec::as_slice).unwrap_or(&[]);
    let changes = v.emphasis.get(path).and_then(|lines| lines.get(i)).map(Vec::as_slice).unwrap_or(&[]);
    spans.extend(paint::code(&line.text, syntax, changes, v.horizontal, body_width, base, Some((&v.palette, line.kind))));
    spans
}

fn note_lines(draft: &Draft, width: usize) -> Vec<Line<'static>> {
    if width < 4 { return vec![Line::from(plain("✎", width, theme::accent()))] }
    let side = if draft.side == Side::Old { "old" } else { "new" };
    let range = if draft.start == draft.end { draft.start.to_string() } else { format!("{}–{}", draft.start, draft.end) };
    let mut lines = vec![Line::from(plain(&format!("╭ ✎ note · {side} L{range}"), width, theme::accent()))];
    let room = width.saturating_sub(3).max(1);
    for paragraph in draft.note.lines() {
        let (mut text, mut used) = (String::new(), 0);
        for word in paragraph.split_inclusive(char::is_whitespace) {
            if used > 0 && used + word.width() > room {
                let mut spans = vec![Span::styled("│ ", theme::accent())];
                spans.extend(plain(text.trim_end(), width.saturating_sub(2), Style::default()));
                lines.push(Line::from(spans));
                text.clear();
                used = 0;
            }
            for c in word.chars() {
                let c = if c.is_control() { ' ' } else { c };
                let w = c.width().unwrap_or(0);
                if used + w > room {
                    let mut spans = vec![Span::styled("│ ", theme::accent())];
                    spans.extend(plain(&text, width.saturating_sub(2), Style::default()));
                    lines.push(Line::from(spans));
                    text.clear();
                    used = 0;
                }
                text.push(c);
                used += w;
            }
        }
        let mut spans = vec![Span::styled("│ ", theme::accent())];
        spans.extend(plain(text.trim_end(), width.saturating_sub(2), Style::default()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(plain("╰─", width, theme::accent())));
    lines
}

fn code_row(v: &DiffView, file: usize, row: super::SideRow, width: usize) -> Line<'static> {
    let lines = v.lines_for(file);
    let gutter = lines.iter().filter_map(|l| l.new.or(l.old)).max().unwrap_or(0).to_string().len().max(2);
    let split = v.layout() == Layout::SideBySide;
    let left_width = width.saturating_sub(1) / 2;
    let right_width = width.saturating_sub(left_width + 1);
    let structural = row.full.filter(|&i| matches!(lines[i].kind, LineKind::Hunk | LineKind::Meta));
    let spans = if let Some(i) = structural {
            let text = if lines[i].kind == LineKind::Hunk {
                let context = lines[i].text.split("@@").nth(2).unwrap_or("").trim();
                if context.is_empty() { format!("  {}", lines[i].text) } else { format!("  {context}") }
            } else { lines[i].text.clone() };
            plain(&text, width, theme::dim())
        } else if split {
            let mut spans = cell(v, file, row.left, Side::Old, left_width, gutter, false);
            spans.push(Span::styled("│", Style::default().fg(v.palette.border)));
            spans.extend(cell(v, file, row.right, Side::New, right_width, gutter, false));
            spans
        } else {
            cell(v, file, row.full, Side::New, width, gutter, true)
        };
    Line::from(spans)
}

fn rows(v: &DiffView, file: usize, width: usize) -> Vec<Row> {
    let lines = v.lines_for(file);
    let split = v.layout() == Layout::SideBySide;
    let left_width = width.saturating_sub(1) / 2;
    let right_width = width.saturating_sub(left_width + 1);
    let mut out = Vec::new();
    let drafts: Vec<_> = v.drafts.iter().filter(|d| d.path == v.data.files[file].path).collect();
    let source_rows = if split { super::side_rows(lines) } else { (0..lines.len()).map(|i| super::SideRow { full: Some(i), ..Default::default() }).collect() };
    for row in source_rows {
        let sources: Vec<_> = [row.left, row.right, row.full].into_iter().flatten().collect();
        out.push(Row { file, header: false, line: None, code: Some(row), sources: sources.clone() });
        let mut left_notes = Vec::new();
        let mut right_notes = Vec::new();
        for d in drafts.iter().filter(|d| sources.iter().any(|&i| super::covers(d, &lines[i]) && Some(d.end) == if d.side == Side::Old { lines[i].old } else { lines[i].new })) {
            if !split {
                out.extend(note_lines(d, width).into_iter().map(|line| Row { file, header: false, line: Some(line), code: None, sources: Vec::new() }));
            } else if d.side == Side::Old {
                left_notes.extend(note_lines(d, left_width));
            } else {
                right_notes.extend(note_lines(d, right_width));
            }
        }
        if split {
            for i in 0..left_notes.len().max(right_notes.len()) {
                let mut spans = left_notes.get(i).map(|l| l.spans.clone()).unwrap_or_else(|| plain("", left_width, Style::default()));
                spans.push(Span::styled("│", Style::default().fg(v.palette.border)));
                spans.extend(right_notes.get(i).map(|l| l.spans.clone()).unwrap_or_else(|| plain("", right_width, Style::default())));
                out.push(Row { file, header: false, line: Some(Line::from(spans)), code: None, sources: Vec::new() });
            }
        }
    }
    out
}

fn file_header(v: &DiffView, file: usize, width: usize) -> Line<'static> {
    let f = &v.data.files[file];
    let counts = match (f.added, f.removed) { (Some(a), Some(r)) => format!("+{a}  −{r}"), _ => "binary".into() };
    let style = Style::default().fg(v.palette.text).bg(v.palette.panel);
    let label_width = width.saturating_sub(counts.width() + 3);
    let mut spans = plain(&format!(" {}", tail(&f.label, label_width.saturating_sub(1))), label_width, style.add_modifier(Modifier::BOLD));
    spans.extend(plain(&format!(" {counts} "), width.saturating_sub(label_width), style));
    Line::from(spans)
}

fn stream(v: &DiffView, width: usize) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    for file in v.file_order() {
        if let Some(previous) = out.last() { let previous_file = previous.file; out.push(Row { file: previous_file, header: false, line: Some(Line::raw("")), code: None, sources: Vec::new() }); }
        out.push(Row { file, header: true, line: Some(file_header(v, file, width)), code: None, sources: Vec::new() });
        let body = rows(v, file, width);
        if body.is_empty() {
            out.push(Row { file, header: false, line: Some(Line::styled("  No text changes", Style::default().fg(v.palette.muted))), code: None, sources: Vec::new() });
        } else { out.extend(body); }
    }
    out
}

pub(super) fn render(frame: &mut Frame, v: &mut DiffView) {
    let area = frame.area();
    v.hits.clear();
    if area.width == 0 || area.height == 0 { return }
    let title = format!("{} · turn {} · {}", v.data.agent, v.data.turn, v.data.headline);
    draw(frame, area.x, area.y, area.width, Line::from(plain(&title, area.width as usize, theme::bold())));
    if area.height < 4 { return }
    let nav_width = if area.width >= 146 && area.height >= 8 { (area.width / 5).clamp(28, 38) } else { 0 };
    let x = area.x + nav_width;
    let width = area.width - nav_width;
    v.last_width = width as usize;
    let planned = stream(v, width as usize);
    let body_height = area.height.saturating_sub(3) as usize;
    v.page = body_height.max(1);
    let cursor = planned.iter().position(|r| r.file == v.file && r.sources.contains(&v.cursor)).unwrap_or_else(|| planned.iter().position(|r| r.file == v.file && r.header).unwrap_or(0));
    if v.jump_file {
        v.scroll = planned.iter().position(|r| r.file == v.file && r.header).unwrap_or(0);
        v.jump_file = false;
    } else if v.follow_cursor {
        if cursor < v.scroll { v.scroll = cursor; }
        if cursor >= v.scroll + v.page { v.scroll = cursor + 1 - v.page; }
    }
    v.scroll = v.scroll.min(planned.len().saturating_sub(1));
    // Scrolling adopts the source at the viewport top so a later note addresses
    // the code being read, rather than an off-screen file.
    if !v.follow_cursor && v.mark.is_none() && v.input.is_none() {
        if let Some(row) = planned.iter().skip(v.scroll).find(|r| r.code.is_some()) {
            v.file = row.file;
            v.cursor = row.code.unwrap().first();
        }
    }
    let pinned = planned.get(v.scroll).map(|r| r.file).unwrap_or(v.file);
    if v.data.files.get(pinned).is_some() {
        draw(frame, x, area.y + 1, width, file_header(v, pinned, width as usize));
    }
    if nav_width > 0 { sidebar(frame, v, Rect::new(area.x, area.y + 2, nav_width, area.height - 3)); }
    for (i, row) in planned.into_iter().skip(v.scroll).take(body_height).enumerate() {
        let y = area.y + 2 + i as u16;
        // The pinned header already labels a file at its initial stream position.
        if row.header && i == 0 { continue }
        if row.header { v.hits.push((Rect::new(x, y, width, 1), super::Hit::File(row.file))); }
        if let Some(code) = row.code {
            if v.layout() == Layout::SideBySide {
                let half = width.saturating_sub(1) / 2;
                for (index, side, rect) in [
                    (code.left.or(code.full), Side::Old, Rect::new(x, y, half, 1)),
                    (code.right.or(code.full), Side::New, Rect::new(x + half + 1, y, width - half - 1, 1)),
                ] {
                    if let Some(index) = index { v.hits.push((rect, super::Hit::Line(row.file, index, side))); }
                }
            } else if let Some(index) = code.full {
                let side = if v.lines_for(row.file)[index].kind == LineKind::Del { Side::Old } else { Side::New };
                v.hits.push((Rect::new(x, y, width, 1), super::Hit::Line(row.file, index, side)));
            }
        }
        let line = row.line.unwrap_or_else(|| code_row(v, row.file, row.code.expect("a review row has code or a note"), width as usize));
        draw(frame, x, y, width, line);
    }
    if v.data.files.is_empty() {
        draw(frame, x, area.y + 2, width, Line::from(plain("No changes in this turn", width as usize, theme::dim())));
    }
    let footer = if let Some(buf) = &v.input {
        format!("note: {buf}▏  Enter saves · Esc cancels")
    } else if v.confirm_undo {
        format!("undo turn {} of {}: {}? y/n", v.data.turn, v.data.agent, v.data.headline)
    } else if let Some(note) = &v.data.note {
        note.clone()
    } else {
        let layout = if v.layout() == Layout::Unified { "s side by side" } else { "s unified" };
        let notes = if v.drafts.is_empty() { String::new() } else { format!("  ⏎ send {} note{}", v.drafts.len(), if v.drafts.len() == 1 { "" } else { "s" }) };
        let undo = if v.data.can_undo { "  u undo" } else { "" };
        if area.width < 90 {
            format!("↑↓ file  j/k line  v mark  c note  s split  q close{notes}")
        } else if area.width < 140 {
            format!("↑↓ files  j/k lines  ←→ pan  c note{notes}  {layout}  q close")
        } else {
            format!("↑↓ file  j/k line  ←→ pan  n/p hunk  v mark  c note{notes}{undo}  {layout}  [ ] turn  q close")
        }
    };
    draw(frame, area.x, area.bottom() - 1, area.width, Line::from(plain(&footer, area.width as usize, if v.input.is_some() || v.confirm_undo { theme::accent() } else { theme::dim() })));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffData, FileEntry, SyntaxSpan, SyntaxTone, parse_patch};
    use ratatui::{Terminal, backend::TestBackend};

    fn review() -> DiffView {
        let patch = "diff --git a/src/retry.rs b/src/retry.rs\n--- a/src/retry.rs\n+++ b/src/retry.rs\n@@ -1,15 +1,17 @@\n use std::time::Duration;\n \n pub struct RetryPolicy {\n     pub attempts: usize,\n     pub delay: Duration,\n }\n \n impl RetryPolicy {\n     pub fn for_endpoint() -> Self {\n         Self {\n-            attempts: 3,\n-            delay: Duration::from_secs(30),\n+            attempts: 5,\n+            delay: Duration::from_secs(2),\n         }\n     }\n+\n+    pub fn exhausted(&self, attempt: usize) -> bool {\n+        attempt >= self.attempts\n+    }\n }\n";
        let mut patches = parse_patch(patch);
        patches.extend(parse_patch("diff --git a/src/client.rs b/src/client.rs\n--- a/src/client.rs\n+++ b/src/client.rs\n@@ -42,7 +42,10 @@ impl Client {\n     pub fn connect(&self) -> Result<Connection> {\n-        connect_once(&self.endpoint)\n+        let policy = RetryPolicy::for_endpoint();\n+        retry(policy, || connect_once(&self.endpoint))\n     }\n }\ndiff --git a/tests/retry.rs b/tests/retry.rs\nnew file mode 100644\n--- /dev/null\n+++ b/tests/retry.rs\n@@ -0,0 +1,9 @@\n+#[test]\n+fn retry_policy_has_a_bounded_delay() {\n+    let policy = RetryPolicy::for_endpoint();\n+    assert_eq!(policy.attempts, 5);\n+    assert!(policy.delay <= Duration::from_secs(2));\n+    assert!(policy.exhausted(5));\n+    assert!(!policy.exhausted(4));\n+}\n"));
        let syntax = patches.iter().map(|p| (p.path.clone(), p.lines.iter().map(|l| {
            l.text.split_inclusive(|c: char| !(c.is_alphanumeric() || c == '_')).map(|text| {
                let tone = match text.trim_matches(|c: char| !c.is_alphanumeric()) {
                    "pub" | "fn" | "struct" | "impl" => SyntaxTone::Keyword,
                    "Self" | "Duration" | "RetryPolicy" | "usize" | "bool" => SyntaxTone::Type,
                    "3" | "5" | "30" | "2" => SyntaxTone::Number,
                    _ => SyntaxTone::Plain,
                };
                SyntaxSpan { text: text.into(), tone }
            }).collect()
        }).collect())).collect();
        let mut v = DiffView::new(DiffData {
            agent: "sol".into(), turn: 12, title: "Bound endpoint retries".into(), headline: "3 files +18 −7".into(),
            files: vec![
                FileEntry { path: "src/retry.rs".into(), label: "src/retry.rs".into(), status: 'M', added: Some(6), removed: Some(2), authored: true },
                FileEntry { path: "src/client.rs".into(), label: "src/client.rs".into(), status: 'M', added: Some(8), removed: Some(3), authored: true },
                FileEntry { path: "tests/retry.rs".into(), label: "tests/retry.rs".into(), status: 'A', added: Some(4), removed: Some(2), authored: false },
            ], patches, syntax, can_undo: true, ..Default::default()
        }, vec![Draft { path: "src/retry.rs".into(), start: 12, end: 12, side: Side::New, text: "            delay: Duration::from_secs(2),".into(), note: "Keep this bounded so a slow endpoint cannot pin a worker forever. Please exercise both retry limits.".into() }]);
        v.side = Side::New;
        v.cursor = v.lines().iter().position(|l| l.kind == LineKind::Add).unwrap();
        v
    }

    #[test]
    fn review_layout_preserves_code_and_notes_at_narrow_and_wide_widths() {
        for width in [70, 120, 180] {
            let mut v = review();
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|f| render(f, &mut v)).unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains("attempts:") && text.contains("note · new L12"));
            assert!(text.contains("retry limits."), "the note must wrap rather than truncate");
            if width == 180 { assert!(text.contains("also changed")); }
            assert_eq!(v.layout(), if width < 110 { Layout::Unified } else { Layout::SideBySide });
            // Optional visual QA export: actual terminal cells, not a mock UI.
            if let Ok(dir) = std::env::var("AXEL_DIFF_PREVIEW_DIR") {
                std::fs::create_dir_all(&dir).unwrap();
                let cells: Vec<_> = buffer.content().iter().map(|c| serde_json::json!({
                    "text": c.symbol(), "fg": format!("{:?}", c.fg), "bg": format!("{:?}", c.bg), "mod": c.modifier.bits()
                })).collect();
                std::fs::write(std::path::Path::new(&dir).join(format!("review-{width}.json")), serde_json::to_vec(&serde_json::json!({ "width": width, "height": 30, "cells": cells })).unwrap()).unwrap();
            }
        }
    }

    #[test]
    fn short_files_share_the_viewport_and_clicks_keep_their_source_identity() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut v = review();
        let mut terminal = Terminal::new(TestBackend::new(180, 65)).unwrap();
        terminal.draw(|f| render(f, &mut v)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("connect_once") && text.contains("retry_policy_has_a_bounded_delay"));
        let (rect, file, line) = v.hits.iter().find_map(|(r, hit)| match hit {
            super::super::Hit::Line(file, i, Side::New) if *file == 2 && v.lines_for(*file)[*i].kind == LineKind::Add => Some((*r, *file, *i)),
            _ => None,
        }).unwrap();
        v.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 6, row: rect.y, modifiers: KeyModifiers::NONE });
        assert_eq!((v.file, v.cursor), (file, line));
        let draft = v.draft_at_cursor("Keep this case".into()).unwrap();
        assert_eq!(draft.path, "tests/retry.rs");
        assert_eq!(draft.side, Side::New);
        assert!(draft.text.contains("#[test]"));
    }

    #[test]
    fn keyboard_crosses_file_boundaries_and_marks_stay_with_their_file() {
        let mut v = review();
        v.layout = Some(Layout::Unified);
        v.cursor = v.lines().len() - 1;
        v.move_cursor(1);
        assert_eq!((v.file, v.cursor), (1, 0));
        v.move_cursor(-1);
        assert_eq!(v.file, 0);
        v.mark = Some(v.cursor);
        v.move_cursor(10);
        assert_eq!(v.file, 0, "a selection cannot silently move into another file");
        v.mark = None;
        v.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE));
        assert_eq!(v.file, 1);
        assert_eq!(v.lines()[v.cursor].kind, LineKind::Hunk);
    }

    #[test]
    fn light_and_dark_reviews_tint_changes_without_reversing_syntax() {
        for (fg, bg) in [((36, 41, 47), (255, 255, 255)), ((230, 237, 243), (50, 50, 58))] {
            let mut v = review();
            v.palette = super::super::palette::Palette::from_colors(fg, bg);
            let mut terminal = Terminal::new(TestBackend::new(180, 65)).unwrap();
            terminal.draw(|f| render(f, &mut v)).unwrap();
            let buffer = terminal.backend().buffer();
            assert!(buffer.content().iter().any(|c| c.bg == v.palette.row(LineKind::Add)));
            assert!(buffer.content().iter().any(|c| Some(c.bg) == v.palette.word(LineKind::Del)));
            assert!(!buffer.content().iter().any(|c| c.modifier.contains(Modifier::REVERSED)));
            assert!(buffer.content().iter().any(|c| c.fg == v.palette.syntax(SyntaxTone::Number)), "number syntax survives word emphasis");
        }
    }

    #[test]
    #[ignore = "interactive visual review in a dedicated terminal"]
    fn live_diff_preview() {
        let v = review();
        super::super::run_diff(v.data, v.drafts).unwrap();
    }

    #[test]
    fn tiny_panes_and_forced_split_layouts_do_not_overflow() {
        for width in [1, 2, 5, 12, 32] {
            for height in [1, 3, 4, 8] {
                let mut v = review();
                v.layout = Some(Layout::SideBySide);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| render(f, &mut v)).unwrap();
            }
        }
    }

    #[test]
    fn comment_side_and_horizontal_panning_preserve_source_anchors() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut v = review();
        v.layout = Some(Layout::SideBySide);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.lines()[v.cursor].kind, LineKind::Del);
        let old = v.draft_at_cursor("old side".into()).unwrap();
        v.handle_key(key(KeyCode::Right));
        assert_eq!(v.draft_at_cursor("old side".into()).unwrap(), old);
        v.handle_key(key(KeyCode::Tab));
        let new = v.draft_at_cursor("new side".into()).unwrap();
        assert_eq!((old.side, new.side), (Side::Old, Side::New));
        assert!(old.text.contains('3') && new.text.contains('5'));
    }

    #[test]
    fn mouse_clicks_select_the_visible_file_or_comment_side_and_wheel_scrolls_code() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut v = review();
        let mut terminal = Terminal::new(TestBackend::new(180, 16)).unwrap();
        terminal.draw(|f| render(f, &mut v)).unwrap();
        let mouse = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let (rect, index) = v.hits.iter().find_map(|(r, hit)| match hit {
            super::super::Hit::Line(file, i, Side::New) if *file == v.file && v.lines()[*i].kind == LineKind::Add => Some((*r, *i)),
            _ => None,
        }).unwrap();
        v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 10, rect.y));
        assert_eq!((v.cursor, v.side), (index, Side::New));
        let file = v.file;
        v.handle_mouse(mouse(MouseEventKind::ScrollDown, rect.x, rect.y));
        terminal.draw(|f| render(f, &mut v)).unwrap();
        assert!(v.scroll > 0);
        assert_eq!(v.file, file, "a short scroll stays within this file");
        let (rect, target) = v.hits.iter().find_map(|(r, hit)| match hit {
            super::super::Hit::File(i) if *i != file => Some((*r, *i)),
            _ => None,
        }).unwrap();
        v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 1, rect.y));
        assert_eq!(v.file, target);
        terminal.draw(|f| render(f, &mut v)).unwrap();
        assert_eq!(v.file, target);
        assert!(v.scroll > 0, "file navigation reveals its position in the stream");
    }
}
