//! Agent prose as styled terminal lines: the Markdown agents write (bold,
//! code, italics, headings, lists, quotes, fenced blocks) shown as text
//! styles, the markers themselves dropped.  Plain terminal output: it wraps
//! to the width and scrolls natively, like everything in the stream.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Inline code and fenced blocks.
pub const CODE: Color = Color::Blue;

/// One character and its style.
type Cell = (char, Style);

/// `text` as lines at most `width` columns wide.
pub fn md_lines(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let width = width.max(8);
    let code = base.fg(CODE).bg(Color::Reset);
    let mut out = Vec::new();
    let mut fenced = false;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        i += 1;
        let trimmed = raw.trim_start();
        // | a | b |, then | --- | --- |: a table, up to its last `|` row.
        if !fenced && trimmed.starts_with('|') && lines.get(i).is_some_and(|l| is_rule(l)) {
            let mut end = i + 1;
            while lines.get(end).is_some_and(|l| l.trim_start().starts_with('|')) {
                end += 1;
            }
            out.extend(table(raw, lines[i], &lines[i + 1..end], width, base, code));
            i = end;
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            // Kept as written, spaces and all, broken only at the width.
            let cells: Vec<Cell> = raw.chars().map(|c| (c, code)).collect();
            for chunk in cells.chunks(width.saturating_sub(2).max(1)) {
                let mut line = vec![(' ', base), (' ', base)];
                line.extend_from_slice(chunk);
                out.push(to_line(&line));
            }
            if cells.is_empty() {
                out.push(Line::from(""));
            }
            continue;
        }
        let (prefix, body, style) = block(raw, base);
        let cells = inline(body, style, code);
        out.extend(wrap_cells(&prefix, &cells, width));
    }
    out
}

/// A line's block kind: what goes before it (and before each of its wrapped
/// lines), the text, and its style.
fn block(line: &str, base: Style) -> (Vec<Cell>, &str, Style) {
    let indent = line.len() - line.trim_start().len();
    let rest = line.trim_start();
    let pad = |n: usize| -> Vec<Cell> { (0..n).map(|_| (' ', base)).collect() };
    // # Heading
    let hashes = rest.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        return (Vec::new(), rest[hashes..].trim(), base.add_modifier(Modifier::BOLD));
    }
    // - item, * item, + item
    if let Some(item) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")).or_else(|| rest.strip_prefix("+ ")) {
        let mut p = pad(indent);
        p.extend([('•', base), (' ', base)]);
        return (p, item, base);
    }
    // 1. item, 1) item
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && (rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ")) {
        let mut p = pad(indent);
        p.extend(rest[..digits + 2].chars().map(|c| (c, base)));
        return (p, &rest[digits + 2..], base);
    }
    // > quote
    if let Some(q) = rest.strip_prefix("> ").or_else(|| rest.strip_prefix('>')) {
        let dim = base.add_modifier(Modifier::DIM);
        return (vec![('│', dim), (' ', dim)], q, dim);
    }
    (pad(indent), rest, base)
}

/// The text with `**bold**`, `` `code` ``, `*italic*` and `_italic_` styled
/// and their markers dropped. A marker only counts when it closes later on
/// the line (and, for the single ones, sits on a word boundary), so
/// `snake_case`, `2*3*4` and a lone backtick stay as written.
fn inline(text: &str, base: Style, code: Style) -> Vec<Cell> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::with_capacity(n);
    let (mut bold, mut italic) = (false, None::<char>);
    let mut i = 0;
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric());
    let style = |bold: bool, italic: bool| {
        let mut s = base;
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    };
    while i < n {
        let c = chars[i];
        // `code`: everything up to the next backtick, as written.
        if c == '`'
            && let Some(len) = chars[i + 1..].iter().position(|x| *x == '`')
            && len > 0
        {
            // Code inside bold is bold code.
            let st = if bold { code.add_modifier(Modifier::BOLD) } else { code };
            out.extend(chars[i + 1..i + 1 + len].iter().map(|c| (*c, st)));
            i += len + 2;
            continue;
        }
        // **bold**
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            let opens = !bold && chars.get(i + 2).is_some_and(|c| !c.is_whitespace()) && find_pair(&chars, i + 2, "**").is_some();
            if opens || bold {
                bold = !bold;
                i += 2;
                continue;
            }
        }
        // *italic*, _italic_
        if (c == '*' || c == '_') && italic == Some(c) && !word(chars.get(i + 1)) {
            italic = None;
            i += 1;
            continue;
        }
        if (c == '*' || c == '_')
            && italic.is_none()
            && !word(if i == 0 { None } else { chars.get(i - 1) })
            && chars.get(i + 1).is_some_and(|c| !c.is_whitespace())
            && closes_single(&chars, i + 1, c)
        {
            italic = Some(c);
            i += 1;
            continue;
        }
        out.push((c, style(bold, italic.is_some())));
        i += 1;
    }
    out
}

/// Where `pat` next occurs at or after `from`.
fn find_pair(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    (from..chars.len().saturating_sub(p.len() - 1)).find(|&j| chars[j..j + p.len()] == p[..])
}

/// A single `marker` after `from` that closes an italic: not doubled, after a
/// non-space, before a non-word.
fn closes_single(chars: &[char], from: usize, marker: char) -> bool {
    (from..chars.len()).any(|j| {
        chars[j] == marker
            && chars.get(j + 1) != Some(&marker)
            && j > from
            && !chars[j - 1].is_whitespace()
            && !chars.get(j + 1).is_some_and(|c| c.is_alphanumeric())
    })
}

/// A table's `| --- | :---: |` line.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// How a column lines up, from its rule cell.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

/// A table row's cells, split at the `|`s outside code (and not escaped).
fn row_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let mut cells = vec![String::new()];
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cells.last_mut().unwrap().push('|');
                chars.next();
            }
            '`' => {
                code = !code;
                cells.last_mut().unwrap().push(c);
            }
            '|' if !code => cells.push(String::new()),
            _ => cells.last_mut().unwrap().push(c),
        }
    }
    // The closing `|` leaves an empty cell behind.
    if cells.len() > 1 && cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

/// A table as aligned columns: the header bold, a rule under it, columns
/// split by `│`. Columns too wide for `width` share it, widest first, and
/// their text wraps within the column.
fn table(header: &str, rule: &str, body: &[&str], width: usize, base: Style, code: Style) -> Vec<Line<'static>> {
    let aligns: Vec<Align> = row_cells(rule)
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect();
    let bold = base.add_modifier(Modifier::BOLD);
    let mut rows: Vec<Vec<Vec<Cell>>> = vec![row_cells(header).iter().map(|c| inline(c, bold, code.add_modifier(Modifier::BOLD))).collect()];
    rows.extend(body.iter().map(|l| row_cells(l).iter().map(|c| inline(c, base, code)).collect()));
    let n = rows.iter().map(Vec::len).max().unwrap_or(0).max(aligns.len());
    for r in &mut rows {
        r.resize(n, Vec::new());
    }
    if n == 0 {
        return Vec::new();
    }
    // Widths: each column's longest cell, then the widest ones shrunk until
    // the row fits (down to a few columns each).
    let mut widths: Vec<usize> = (0..n).map(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0).max(1)).collect();
    let room = width.saturating_sub(3 * (n - 1));
    const MIN: usize = 6;
    while widths.iter().sum::<usize>() > room {
        let (at, w) = widths.iter().copied().enumerate().max_by_key(|(_, w)| *w).unwrap();
        if w <= MIN {
            break;
        }
        widths[at] -= 1;
    }
    let dim = base.add_modifier(Modifier::DIM);
    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let wrapped: Vec<Vec<Vec<Cell>>> = row.iter().zip(&widths).map(|(cell, w)| wrap_words(cell, *w)).collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for li in 0..height {
            let mut line: Vec<Cell> = Vec::new();
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    line.extend([(' ', base), ('│', dim), (' ', base)]);
                }
                let text = wrapped[c].get(li).map(Vec::as_slice).unwrap_or(&[]);
                let gap = w.saturating_sub(text.len());
                let (before, after) = match aligns.get(c).copied().unwrap_or(Align::Left) {
                    Align::Left => (0, gap),
                    Align::Right => (gap, 0),
                    Align::Center => (gap / 2, gap - gap / 2),
                };
                line.extend((0..before).map(|_| (' ', base)));
                line.extend_from_slice(text);
                // No trailing spaces after the last column.
                if c + 1 < n {
                    line.extend((0..after).map(|_| (' ', base)));
                }
            }
            out.push(to_line(&line));
        }
        if ri == 0 {
            let mut line: Vec<Cell> = Vec::new();
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    line.extend([('─', dim), ('┼', dim), ('─', dim)]);
                }
                line.extend((0..*w).map(|_| ('─', dim)));
            }
            out.push(to_line(&line));
        }
    }
    out
}

/// Word-wrap styled text to `width`, `prefix` before the first line and as
/// much space before the rest (so a list item's lines line up under its text).
fn wrap_cells(prefix: &[Cell], cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    let hang: Vec<Cell> = prefix.iter().map(|(_, s)| (' ', *s)).collect();
    let room = width.saturating_sub(prefix.len()).max(4);
    wrap_words(cells, room)
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut full = if i == 0 { prefix.to_vec() } else { hang.clone() };
            full.extend(l);
            to_line(&full)
        })
        .collect()
}

/// Styled text word-wrapped to lines of at most `room` cells.
fn wrap_words(cells: &[Cell], room: usize) -> Vec<Vec<Cell>> {
    let room = room.max(1);
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    // Words, each with the space cell that came before it (its own style).
    let mut words: Vec<(Option<Cell>, Vec<Cell>)> = vec![(None, Vec::new())];
    for &cell in cells {
        if cell.0 == ' ' {
            words.push((Some(cell), Vec::new()));
        } else {
            words.last_mut().unwrap().1.push(cell);
        }
    }
    for (space, word) in words {
        let need = if cur.is_empty() { word.len() } else { cur.len() + 1 + word.len() };
        if need > room && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(space.unwrap_or((' ', Style::default())));
        }
        // A word longer than the line breaks where it must.
        let mut w = &word[..];
        while cur.len() + w.len() > room && !w.is_empty() {
            let take = room - cur.len();
            if take == 0 {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            cur.extend_from_slice(&w[..take]);
            lines.push(std::mem::take(&mut cur));
            w = &w[take..];
        }
        cur.extend_from_slice(w);
    }
    lines.push(cur);
    lines
}

/// Cells as a line, runs of one style as one span.
fn to_line(cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut style = None;
    for &(c, s) in cells {
        if style != Some(s) && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), style.unwrap_or_default()));
        }
        style = Some(s);
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style.unwrap_or_default()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    fn span<'a>(lines: &'a [Line], content: &str) -> &'a Span<'a> {
        lines.iter().flat_map(|l| l.spans.iter()).find(|s| s.content == content).unwrap_or_else(|| panic!("no span {content:?} in {:?}", text(lines)))
    }

    #[test]
    fn bold_and_code_lose_their_markers_and_keep_their_look() {
        let l = md_lines("- **`rust/Cargo.lock`:** my interrupted `cargo update` changed it", 200, Style::default());
        assert_eq!(text(&l), vec!["• rust/Cargo.lock: my interrupted cargo update changed it"]);
        let path = span(&l, "rust/Cargo.lock").style;
        assert!(path.fg == Some(CODE) && path.add_modifier.contains(Modifier::BOLD), "code inside bold is bold code");
        assert!(span(&l, ":").style.add_modifier.contains(Modifier::BOLD), "the colon is still bold");
        assert_eq!(span(&l, "cargo update").style.fg, Some(CODE));
    }

    #[test]
    fn markers_that_arent_markdown_stay() {
        let l = md_lines("use snake_case and 2*3*4, a lone ` and **unclosed", 200, Style::default());
        assert_eq!(text(&l), vec!["use snake_case and 2*3*4, a lone ` and **unclosed"]);
        let l = md_lines("an *emphasised* word and _another_ one", 200, Style::default());
        assert_eq!(text(&l), vec!["an emphasised word and another one"]);
        assert!(span(&l, "emphasised").style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn headings_lists_and_quotes() {
        let l = md_lines("## What I would do\n1. first\n> quoted", 200, Style::default());
        assert_eq!(text(&l), vec!["What I would do", "1. first", "│ quoted"]);
        assert!(span(&l, "What I would do").style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_list_item_wraps_under_its_text() {
        let l = md_lines("- one two three four five six", 14, Style::default());
        assert_eq!(text(&l), vec!["• one two", "  three four", "  five six"]);
    }

    #[test]
    fn a_fenced_block_keeps_its_spacing_and_drops_its_fences() {
        let l = md_lines("run:\n```sh\n  cargo test  -p axel\n```\nthen", 40, Style::default());
        assert_eq!(text(&l), vec!["run:", "    cargo test  -p axel", "then"]);
        let code = span(&l, "  cargo test  -p axel").style;
        assert_eq!((code.fg, code.bg), (Some(CODE), Some(Color::Reset)), "use the terminal's background");
    }

    #[test]
    fn a_table_is_drawn_as_aligned_columns() {
        let md = "before\n| | Message | Thinking |\n| --- | --- | ---: |\n| Who sees it | The user | **Nobody** |\n| Here | `text` | pill |\nafter";
        let l = md_lines(md, 200, Style::default());
        assert_eq!(
            text(&l),
            vec![
                "before",
                "            │ Message  │ Thinking",
                "────────────┼──────────┼─────────",
                "Who sees it │ The user │   Nobody",
                "Here        │ text     │     pill",
                "after",
            ]
        );
        assert!(span(&l, "Message").style.add_modifier.contains(Modifier::BOLD), "the header is bold");
        assert!(span(&l, "Nobody").style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span(&l, "text").style.fg, Some(CODE));
    }

    #[test]
    fn a_table_too_wide_wraps_within_its_columns() {
        let md = "| a | b |\n| - | - |\n| one | the quick brown fox jumps over the lazy dog |";
        let l = md_lines(md, 20, Style::default());
        assert!(text(&l).iter().all(|r| r.chars().count() <= 20), "{:?}", text(&l));
        assert_eq!(text(&l)[2], "one │ the quick");
        assert_eq!(text(&l)[3], "    │ brown fox");
    }

    #[test]
    fn pipes_in_code_or_escaped_stay_in_their_cell() {
        assert_eq!(row_cells("| `a|b` | c \\| d |"), vec!["`a|b`", "c | d"]);
        // A lone `|` line with no rule under it is just text.
        assert_eq!(text(&md_lines("| not a table", 80, Style::default())), vec!["| not a table"]);
    }
}
