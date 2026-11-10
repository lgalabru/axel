//! Restrained palette: one accent, dim text, and the seven agent colours.

use axel_store::{AgentColor, StatusState};
use ratatui::style::{Color, Modifier, Style};

/// The terminal's own foreground and background, in 8-bit RGB.
#[derive(Clone, Copy, Debug)]
pub struct TerminalColors {
    pub foreground: (u8, u8, u8),
    pub background: (u8, u8, u8),
}

/// Query once before crossterm owns terminal input. Unknown palettes use native
/// colors; a failed or unsupported probe never implies a dark theme.
pub fn terminal_colors() -> Option<TerminalColors> {
    static COLORS: std::sync::OnceLock<Option<TerminalColors>> = std::sync::OnceLock::new();
    *COLORS.get_or_init(|| {
        let mut options = terminal_colorsaurus::QueryOptions::default();
        options.timeout = std::time::Duration::from_millis(150);
        terminal_colorsaurus::color_palette(options).ok().map(|p| TerminalColors {
            foreground: p.foreground.scale_to_8bit(), background: p.background.scale_to_8bit(),
        })
    })
}

/// The single accent colour (titles, selection, active input): coral red #FF7565.
pub const ACCENT: Color = Color::Rgb(255, 117, 101);
/// Muted text for hints, breadcrumbs and timestamps.
pub const DIM: Color = Color::DarkGray;
/// Errors and denied/blocked signals.
pub const DANGER: Color = Color::Indexed(167);
/// Success / allowed / done.
pub const OK: Color = Color::Indexed(108);

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}
pub fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}
pub fn dim() -> Style {
    Style::default().fg(DIM)
}
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
pub fn danger() -> Style {
    Style::default().fg(DANGER)
}
pub fn ok() -> Style {
    Style::default().fg(OK)
}
pub fn selected() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

/// Map an agent colour to a terminal colour. No colour falls back to the accent.
pub fn agent_color(color: Option<AgentColor>) -> Color {
    match color {
        Some(AgentColor::Purple) => Color::Magenta,
        Some(AgentColor::Yellow) => Color::Yellow,
        Some(AgentColor::Red) => Color::Red,
        Some(AgentColor::Green) => Color::Green,
        Some(AgentColor::Blue) => Color::Blue,
        Some(AgentColor::Gray) => Color::Gray,
        Some(AgentColor::Orange) => Color::Indexed(208),
        None => ACCENT,
    }
}

/// Colour for a status state.
pub fn state_color(state: StatusState) -> Color {
    match state {
        StatusState::Working => ACCENT,
        StatusState::Blocked => Color::Yellow,
        StatusState::Done => OK,
        StatusState::Error => DANGER,
        StatusState::Idle => DIM,
    }
}
