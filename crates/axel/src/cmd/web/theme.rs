//! The page's colours, from the pane's terminal: Ladybird's frames are
//! opaque, so the page can only look part of the pane by painting the
//! terminal's own background, and its text in the terminal's foreground.
//! Every other surface is a step between the two, as the CSS defaults are
//! for its own dark palette.

use axel_tui::theme::TerminalColors;

type Rgb = (u8, u8, u8);

/// How far each surface sits from the background toward the text, in
/// percent: measured from the page's own dark palette (`app.css`), where
/// `--surface` #24242a is 3% of the way from #1d1d21 to #ececf1.
const STEPS: [(&str, u8); 5] = [("--surface", 3), ("--line-soft", 6), ("--raised", 7), ("--hover", 10), ("--line", 11)];
/// Text shades, as steps from the text back toward the background.
const TEXT_STEPS: [(&str, u8); 3] = [("--text-2", 27), ("--dim", 48), ("--faint", 68)];

/// CSS custom properties for `<html style="…">`: empty without colours,
/// which keeps the page's own palette.
pub fn css(colors: Option<TerminalColors>) -> String {
    let Some(TerminalColors { foreground: fg, background: bg }) = colors else { return String::new() };
    let mut out = format!("--bg:{};--text:{};", hex(bg), hex(fg));
    for (name, percent) in STEPS {
        out.push_str(&format!("{name}:{};", hex(mix(bg, fg, percent))));
    }
    for (name, percent) in TEXT_STEPS {
        out.push_str(&format!("{name}:{};", hex(mix(fg, bg, percent))));
    }
    let scheme = if luminance(bg) < luminance(fg) { "dark" } else { "light" };
    out.push_str(&format!("color-scheme:{scheme};"));
    debug_assert!(out.chars().all(|c| c.is_ascii_alphanumeric() || "-:;#".contains(c)), "nothing but properties: {out}");
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
    fn the_page_takes_the_terminals_colours() {
        assert_eq!(css(None), "", "no answer keeps the page's own palette");
        let dark = css(Some(TerminalColors { foreground: (0xec, 0xec, 0xf1), background: (0x1d, 0x1d, 0x21) }));
        assert!(dark.starts_with("--bg:#1d1d21;--text:#ececf1;--surface:#232327;"), "{dark}");
        assert!(dark.ends_with("color-scheme:dark;"));
        let light = css(Some(TerminalColors { foreground: (0x20, 0x20, 0x20), background: (0xff, 0xff, 0xff) }));
        assert!(light.contains("--bg:#ffffff;") && light.ends_with("color-scheme:light;"), "{light}");
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 50), (100, 50, 25));
    }
}
