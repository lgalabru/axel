//! Agent prose as styled terminal lines: the Markdown agents write (bold,
//! code, italics, headings, lists, quotes, fenced blocks) shown as text
//! styles, the markers themselves dropped.  Plain terminal output: it wraps
//! to the width and scrolls natively, like everything in the stream.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Inline code and fenced blocks.
pub const CODE: Color = Color::Indexed(110);

/// One character and its style.
type Cell = (char, Style);

/// `text` as lines at most `width` columns wide.
pub fn md_lines(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let width = width.max(8);
    let code = base.fg(CODE);
    let mut out = Vec::new();
    let mut fenced = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
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

/// Word-wrap styled text to `width`, `prefix` before the first line and as
/// much space before the rest (so a list item's lines line up under its text).
fn wrap_cells(prefix: &[Cell], cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    let hang: Vec<Cell> = prefix.iter().map(|(_, s)| (' ', *s)).collect();
    let room = width.saturating_sub(prefix.len()).max(4);
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
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut full = if i == 0 { prefix.to_vec() } else { hang.clone() };
            full.extend(l);
            to_line(&full)
        })
        .collect()
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
        let l = md_lines("run:\n```sh\n  cargo test  -p axel\n```\nthen", 200, Style::default());
        assert_eq!(text(&l), vec!["run:", "    cargo test  -p axel", "then"]);
        assert_eq!(span(&l, "  cargo test  -p axel").style.fg, Some(CODE));
    }
}
