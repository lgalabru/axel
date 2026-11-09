use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::{DiffLine, LineKind, SyntaxSpan, SyntaxTone, side_rows};

fn tokens(text: &str) -> Vec<(Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| if c.is_alphanumeric() || c == '_' { 0 } else if c.is_whitespace() { 1 } else { 2 };
    while start < chars.len() {
        let mut end = start + 1;
        while end < chars.len() && class(chars[start]) < 2 && class(chars[end]) == class(chars[start]) {
            end += 1;
        }
        out.push((start..end, chars[start..end].iter().collect()));
        start = end;
    }
    out
}

fn changed_words(old: &str, new: &str, budget: &mut usize) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let a = tokens(old);
    let b = tokens(new);
    // Bound work for generated code and very long minified lines.
    let work = a.len().saturating_mul(b.len());
    if work > 16_384 || work > *budget {
        let a: Vec<_> = old.chars().collect();
        let b: Vec<_> = new.chars().collect();
        let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
        return (vec![prefix..a.len() - suffix], vec![prefix..b.len() - suffix]);
    }
    *budget -= work;
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i].1 == b[j].1 { 1 + lcs[i + 1][j + 1] } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut removed, mut added) = (0, 0, Vec::new(), Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i].1 == b[j].1 {
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            removed.push(a[i].0.clone());
            i += 1;
        } else {
            added.push(b[j].0.clone());
            j += 1;
        }
    }
    (removed, added)
}

pub(super) fn word_changes(lines: &[DiffLine]) -> Vec<Vec<Range<usize>>> {
    let mut changes = vec![Vec::new(); lines.len()];
    let mut budget = 1_000_000;
    for row in side_rows(lines) {
        if let (Some(a), Some(b)) = (row.left, row.right)
            && lines[a].kind == LineKind::Del && lines[b].kind == LineKind::Add
        {
            let (old, new) = changed_words(&lines[a].text, &lines[b].text, &mut budget);
            changes[a] = old;
            changes[b] = new;
        }
    }
    changes
}

fn tone(tone: SyntaxTone, palette: &super::palette::Palette) -> Style {
    let style = Style::default().fg(palette.syntax(tone));
    match tone {
        SyntaxTone::Bold => style.add_modifier(Modifier::BOLD),
        SyntaxTone::Italic => style.add_modifier(Modifier::ITALIC),
        _ => style,
    }
}

fn append(out: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = out.last_mut().filter(|s| s.style == style) {
        last.content.to_mut().push_str(text);
    } else {
        out.push(Span::styled(text.to_string(), style));
    }
}

/// A code cell in terminal columns. The source and review anchors stay intact.
pub(super) fn code(text: &str, syntax: &[SyntaxSpan], changes: &[Range<usize>], offset: usize, width: usize, base: Style, palette: Option<(&super::palette::Palette, LineKind)>) -> Vec<Span<'static>> {
    if width == 0 { return Vec::new() }
    let fallback = super::palette::Palette::default();
    let word_bg = palette.and_then(|(p, kind)| p.word(kind));
    let palette = palette.map(|(p, _)| p).unwrap_or(&fallback);
    let syntax_matches = syntax.iter().map(|s| s.text.as_str()).collect::<String>() == text;
    let cells: Vec<(char, Style)> = if syntax_matches {
        syntax.iter().flat_map(|s| s.text.chars().map(move |c| (c, base.patch(tone(s.tone, palette))))).collect()
    } else {
        text.chars().map(|c| (c, base)).collect()
    };
    let cell_width = |c: char, col: usize| if c == '\t' { 4 - col % 4 } else if c.is_control() { 1 } else { c.width().unwrap_or(0) };
    let total = cells.iter().fold(0, |col, (c, _)| col + cell_width(*c, col));
    let clipped = total > offset.saturating_add(width);
    let room = width.saturating_sub(usize::from(clipped));
    let (mut col, mut used, mut out) = (0, 0, Vec::new());
    for (i, (c, mut style)) in cells.into_iter().enumerate() {
        let w = cell_width(c, col);
        let start = col;
        col += w;
        if col <= offset && w > 0 { continue }
        if start >= offset + room && w > 0 { break }
        if changes.iter().any(|r| r.contains(&i)) {
            style = if let Some(bg) = word_bg { style.bg(bg) } else { style.add_modifier(Modifier::UNDERLINED) };
        }
        if start < offset || col > offset + room || c == '\t' {
            let spaces = col.min(offset + room).saturating_sub(start.max(offset));
            append(&mut out, &" ".repeat(spaces), style);
            used += spaces;
        } else if w > 0 || used > 0 {
            let c = if c.is_control() { '�' } else { c };
            append(&mut out, &c.to_string(), style);
            used += w;
        }
    }
    append(&mut out, &" ".repeat(room.saturating_sub(used)), base);
    if clipped { append(&mut out, "›", crate::theme::dim()); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_changes_leave_unchanged_tokens_between_edits_alone() {
        let (a, b) = changed_words("let old = call(1);", "let new = call(2);", &mut 1_000_000);
        let text = |s: &str, ranges: &[Range<usize>]| s.chars().enumerate().filter(|(i, _)| ranges.iter().any(|r| r.contains(i))).map(|(_, c)| c).collect::<String>();
        assert_eq!(text("let old = call(1);", &a), "old1");
        assert_eq!(text("let new = call(2);", &b), "new2");
    }

    #[test]
    fn code_cells_keep_terminal_width_and_sanitize_controls() {
        for (text, offset, width) in [("a\t界e\u{301}xyz", 2, 7), ("界界界", 1, 4), ("x\x1b[31m", 0, 12)] {
            let spans = code(text, &[], &[], offset, width, Style::default(), None);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), width);
            assert!(!text.contains('\x1b'));
        }
    }
}
