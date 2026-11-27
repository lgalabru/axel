//! The page a web pane shows, built into the binary: no files to install,
//! nothing fetched from anywhere but this pane's own server.

pub const PAGE: &str = include_str!("assets/page.html");
pub const CSS: &str = include_str!("assets/app.css");
pub const JS: &str = include_str!("assets/app.js");

/// JetBrains Mono 2.304 (SIL OFL 1.1, `assets/fonts/OFL.txt`), the font
/// Rex's terminal draws with by default (Ghostty's built-in one), which
/// Ladybird can only use from here: it isn't installed as a system font.
pub const FONTS: [(&str, &[u8]); 3] = [
    ("/fonts/JetBrainsMono-Regular.woff2", include_bytes!("assets/fonts/JetBrainsMono-Regular.woff2")),
    ("/fonts/JetBrainsMono-Bold.woff2", include_bytes!("assets/fonts/JetBrainsMono-Bold.woff2")),
    ("/fonts/JetBrainsMono-Italic.woff2", include_bytes!("assets/fonts/JetBrainsMono-Italic.woff2")),
];
