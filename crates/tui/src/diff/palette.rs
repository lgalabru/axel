use ratatui::style::Color;

use super::{LineKind, SyntaxTone};

/// Derived surfaces are used only when the terminal reports its own colours.
/// Unsupported terminals retain native colours and underline word changes.
#[derive(Clone, Debug)]
pub(super) struct Palette {
    pub text: Color,
    pub muted: Color,
    pub panel: Color,
    pub border: Color,
    syntax: [Color; 5],
    added: Color,
    removed: Color,
    added_word: Option<Color>,
    removed_word: Option<Color>,
    pub add_sign: Color,
    pub remove_sign: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self { text: Color::Reset, muted: Color::DarkGray, panel: Color::Reset,
            border: Color::DarkGray, syntax: [Color::Magenta, Color::Green, Color::Yellow, Color::Blue, Color::Cyan], added: Color::Reset, removed: Color::Reset,
            added_word: None, removed_word: None, add_sign: Color::Green, remove_sign: Color::Red }
    }
}

fn blend(bg: (u8, u8, u8), tint: (u8, u8, u8), amount: f32) -> Color {
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color::Rgb(channel(bg.0, tint.0), channel(bg.1, tint.1), channel(bg.2, tint.2))
}

impl Palette {
    pub fn detect() -> Self {
        // Crossterm owns terminal input after the first event poll. Query only
        // before that reader starts, and reuse the palette across turn changes.
        static PALETTE: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
        PALETTE.get_or_init(Self::query).clone()
    }

    fn query() -> Self {
        crate::theme::terminal_colors().map(|p| {
            Self::from_colors(p.foreground, p.background)
        }).unwrap_or_default()
    }

    pub fn from_colors(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> Self {
        let light = u32::from(bg.0) * 299 + u32::from(bg.1) * 587 + u32::from(bg.2) * 114 > 150_000;
        let (green, red) = if light { ((26, 127, 55), (207, 34, 46)) } else { ((86, 190, 112), (235, 109, 112)) };
        Self {
            syntax: if light {
                [Color::Rgb(164, 36, 64), Color::Rgb(10, 91, 45), Color::Rgb(139, 74, 17), Color::Rgb(5, 80, 174), Color::Rgb(88, 55, 155)]
            } else {
                [Color::Rgb(235, 143, 173), Color::Rgb(166, 209, 157), Color::Rgb(230, 185, 130), Color::Rgb(137, 180, 230), Color::Rgb(148, 204, 207)]
            },
            text: Color::Rgb(fg.0, fg.1, fg.2), muted: blend(bg, fg, 0.60),
            panel: blend(bg, fg, 0.055), border: blend(bg, fg, 0.18),
            added: blend(bg, green, 0.10), removed: blend(bg, red, 0.10),
            added_word: Some(blend(bg, green, 0.23)), removed_word: Some(blend(bg, red, 0.23)),
            add_sign: Color::Rgb(green.0, green.1, green.2), remove_sign: Color::Rgb(red.0, red.1, red.2),
        }
    }

    pub fn syntax(&self, tone: SyntaxTone) -> Color {
        match tone {
            SyntaxTone::Muted => self.muted,
            SyntaxTone::Keyword => self.syntax[0], SyntaxTone::String => self.syntax[1],
            SyntaxTone::Number => self.syntax[2], SyntaxTone::Function => self.syntax[3],
            SyntaxTone::Type => self.syntax[4], _ => self.text,
        }
    }

    pub fn row(&self, kind: LineKind) -> Color {
        match kind { LineKind::Add => self.added, LineKind::Del => self.removed, _ => Color::Reset }
    }

    pub fn word(&self, kind: LineKind) -> Option<Color> {
        match kind { LineKind::Add => self.added_word, LineKind::Del => self.removed_word, _ => None }
    }
}
