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

    // Branch names stay readable in a terminal tab and a `git branch` list at 40.
    const SLUG_CHARS_MAX: usize = 40;
    if out.chars().count() > SLUG_CHARS_MAX {
        let cut: String = out.chars().take(SLUG_CHARS_MAX).collect();
        out = match cut.rfind('-') {
            Some(idx) if idx >= SLUG_CHARS_MAX / 2 => cut[..idx].to_string(),
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
    use axel_i18n::t;
    let delta = now_secs.saturating_sub(ts_secs);
    if delta < 60 {
        return t!("just now").to_string();
    }
    if delta < 3600 {
        t!("{n}m ago", n = delta / 60)
    } else if delta < 86_400 {
        t!("{n}h ago", n = delta / 3600)
    } else if delta < 7 * 86_400 {
        t!("{n}d ago", n = delta / 86_400)
    } else if delta < 30 * 86_400 {
        t!("{n}w ago", n = delta / (7 * 86_400))
    } else if delta < 365 * 86_400 {
        t!("{n}mo ago", n = delta / (30 * 86_400))
    } else {
        t!("{n}y ago", n = delta / (365 * 86_400))
    }
}

/// Current Unix time in seconds.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current Unix time in milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
            score = score.saturating_add(10);
        }
        last_hit = Some(idx);
    }
    Some(score)
}

/// A count of terminal cells or rows as ratatui's `u16`, saturating: no
/// terminal is 65 535 cells wide, so a larger count only means "all of it".
pub(crate) fn cells(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// The cells `c` takes in a terminal: 2 for CJK, 0 for a combining mark or
/// a control character.
pub fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// The cells `s` takes in a terminal.
pub fn text_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` terminal cells, ending in `…` when cut. A
/// wide character is never split: when it would straddle the edge it goes,
/// and the ellipsis keeps a cell of its own.
pub fn ellipsize(s: &str, max: usize) -> String {
    if text_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let room = max - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > room {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    debug_assert!(text_width(&out) <= max, "an ellipsized text fits its room");
    out
}

/// The longest start of `s` that fits in `max` cells, never half a wide character.
pub fn fit_width(s: &str, max: usize) -> &str {
    let mut used = 0usize;
    for (i, c) in s.char_indices() {
        let w = char_width(c);
        if used + w > max {
            return &s[..i];
        }
        used += w;
    }
    s
}

/// `s` and spaces after it to fill `width` cells (`{:<width$}` counts
/// characters, which leaves a CJK column short).
pub fn pad_end(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(text_width(s))))
}

/// Spaces, then `s`, to fill `width` cells: a right-aligned column.
pub fn pad_start(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(text_width(s))))
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
        // A debugging aid: a key the log misses must not disturb the TUI.
        let _ = writeln!(f, "{} {ev:?}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// Undo [`enhance_keys`].
pub fn restore_keys(enhanced: bool) {
    if enhanced {
        // Teardown: a terminal that won't take the pop is already gone or ours no longer.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
    }
}


/// Most columns a thread's title takes: it names windows, dashboard rows and
/// the messages pane's header, where a sentence of a prompt doesn't fit.
pub const TITLE_COLS_MAX: usize = 60;

/// A thread's title from what was typed: its first non-blank line, links
/// shortened (`pay-web-ui#49`, `docs.rs`), cut at a word to
/// [`TITLE_COLS_MAX`] columns.  The whole text stays the intent, the
/// agent's first prompt.  Labels can't hold line breaks.
pub fn title_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let short: Vec<String> = line.split(' ').map(short_link).collect();
    let title = cut_at_word(&short.join(" "), TITLE_COLS_MAX);
    debug_assert!(text_width(&title) <= TITLE_COLS_MAX);
    title
}

/// A link as a title shows it: a GitHub pull request or issue as
/// `repo#49`, a GitHub repo as `owner/repo`, anything else as its host.
/// Punctuation after it (`…/pull/49,`) stays.  Other words pass through.
fn short_link(word: &str) -> String {
    let Some(rest) = word.strip_prefix("https://").or_else(|| word.strip_prefix("http://")) else { return word.to_string() };
    let trail_at = rest.trim_end_matches([',', '.', ';', ':', '!', '?', ')']).len();
    let (url, trail) = rest.split_at(trail_at);
    let mut parts = url.split('/').filter(|p| !p.is_empty());
    let host = parts.next().unwrap_or("").trim_start_matches("www.");
    let path: Vec<&str> = parts.collect();
    let short = match (host, path.as_slice()) {
        ("github.com", [_, repo, "pull" | "issues", number, ..]) => format!("{repo}#{number}"),
        ("github.com", [owner, repo, ..]) => format!("{owner}/{repo}"),
        _ if host.is_empty() => return word.to_string(),
        _ => host.to_string(),
    };
    format!("{short}{trail}")
}

/// `text` cut to `max` columns at the last word that fits, with `…`; a
/// single word longer than half of it is cut mid-word instead.
fn cut_at_word(text: &str, max: usize) -> String {
    if text_width(text) <= max {
        return text.to_string();
    }
    let room = max.saturating_sub(1);
    let mut cut = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > room {
            break;
        }
        cut.push(c);
        used += w;
    }
    if let Some(space) = cut.rfind(' ').filter(|&at| text_width(&cut[..at]) >= max / 2) {
        cut.truncate(space);
    }
    let mut out = cut.trim_end_matches([' ', ',', ';', ':', '-']).to_string();
    out.push('…');
    out
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

/// A link to what one turn changed: what a click on the change line under a
/// turn opens (`axel thread diff`), through the `axel-links://` handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLink {
    pub thread: String,
    pub session_id: String,
    pub turn: i64,
    /// The Rex session to open the diff pane in, and its server.
    pub rex_session: String,
    pub server: Option<String>,
}

impl DiffLink {
    /// `axel-links://diff?thread=…&session=…&turn=…&rex=…[&server=…]`, values percent-encoded.
    pub fn url(&self) -> String {
        let mut u = format!(
            "{}://diff?thread={}&session={}&turn={}&rex={}",
            ToolsLink::SCHEME,
            pct_encode(&self.thread),
            pct_encode(&self.session_id),
            self.turn,
            pct_encode(&self.rex_session)
        );
        if let Some(sv) = &self.server {
            u.push_str(&format!("&server={}", pct_encode(sv)));
        }
        u
    }

    pub fn parse(url: &str) -> Option<DiffLink> {
        let rest = url.strip_prefix(&format!("{}://diff?", ToolsLink::SCHEME))?;
        let (mut thread, mut session_id, mut turn, mut rex_session, mut server) = (None, None, None, None, None);
        for pair in rest.split('&') {
            let (k, v) = pair.split_once('=')?;
            let v = pct_decode(v)?;
            match k {
                "thread" => thread = Some(v),
                "session" => session_id = Some(v),
                "turn" => turn = v.parse().ok(),
                "rex" => rex_session = Some(v),
                "server" => server = Some(v),
                _ => {}
            }
        }
        Some(DiffLink { thread: thread?, session_id: session_id?, turn: turn?, rex_session: rex_session?, server })
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
    fn ellipsize_counts_cells_and_never_splits_a_wide_character() {
        // Six characters, twelve cells.
        assert_eq!(ellipsize("修复登录错误", 12), "修复登录错误");
        assert_eq!(ellipsize("修复登录错误", 11), "修复登录错…");
        // Five cells: two characters (4) and the ellipsis, not half a third.
        assert_eq!(ellipsize("修复登录错误", 5), "修复…");
        assert_eq!(ellipsize("修复登录错误", 4), "修…");
        assert_eq!(ellipsize("修复登录错误", 2), "…");
        assert_eq!(ellipsize("修复登录错误", 1), "…");
        assert_eq!(ellipsize("ab修复", 4), "ab…");
        for max in 0..14 {
            assert!(text_width(&ellipsize("修复 the 登录 bug", max)) <= max, "{max}");
        }
        assert_eq!(fit_width("ab修复", 3), "ab");
        assert_eq!(fit_width("ab修复", 4), "ab修");
        assert_eq!(pad_end("修复", 6), "修复  ");
        assert_eq!(pad_start("修复", 5), " 修复");
        assert_eq!(pad_end("修复登录", 2), "修复登录", "too wide: left as it is");
    }

    #[test]
    fn a_title_is_the_first_line_that_says_something() {
        assert_eq!(super::title_line("\n  fix the login bug  \nit fails on refresh"), "fix the login bug");
        // A long first line: links shortened, cut at a word, within the limit.
        let title = super::title_line("can you help me refine https://github.com/solana-foundation/pay-web-ui/pull/49, using our design skill stageshow. i want to polish the 2 first sections");
        assert!(title.starts_with("can you help me refine pay-web-ui#49, using our design"), "{title}");
        assert!(title.ends_with('…') && super::text_width(&title) <= super::TITLE_COLS_MAX, "{title}");
        assert_eq!(super::title_line("read https://docs.rs/tokio/latest and https://github.com/tokio-rs/tokio"), "read docs.rs and tokio-rs/tokio");
        assert_eq!(super::title_line(&"修".repeat(80)).chars().count(), super::TITLE_COLS_MAX / 2, "wide characters: half as many, ellipsis included");
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
        let diff = super::DiffLink { thread: "01a19dee".into(), session_id: "01S".into(), turn: 4, rex_session: "session:01a18a1a-98e9".into(), server: None };
        assert_eq!(diff.url(), "axel-links://diff?thread=01a19dee&session=01S&turn=4&rex=session%3A01a18a1a-98e9");
        assert_eq!(super::DiffLink::parse(&diff.url()), Some(diff));
        assert_eq!(super::DiffLink::parse("axel-links://diff?thread=t&session=s&turn=x&rex=r"), None, "the turn is a number");
        assert_eq!(super::ToolsLink::parse("axel-links://diff?thread=t&session=s&turn=1&rex=r"), None);
        let r = super::RevealLink { path: "/tmp/a b/Screenshot 01.03.png".into() };
        assert_eq!(super::RevealLink::parse(&r.url()), Some(r));
        assert_eq!(super::RevealLink::parse("axel-links://copy?item=x"), None);
        assert_eq!(super::hyperlink("axel://x", "5 tool calls"), "\x1b]8;;axel://x\x1b\\5 tool calls\x1b]8;;\x1b\\");
    }
}
