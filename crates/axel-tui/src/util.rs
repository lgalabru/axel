//! Small pure helpers shared by the launcher and inbox: slugs, relative
//! timestamps and the fuzzy matcher used by list filters.

/// Turn a free-form title into a branch-friendly slug.
///
/// Lowercases, maps every run of non-alphanumeric characters to a single `-`,
/// trims leading/trailing dashes and caps the result at 40 characters (cut on a
/// dash boundary when possible). An empty result becomes `"task"`.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_alphanumeric() {
            // Non-ASCII letters/digits: keep them lowercased, they are valid in git refs.
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_dash = true;
        }
    }

    const MAX: usize = 40;
    if out.chars().count() > MAX {
        let cut: String = out.chars().take(MAX).collect();
        out = match cut.rfind('-') {
            Some(idx) if idx >= MAX / 2 => cut[..idx].to_string(),
            _ => cut,
        };
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "task".to_string() } else { out }
}

/// Human relative time such as `"just now"`, `"5m ago"`, `"2h ago"`, `"3d ago"`.
///
/// Both arguments are Unix seconds. Timestamps in the future (clock skew) are
/// reported as `"just now"`.
pub fn relative_time(ts_secs: i64, now_secs: i64) -> String {
    let delta = now_secs.saturating_sub(ts_secs);
    if delta < 60 {
        return "just now".to_string();
    }
    let (n, unit) = if delta < 3600 {
        (delta / 60, "m")
    } else if delta < 86_400 {
        (delta / 3600, "h")
    } else if delta < 7 * 86_400 {
        (delta / 86_400, "d")
    } else if delta < 30 * 86_400 {
        (delta / (7 * 86_400), "w")
    } else if delta < 365 * 86_400 {
        (delta / (30 * 86_400), "mo")
    } else {
        (delta / (365 * 86_400), "y")
    };
    format!("{n}{unit} ago")
}

/// Current Unix time in seconds.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Case-insensitive fuzzy match: every character of `needle` must appear in
/// `haystack` in order (not necessarily adjacent). An empty needle matches.
/// Returns a score (higher is better) or `None` when it does not match.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<u32> {
    let needle: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    let hay_str: String = hay.iter().collect();
    let needle_str: String = needle.iter().collect();
    // Exact substring wins outright.
    if let Some(pos) = hay_str.find(&needle_str) {
        return Some(1000 - pos.min(500) as u32);
    }
    let mut score: u32 = 100;
    let mut hi = 0usize;
    let mut last_hit: Option<usize> = None;
    for nc in &needle {
        let mut found = None;
        while hi < hay.len() {
            if hay[hi] == *nc {
                found = Some(hi);
                hi += 1;
                break;
            }
            hi += 1;
        }
        let idx = found?;
        if let Some(prev) = last_hit
            && idx == prev + 1
        {
            score += 10;
        }
        last_hit = Some(idx);
    }
    Some(score)
}

/// Truncate `s` to at most `max` characters, appending `…` when cut.
pub fn ellipsize(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Compact token count: `950`, `12.4k`, `1.2M`.
pub fn format_tokens(n: i64) -> String {
    let n = n.max(0) as f64;
    if n < 1000.0 {
        format!("{}", n as i64)
    } else if n < 10_000.0 {
        format!("{:.1}k", n / 1000.0)
    } else if n < 1_000_000.0 {
        format!("{}k", (n / 1000.0).round() as i64)
    } else if n < 10_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else {
        format!("{}M", (n / 1_000_000.0).round() as i64)
    }
}

/// Sum `(at, value)` samples into `buckets` equal time buckets spanning
/// `since..=now`, oldest first. Samples outside the window are dropped.
pub fn bucket_series(samples: &[(i64, i64)], since: i64, now: i64, buckets: usize) -> Vec<u64> {
    let mut out = vec![0u64; buckets];
    if buckets == 0 || now <= since {
        return out;
    }
    let span = (now - since) as f64;
    for &(at, v) in samples {
        if at < since || at > now || v <= 0 {
            continue;
        }
        let idx = (((at - since) as f64 / span) * buckets as f64) as usize;
        let idx = idx.min(buckets - 1);
        out[idx] = out[idx].saturating_add(v as u64);
    }
    out
}

/// Replace a leading `~` with the user's home directory.
pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest.trim_start_matches('/'));
    }
    std::path::PathBuf::from(path)
}

/// Shorten a path for display: home becomes `~`.
pub fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        let rest = rest.to_string_lossy();
        return if rest.is_empty() { "~".to_string() } else { format!("~/{rest}") };
    }
    path.to_string_lossy().into_owned()
}

/// Ask the terminal to report keys unambiguously (the kitty keyboard
/// protocol, which Rex speaks), so Shift+Enter arrives as itself rather than
/// as Enter.  Returns whether it was turned on, for [`restore_keys`].
pub fn enhance_keys() -> bool {
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    if !crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        return false;
    }
    crossterm::execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)).is_ok()
}

/// While `~/.axel/logs/keys.on` exists, each key and paste as the TUI
/// receives it, appended to `~/.axel/logs/keys.log`: to see what a key
/// really arrives as. Off by default, since it records what's typed.
pub fn log_key(ev: &crossterm::event::Event) {
    use crossterm::event::Event;
    if !matches!(ev, Event::Key(_) | Event::Paste(_)) {
        return;
    }
    let Some(dir) = dirs::home_dir().map(|h| h.join(".axel/logs")) else { return };
    if !dir.join("keys.on").exists() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("keys.log")) {
        let _ = writeln!(f, "{} {ev:?}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// Undo [`enhance_keys`].
pub fn restore_keys(enhanced: bool) {
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
    }
}


/// A thread's title from what was typed: its first non-blank line (the
/// whole text is the intent, the agent's first prompt). Labels can't hold
/// line breaks.
pub fn title_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// A link to one run of tool calls (a fold) on a thread: what a click on a
/// fold line in the conversation pane opens, through the `axel-links://` handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolsLink {
    pub thread: String,
    /// The fold's first tool call item id.
    pub fold: String,
    /// The Rex session to open the details pane in, and its server.
    pub session: String,
    pub server: Option<String>,
}

impl ToolsLink {
    /// Its own scheme: `axel://` belongs to the Axel app's sign-in callback.
    pub const SCHEME: &'static str = "axel-links";

    /// `axel-links://tools?thread=…&fold=…&session=…[&server=…]`, values percent-encoded.
    pub fn url(&self) -> String {
        let mut u = format!(
            "{}://tools?thread={}&fold={}&session={}",
            Self::SCHEME,
            pct_encode(&self.thread),
            pct_encode(&self.fold),
            pct_encode(&self.session)
        );
        if let Some(sv) = &self.server {
            u.push_str(&format!("&server={}", pct_encode(sv)));
        }
        u
    }

    pub fn parse(url: &str) -> Option<ToolsLink> {
        let rest = url.strip_prefix(&format!("{}://tools?", Self::SCHEME))?;
        let mut thread = None;
        let mut fold = None;
        let mut session = None;
        let mut server = None;
        for pair in rest.split('&') {
            let (k, v) = pair.split_once('=')?;
            let v = pct_decode(v)?;
            match k {
                "thread" => thread = Some(v),
                "fold" => fold = Some(v),
                "session" => session = Some(v),
                "server" => server = Some(v),
                _ => {}
            }
        }
        Some(ToolsLink { thread: thread?, fold: fold?, session: session?, server })
    }
}

/// A link that copies the command a tool call ran: `axel-links://copy?item=…`.
/// It names the call, not the command, so it stays short whatever the command,
/// and the handler copies it exactly as the agent ran it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyLink {
    /// The tool call's conversation item id.
    pub item: String,
}

impl CopyLink {
    pub fn url(&self) -> String {
        format!("{}://copy?item={}", ToolsLink::SCHEME, pct_encode(&self.item))
    }

    pub fn parse(url: &str) -> Option<CopyLink> {
        let rest = url.strip_prefix(&format!("{}://copy?", ToolsLink::SCHEME))?;
        let item = rest.split('&').find_map(|pair| pair.strip_prefix("item=")).and_then(pct_decode)?;
        (!item.is_empty()).then_some(CopyLink { item })
    }
}

/// A link that shows an attached file in Finder: `axel-links://reveal?path=…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealLink {
    pub path: String,
}

impl RevealLink {
    pub fn url(&self) -> String {
        format!("{}://reveal?path={}", ToolsLink::SCHEME, pct_encode(&self.path))
    }

    pub fn parse(url: &str) -> Option<RevealLink> {
        let rest = url.strip_prefix(&format!("{}://reveal?", ToolsLink::SCHEME))?;
        let path = rest.split('&').find_map(|pair| pair.strip_prefix("path=")).and_then(pct_decode)?;
        (!path.is_empty()).then_some(RevealLink { path })
    }
}

/// `text` as a terminal hyperlink (OSC 8) to `url`.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Fix the login bug"), "fix-the-login-bug");
        assert_eq!(slugify("  Hello,   World! "), "hello-world");
        assert_eq!(slugify("Add OAuth2 / PKCE support"), "add-oauth2-pkce-support");
    }

    #[test]
    fn slugify_empty_and_symbols() {
        assert_eq!(slugify(""), "task");
        assert_eq!(slugify("!!! ???"), "task");
        assert_eq!(slugify("---a---"), "a");
    }

    #[test]
    fn slugify_truncates_on_boundary() {
        let long = "this is a very long title that keeps going and going forever more";
        let slug = slugify(long);
        assert!(slug.chars().count() <= 40, "{slug}");
        assert!(!slug.ends_with('-'));
        assert_eq!(slug, "this-is-a-very-long-title-that-keeps");
    }

    #[test]
    fn slugify_unicode() {
        assert_eq!(slugify("Café Menü"), "café-menü");
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now + 50, now), "just now");
        assert_eq!(relative_time(now - 59, now), "just now");
        assert_eq!(relative_time(now - 60, now), "1m ago");
        assert_eq!(relative_time(now - 5 * 60, now), "5m ago");
        assert_eq!(relative_time(now - 2 * 3600, now), "2h ago");
        assert_eq!(relative_time(now - 3 * 86_400, now), "3d ago");
        assert_eq!(relative_time(now - 2 * 7 * 86_400, now), "2w ago");
        assert_eq!(relative_time(now - 45 * 86_400, now), "1mo ago");
        assert_eq!(relative_time(now - 400 * 86_400, now), "1y ago");
    }

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("", "anything").is_some());
        assert!(fuzzy_score("axl", "axel").is_some());
        assert!(fuzzy_score("AXEL", "~/code/axel").is_some());
        assert!(fuzzy_score("zz", "axel").is_none());
        // substring beats scattered
        assert!(fuzzy_score("axel", "axel-next").unwrap() > fuzzy_score("axel", "a x e l").unwrap());
    }

    #[test]
    fn format_tokens_scales() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_400), "12k");
        assert_eq!(format_tokens(2_040), "2.0k");
        assert_eq!(format_tokens(310_000), "310k");
        assert_eq!(format_tokens(1_250_000), "1.2M");
        assert_eq!(format_tokens(-5), "0");
    }

    #[test]
    fn bucket_series_sums_into_window() {
        let s = vec![(0, 5), (10, 1), (59, 2), (60, 7), (61, 100), (-1, 100)];
        assert_eq!(bucket_series(&s, 0, 60, 6), vec![5, 1, 0, 0, 0, 9]);
        assert_eq!(bucket_series(&s, 0, 0, 6), vec![0; 6]);
        assert_eq!(bucket_series(&s, 0, 60, 0), Vec::<u64>::new());
    }

    #[test]
    fn ellipsize_cuts() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("hello world", 5), "hell…");
        assert_eq!(ellipsize("hello", 0), "");
    }

    #[test]
    fn a_title_is_the_first_line_that_says_something() {
        assert_eq!(super::title_line("\n  fix the login bug  \nit fails on refresh"), "fix the login bug");
        assert_eq!(super::title_line("one line"), "one line");
        assert_eq!(super::title_line(" \n "), "");
    }

    #[test]
    fn a_tools_link_round_trips_through_its_url() {
        let l = super::ToolsLink {
            thread: "01a19dee".into(),
            fold: "item-7".into(),
            session: "session:01a18a1a-98e9".into(),
            server: Some("unix:///Users/ludo/Library/Application%20Support/rex/server.sock".into()),
        };
        let url = l.url();
        assert!(url.starts_with("axel-links://tools?thread=01a19dee&fold=item-7&session=session%3A01a18a1a-98e9&server=unix%3A%2F%2F%2F"), "{url}");
        assert!(url.contains("Application%2520Support"), "a % in a value is encoded too: {url}");
        assert_eq!(super::ToolsLink::parse(&url), Some(l));
        assert_eq!(super::ToolsLink::parse("https://example.com"), None);
        assert_eq!(super::ToolsLink::parse("axel-links://tools?thread=t"), None, "fold and session are needed");
        assert_eq!(super::ToolsLink::parse("axel://tools?thread=t&fold=f&session=s"), None, "not ours: the app's own scheme");
        let copy = super::CopyLink { item: "01M6F4203Q&x".into() };
        assert_eq!(copy.url(), "axel-links://copy?item=01M6F4203Q%26x");
        assert_eq!(super::CopyLink::parse(&copy.url()), Some(copy));
        assert_eq!(super::CopyLink::parse("axel-links://copy?item="), None);
        assert_eq!(super::ToolsLink::parse("axel-links://copy?item=x"), None, "a copy link isn't a tools link");
        let r = super::RevealLink { path: "/tmp/a b/Screenshot 01.03.png".into() };
        assert_eq!(super::RevealLink::parse(&r.url()), Some(r));
        assert_eq!(super::RevealLink::parse("axel-links://copy?item=x"), None);
        assert_eq!(super::hyperlink("axel://x", "5 tool calls"), "\x1b]8;;axel://x\x1b\\5 tool calls\x1b]8;;\x1b\\");
    }
}
