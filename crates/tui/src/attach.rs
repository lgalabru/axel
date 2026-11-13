//! File-drop attachments. In a terminal a drag-and-drop arrives as a bracketed
//! paste of one or more paths; [`parse_dropped_paths`] recognises that shape so
//! the paste can become attachment chips instead of text.

use std::path::{Path, PathBuf};

use crate::util::expand_tilde;

/// Split a paste into tokens on newlines and unquoted, unescaped whitespace.
/// Supports `'single'` / `"double"` quotes and backslash escapes.
fn tokenize(paste: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in paste.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if escaped {
        cur.push('\\');
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Decode `%XX` escapes (invalid sequences are kept verbatim).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turn one token into a path: `file://` URLs are decoded, `~` expanded.
fn token_to_path(token: &str) -> PathBuf {
    if let Some(rest) = token.strip_prefix("file://") {
        // file:///abs/path or file://localhost/abs/path
        let path = match rest.find('/') {
            Some(0) => rest.to_string(),
            Some(i) => rest[i..].to_string(),
            None => rest.to_string(),
        };
        return PathBuf::from(percent_decode(&path));
    }
    expand_tilde(token)
}

/// Interpret `paste` as dropped file paths. Returns `Some(paths)` only when the
/// paste is non-empty and **every** token resolves (canonicalised) to an existing
/// file; otherwise `None`, and the caller treats the paste as text. A directory
/// is text: it's a place you're naming, and there's nothing in it to attach.
pub fn parse_dropped_paths(paste: &str) -> Option<Vec<PathBuf>> {
    let tokens = tokenize(paste);
    if tokens.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        let p = token_to_path(&t);
        if !p.is_absolute() && !t.starts_with('~') && !t.starts_with("file://") && !t.starts_with('/') && !t.starts_with('.') {
            // Bare words ("hello world") are text, not relative paths.
            return None;
        }
        let canon = std::fs::canonicalize(&p).ok().filter(|c| c.is_file())?;
        if !out.contains(&canon) {
            out.push(canon);
        }
    }
    Some(out)
}

/// File name of an attachment for a chip.
pub fn chip_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Shorten `name` to `max` chars with a middle ellipsis, keeping the extension
/// (`2D1AF3B4-91C7-4C4A…EF.png`).
pub fn middle_ellipsize(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    // Extension = from the last '.' (not leading), at most 8 chars incl. the dot.
    let ext_start = name.rfind('.').filter(|&i| i > 0 && chars.len() - name[..i].chars().count() <= 8);
    let tail: Vec<char> = match ext_start {
        Some(i) => name[i..].chars().collect(),
        None => Vec::new(),
    };
    let room = max - 1; // for the ellipsis
    if !tail.is_empty() && tail.len() < room {
        let head_len = room - tail.len();
        let head: String = chars[..head_len].iter().collect();
        return format!("{head}…{}", tail.iter().collect::<String>());
    }
    // Extension alone does not leave room for a head: plain middle cut.
    let front = room.div_ceil(2);
    let back = room - front;
    format!("{}…{}", chars[..front].iter().collect::<String>(), chars[chars.len() - back..].iter().collect::<String>())
}

/// Display columns of a chip line (📎 is double width).
fn chip_cols(s: &str) -> usize {
    s.chars().count() + s.matches('📎').count()
}

/// One rendered chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipPart {
    /// `📎 name`, plus ` ✕` when selected.
    pub text: String,
    /// Index into the attachments vec.
    pub index: usize,
    pub selected: bool,
}

/// Lay chips out for a row `width` columns wide. A lone chip may use the whole
/// row; when several chips share it their names are middle-ellipsized evenly,
/// and chips are dropped (`+N`, returned as `hidden`) only when even short names
/// do not fit. The `selected` chip is always among the shown ones.
pub fn chip_parts(attachments: &[PathBuf], width: usize, selected: Option<usize>) -> (Vec<ChipPart>, usize) {
    if attachments.is_empty() || width == 0 {
        return (Vec::new(), attachments.len());
    }
    const MIN_NAME: usize = 8;
    let total = attachments.len();
    let names: Vec<String> = attachments.iter().map(|p| chip_name(p)).collect();
    let mut shown = total;
    loop {
        let hidden = total - shown;
        if shown == 0 {
            return (Vec::new(), hidden);
        }
        // Window so the selected chip is visible.
        let start = selected.map(|s| (s + 1).saturating_sub(shown)).unwrap_or(0).min(total - shown);
        let idxs: Vec<usize> = (start..start + shown).collect();
        let suffix_cols = if hidden > 0 { 2 + format!("+{hidden}").len() } else { 0 };
        let sel_cols = if selected.is_some_and(|s| idxs.contains(&s)) { 2 } else { 0 };
        // Fixed cost: "📎 " (3 cols) per chip, "  " between chips, " ✕" on the selected, "  +N" when hidden.
        let fixed = shown * 3 + (shown - 1) * 2 + sel_cols + suffix_cols;
        let natural: usize = idxs.iter().map(|&i| names[i].chars().count()).sum();
        let budget = width.saturating_sub(fixed);
        let labels: Vec<String> = if natural <= budget {
            idxs.iter().map(|&i| names[i].clone()).collect()
        } else {
            let per = budget / shown;
            if per < MIN_NAME {
                shown -= 1;
                continue;
            }
            let mut spare: usize = idxs.iter().map(|&i| per.saturating_sub(names[i].chars().count())).sum();
            idxs.iter()
                .map(|&i| {
                    let len = names[i].chars().count();
                    let mut max = per;
                    if len > per {
                        let extra = spare.min(len - per);
                        max += extra;
                        spare -= extra;
                    }
                    middle_ellipsize(&names[i], max)
                })
                .collect()
        };
        let parts: Vec<ChipPart> = idxs
            .iter()
            .zip(labels)
            .map(|(&i, label)| {
                let sel = selected == Some(i);
                ChipPart { text: format!("📎 {label}{}", if sel { " ✕" } else { "" }), index: i, selected: sel }
            })
            .collect();
        let line_cols: usize = parts.iter().map(|p| chip_cols(&p.text)).sum::<usize>() + (shown - 1) * 2 + suffix_cols;
        if line_cols <= width {
            return (parts, hidden);
        }
        shown -= 1;
    }
}

/// `📎 a.png  📎 b.txt  +N` fitted to `width` (no selection).
pub fn chips_line(attachments: &[PathBuf], width: usize) -> String {
    let (parts, hidden) = chip_parts(attachments, width, None);
    let mut line = parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("  ");
    if hidden > 0 {
        if !line.is_empty() {
            line.push_str("  ");
        }
        line.push_str(&format!("+{hidden}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("axel-tui-attach-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap()
    }

    #[test]
    fn tokenizer_handles_quotes_and_escapes() {
        assert_eq!(tokenize("a b\tc\nd"), vec!["a", "b", "c", "d"]);
        assert_eq!(tokenize("'/x/My File.png' \"/y/Other File.txt\""), vec!["/x/My File.png", "/y/Other File.txt"]);
        assert_eq!(tokenize(r"/x/My\ File.png"), vec!["/x/My File.png"]);
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("My%20File%2Bx.png"), "My File+x.png");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn parse_single_quoted_escaped_and_file_url() {
        let t = Tmp::new("basic");
        let plain = t.file("plain.txt");
        let spaced = t.file("My File.png");
        let expect_plain = canon(&plain);
        let expect_spaced = canon(&spaced);

        assert_eq!(parse_dropped_paths(plain.to_str().unwrap()), Some(vec![expect_plain.clone()]));
        assert_eq!(parse_dropped_paths(&format!("'{}'", spaced.display())), Some(vec![expect_spaced.clone()]));
        assert_eq!(parse_dropped_paths(&format!("\"{}\"", spaced.display())), Some(vec![expect_spaced.clone()]));
        assert_eq!(parse_dropped_paths(&spaced.display().to_string().replace(' ', "\\ ")), Some(vec![expect_spaced.clone()]));
        let url = format!("file://{}", spaced.display().to_string().replace(' ', "%20"));
        assert_eq!(parse_dropped_paths(&url), Some(vec![expect_spaced.clone()]));
        let url_host = format!("file://localhost{}", plain.display());
        assert_eq!(parse_dropped_paths(&url_host), Some(vec![expect_plain.clone()]));
        // A directory is text, alone or with files.
        assert_eq!(parse_dropped_paths(t.0.to_str().unwrap()), None);
        assert_eq!(parse_dropped_paths(&format!("file://{}", t.0.display())), None);
        assert_eq!(parse_dropped_paths(&format!("{} {}", plain.display(), t.0.display())), None);
    }

    #[test]
    fn parse_multi_line_mixed_and_dedup() {
        let t = Tmp::new("multi");
        let a = t.file("a.txt");
        let b = t.file("b.txt");
        let paste = format!("{}\n{}\n", a.display(), b.display());
        assert_eq!(parse_dropped_paths(&paste), Some(vec![canon(&a), canon(&b)]));
        let paste = format!("{} {}", a.display(), a.display());
        assert_eq!(parse_dropped_paths(&paste), Some(vec![canon(&a)]), "duplicates collapse");
        // one missing path makes the whole paste text
        let paste = format!("{}\n{}", a.display(), t.0.join("missing.txt").display());
        assert_eq!(parse_dropped_paths(&paste), None);
        assert_eq!(parse_dropped_paths("hello world"), None);
        assert_eq!(parse_dropped_paths(""), None);
        assert_eq!(parse_dropped_paths("/definitely/not/here"), None);
    }

    #[test]
    fn parse_tilde_expansion() {
        let Some(home) = dirs::home_dir() else { return };
        let name = format!(".axel-tui-attach-test-{}", std::process::id());
        let p = home.join(&name);
        if std::fs::write(&p, b"x").is_err() {
            return; // home not writable here; nothing to test
        }
        let got = parse_dropped_paths(&format!("~/{name}"));
        let _ = std::fs::remove_file(&p);
        assert_eq!(got.map(|v| v.len()), Some(1));
    }

    #[test]
    fn chips_fit_width() {
        let paths = vec![PathBuf::from("/x/a.png"), PathBuf::from("/x/b.txt"), PathBuf::from("/x/c.rs")];
        assert_eq!(chips_line(&paths, 80), "📎 a.png  📎 b.txt  📎 c.rs");
        assert_eq!(chips_line(&paths, 20), "📎 a.png  +2");
        assert_eq!(chips_line(&paths, 3), "+3");
        assert_eq!(chips_line(&[], 80), "");
    }

    #[test]
    fn lone_chip_uses_full_width_and_names_shorten_in_the_middle() {
        let long = PathBuf::from("/x/2D1AF3B4-91C7-4C4A-B359-0123456789AB.png");
        // alone with room: full name
        assert_eq!(chips_line(std::slice::from_ref(&long), 60), "📎 2D1AF3B4-91C7-4C4A-B359-0123456789AB.png");
        // alone but narrow: middle ellipsis keeping the extension, exactly filling the row
        let narrow = chips_line(std::slice::from_ref(&long), 24);
        assert_eq!(narrow, "📎 2D1AF3B4-91C7-4C….png");
        assert_eq!(chip_cols(&narrow), 24);
        // two long names share the row evenly instead of dropping one
        let two = vec![long.clone(), PathBuf::from("/x/another-very-long-screenshot-name.jpeg")];
        let shared = chips_line(&two, 50);
        assert!(shared.matches('📎').count() == 2 && !shared.contains('+'), "{shared}");
        assert!(shared.contains("….png") && shared.contains("….jpeg"), "{shared}");
        assert!(chip_cols(&shared) <= 50, "{shared}");
        // a short name gives its spare columns to the long one
        let mixed = vec![PathBuf::from("/x/a.md"), long.clone()];
        let line = chips_line(&mixed, 40);
        assert!(line.starts_with("📎 a.md  📎 2D1AF3B4"), "{line}");
        assert!(chip_cols(&line) <= 40, "{line}");
        // selection: marked with ✕, kept visible in a narrow row
        let three = vec![PathBuf::from("/x/a.png"), PathBuf::from("/x/b.txt"), PathBuf::from("/x/c.rs")];
        let (parts, hidden) = chip_parts(&three, 80, Some(1));
        assert_eq!(hidden, 0);
        assert_eq!(parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["📎 a.png", "📎 b.txt ✕", "📎 c.rs"]);
        assert!(parts[1].selected && !parts[0].selected);
        let (parts, hidden) = chip_parts(&three, 13, Some(2));
        assert_eq!(hidden, 2);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].index, 2, "the selected chip is the visible one");
        assert_eq!(parts[0].text, "📎 c.rs ✕");
        // middle_ellipsize details
        assert_eq!(middle_ellipsize("short.rs", 20), "short.rs");
        assert_eq!(middle_ellipsize("abcdefghij.png", 9), "abcd….png");
        assert_eq!(middle_ellipsize("abcdefghijkl", 7), "abc…jkl");
        assert_eq!(middle_ellipsize("x", 0), "");
        assert_eq!(middle_ellipsize("xyz", 1), "…");
    }
}
