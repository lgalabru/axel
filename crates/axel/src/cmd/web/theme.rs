//! The page's colours and type, from the pane's terminal: Ladybird's frames
//! are opaque, so the page looks part of the pane only by painting what
//! the terminal paints.  Its background, text and 16-colour palette (the
//! colours the terminal view draws agents and dim text in), and text the
//! size of the terminal's: as wide as one of its cells per character.

use super::terminal::{Colors, PaneSize, Rgb};

/// How far each surface sits from the background toward the text, in
/// percent: measured from the page's own dark palette (`app.css`), where
/// `--surface` #24242a is 3% of the way from #1d1d21 to #ececf1.
const STEPS: [(&str, u8); 5] = [("--surface", 4), ("--line-soft", 7), ("--raised", 9), ("--hover", 12), ("--line", 14)];
/// Text shades, as steps from the text back toward the background, for a
/// terminal that didn't say its bright black (palette 8, its dim text).
const TEXT_STEPS: [(&str, u8); 2] = [("--text-2", 22), ("--faint", 62)];
const DIM_STEP: u8 = 45;

/// JetBrains Mono's advance is 600 units of its 1000 per em: text whose
/// characters are a cell wide is the cell's width over this.
const ADVANCE_EM: f64 = 0.6;
/// No terminal text is smaller or larger than this, in CSS pixels.
const FONT_PX_MIN: f64 = 9.0;
const FONT_PX_MAX: f64 = 32.0;

/// The terminal's text size in CSS pixels, for a pane of `size` drawn at
/// `ratio`: its characters as wide as the terminal's cells.
pub fn font_px(size: &PaneSize, ratio: f64) -> Option<f64> {
    assert!(ratio > 0.0, "a pixel ratio is positive");
    if size.cols == 0 || size.width_px == 0 {
        return None;
    }
    let cell_css = f64::from(size.width_px) / f64::from(size.cols) / ratio;
    Some((cell_css / ADVANCE_EM).clamp(FONT_PX_MIN, FONT_PX_MAX))
}

/// CSS custom properties for the page's root.  What the terminal didn't
/// say keeps the page's own value.
pub fn css(colors: &Colors, font_px: Option<f64>) -> String {
    let mut out = String::new();
    if let Some(px) = font_px {
        out.push_str(&format!("--font-size:{:.2}px;", px));
    }
    for (index, rgb) in colors.palette.iter().enumerate() {
        if let Some(rgb) = rgb {
            out.push_str(&format!("--p{index}:{};", hex(*rgb)));
        }
    }
    if let (Some(fg), Some(bg)) = (colors.foreground, colors.background) {
        out.push_str(&format!("--bg:{};--text:{};", hex(bg), hex(fg)));
        for (name, percent) in STEPS {
            out.push_str(&format!("{name}:{};", hex(mix(bg, fg, percent))));
        }
        for (name, percent) in TEXT_STEPS {
            out.push_str(&format!("{name}:{};", hex(mix(fg, bg, percent))));
        }
        let dim = colors.palette[8].unwrap_or_else(|| mix(fg, bg, DIM_STEP));
        out.push_str(&format!("--dim:{};", hex(dim)));
        let scheme = if luminance(bg) < luminance(fg) { "dark" } else { "light" };
        out.push_str(&format!("color-scheme:{scheme};"));
    }
    debug_assert!(out.chars().all(|c| c.is_ascii_alphanumeric() || "-:;#.".contains(c)), "nothing but properties: {out}");
    out
}

/// `from`, `percent` of the way to `to`.
fn mix(from: Rgb, to: Rgb, percent: u8) -> Rgb {
    assert!(percent <= 100, "a mix is a percentage");
    let channel = |a: u8, b: u8| {
        let (a, b, p) = (i32::from(a), i32::from(b), i32::from(percent));
        u8::try_from(a + (b - a) * p / 100).expect("a mix stays between its ends")
    };
    (channel(from.0, to.0), channel(from.1, to.1), channel(from.2, to.2))
}

fn luminance((r, g, b): Rgb) -> u32 {
    299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)
}

fn hex((r, g, b): Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_takes_the_terminals_colours_and_palette() {
        assert_eq!(css(&Colors::default(), None), "", "no answer keeps the page's own palette");
        let mut colors = Colors { foreground: Some((0xec, 0xec, 0xf1)), background: Some((0x1d, 0x1d, 0x21)), ..Colors::default() };
        colors.palette[3] = Some((0xe6, 0xb4, 0x50));
        let dark = css(&colors, None);
        assert!(dark.starts_with("--p3:#e6b450;--bg:#1d1d21;--text:#ececf1;"), "{dark}");
        assert!(dark.ends_with("color-scheme:dark;"));
        colors.palette[8] = Some((0x66, 0x66, 0x66));
        assert!(css(&colors, None).contains("--dim:#666666;"), "dim text is the terminal's bright black");
        let light = css(&Colors { foreground: Some((0x20, 0x20, 0x20)), background: Some((0xff, 0xff, 0xff)), ..Colors::default() }, None);
        assert!(light.contains("--bg:#ffffff;") && light.ends_with("color-scheme:light;"), "{light}");
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 50), (100, 50, 25));
    }

    #[test]
    fn text_is_as_wide_as_the_terminals_cells() {
        // A Retina pane: 120 cells across 2040 pixels, at 2x: 8.5 CSS pixels a cell.
        let pane = PaneSize { rows: 50, cols: 120, width_px: 2040, height_px: 1700 };
        let px = font_px(&pane, 2.0).unwrap();
        assert!((px - 14.1667).abs() < 0.01, "{px}");
        assert!(css(&Colors::default(), Some(px)).starts_with("--font-size:14.17px;"));
        assert_eq!(font_px(&PaneSize { width_px: 0, ..pane }, 1.0), None, "no pixels, no size: the page's own");
        assert_eq!(font_px(&PaneSize { width_px: 100, ..pane }, 3.0), Some(FONT_PX_MIN));
    }
}
