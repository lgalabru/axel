//! One-row charts in braille: two bars a cell, each up to four dots tall.
//!
//! Braille is a font glyph, so it sits on the text's baseline and within
//! its height. Block elements (`▁…█`) fill the whole cell instead and look
//! adrift between rows. Every bar stands on a floor of dots (dim where the
//! whole cell is quiet), so a chart reads as one unbroken timeline.

use ratatui::style::Style;
use ratatui::text::Span;

use crate::theme;

/// `series` (oldest first) as `bars` bars, the newest at the right, each
/// the average of its slice so uneven slices compare. Never more bars than
/// samples.
pub fn bars(series: &[i64], bars: usize) -> Vec<i64> {
    let n = series.len();
    let bars = bars.min(n);
    (0..bars)
        .map(|b| {
            let slice = &series[b * n / bars..(b + 1) * n / bars];
            slice.iter().sum::<i64>() / slice.len() as i64
        })
        .collect()
}

/// The tallest bar `series` makes in `width` cells: a shared scale for
/// charts drawn side by side.
pub fn peak(series: &[i64], width: usize) -> i64 {
    bars(series, 2 * width).into_iter().max().unwrap_or(0)
}

/// `series` charted in `width` cells against `max` (its own peak when 0),
/// right-aligned: a short series leaves quiet cells on the left.
pub fn braille(series: &[i64], width: usize, max: i64, style: Style) -> Vec<Span<'static>> {
    // Dots of each braille column, bottom up.
    const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];
    let mut b = bars(series, 2 * width);
    let max = if max > 0 { max } else { b.iter().copied().max().unwrap_or(0) };
    let mut padded = vec![0; 2 * width - b.len()];
    padded.append(&mut b);
    let level = |v: i64| if v <= 0 || max <= 0 { 0 } else { ((v * 4 + max - 1) / max).clamp(1, 4) as usize };
    let dots = |col: &[u32; 4], n: usize| col[..n].iter().sum::<u32>();
    padded
        .chunks(2)
        .map(|pair| {
            let (l, r) = (level(pair[0]), level(pair[1]));
            if l == 0 && r == 0 {
                Span::styled("⣀", theme::dim())
            } else {
                // A quiet bar beside a busy one keeps its floor dot, so the
                // floor runs unbroken under the bars.
                let ch = char::from_u32(0x2800 + dots(&LEFT, l.max(1)) + dots(&RIGHT, r.max(1))).unwrap_or(' ');
                Span::styled(ch.to_string(), style)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(series: &[i64], w: usize) -> String {
        braille(series, w, 0, Style::default()).iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn two_bars_a_cell_bottom_up() {
        let mut s = vec![0; 60];
        s[0] = 10;
        s[59] = 10;
        assert_eq!(text(&s, 6), "⣇⣀⣀⣀⣀⣸", "five samples a bar, the first and last full height");
        assert_eq!(text(&[0; 60], 4), "⣀⣀⣀⣀", "a quiet series is a dotted floor");
    }

    #[test]
    fn a_short_series_sits_at_the_right() {
        assert_eq!(text(&[5, 10], 3), "⣀⣀⣼");
        assert_eq!(text(&[], 2), "⣀⣀");
    }

    #[test]
    fn uneven_slices_are_averaged() {
        assert_eq!(bars(&[4, 4, 4], 2), vec![4, 4], "one sample then two: same rate, same bar");
    }
}
