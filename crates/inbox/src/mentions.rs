//! `@name` and bare-pubkey mention resolution.
//!
//! Ported from Buzz `mentions.rs`, minus NIP-27 `nostr:` URIs and npubs.
//! Pure functions, no I/O.
//!
//! ```text
//! text ──► strip_code_regions ──┬──► extract_at_names ──► match_names_to_profiles ─┐
//!                               └──► extract_pubkey_tokens (bare base58 32-byte) ──┤
//!                                                                                  ▼
//!                                                        dedupe, cap MENTION_CAP ──► pubkeys
//! ```
//!
//! An `@name` only counts when the `@` is at the start of the text or
//! preceded by ASCII whitespace (so `user@host` is not a mention). Names are
//! matched against profiles case-insensitively and exactly. A bare base58
//! token that decodes to exactly 32 bytes (optionally prefixed with `@`) is
//! a direct mention of that pubkey. Code spans and fenced blocks are
//! ignored.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Maximum number of mention `p` tags on a single message.
pub const MENTION_CAP: usize = 50;

/// A mentionable identity: its base58 pubkey and the `@name` it answers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Base58 public key.
    pub pubkey: String,
    /// Agent name, matched case-insensitively.
    pub name: String,
}

impl Profile {
    /// Convenience constructor.
    pub fn new(pubkey: impl Into<String>, name: impl Into<String>) -> Profile {
        Profile {
            pubkey: pubkey.into(),
            name: name.into(),
        }
    }
}

/// Resolve every mention in `text` to a pubkey.
///
/// `@name` tokens are matched against `profiles` (case-insensitive, exact
/// name, profile order); bare base58 pubkeys are taken as-is. Code spans and
/// fenced blocks are skipped. The result is deduplicated, in first-seen
/// order (name matches first, then bare pubkeys), and capped at
/// [`MENTION_CAP`].
pub fn resolve_mentions(text: &str, profiles: &[Profile]) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let stripped = strip_code_regions(text);
    let names = extract_at_names(&stripped);
    let mut out = match_names_to_profiles(&names, profiles);
    let direct = extract_pubkey_tokens(&stripped);

    let mut seen: HashSet<String> = HashSet::new();
    out.retain(|pk| seen.insert(pk.clone()));
    for pk in direct {
        if out.len() >= MENTION_CAP {
            break;
        }
        if seen.insert(pk.clone()) {
            out.push(pk);
        }
    }
    out.truncate(MENTION_CAP);
    out
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// Extract `@name` tokens from `text`.
///
/// Returns lowercased names in first-seen order, deduplicated. An `@` only
/// starts a mention at the start of the text or after ASCII whitespace.
/// Allowed name characters: ASCII alphanumerics, `.`, `-`, `_`; trailing
/// `.`, `-`, `_` are trimmed so `@alice.` mentions `alice`.
pub fn extract_at_names(text: &str) -> Vec<String> {
    if text.is_empty() || !text.contains('@') {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for (i, _) in text.match_indices('@') {
        let preceded = i == 0 || text.as_bytes()[i - 1].is_ascii_whitespace();
        if !preceded {
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        let name = rest[..end].trim_end_matches(['.', '-', '_']);
        if name.is_empty() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if seen.insert(lower.clone()) {
            names.push(lower);
        }
    }
    names
}

/// Match extracted names against profiles.
///
/// Case-insensitive, exact match on `Profile::name`. Output order is
/// profile order. Profiles with empty names never match. Several profiles
/// with the same name all match (ambiguity is the caller's problem; agent
/// names are unique in the store).
pub fn match_names_to_profiles(names: &[String], profiles: &[Profile]) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    profiles
        .iter()
        .filter(|p| !p.name.is_empty())
        .filter(|p| names.iter().any(|n| n.eq_ignore_ascii_case(&p.name)))
        .map(|p| p.pubkey.clone())
        .collect()
}

fn is_base58_char(c: char) -> bool {
    c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l')
}

/// Extract bare base58 pubkeys from `text`.
///
/// A whitespace-delimited token (optionally prefixed with `@`, trailing
/// punctuation trimmed) of 32–44 base58 characters that decodes to exactly
/// 32 bytes is a pubkey. Returns the tokens as written, deduplicated, in
/// first-seen order.
pub fn extract_pubkey_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text.split_ascii_whitespace() {
        let token = raw
            .trim_start_matches(['@', '(', '[', '<', '"', '\''])
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\'']);
        if !(32..=44).contains(&token.len()) || !token.chars().all(is_base58_char) {
            continue;
        }
        let Ok(bytes) = bs58::decode(token).into_vec() else {
            continue;
        };
        if bytes.len() != 32 {
            continue;
        }
        if seen.insert(token.to_string()) {
            out.push(token.to_string());
        }
    }
    out
}

/// Replace fenced code blocks (```…```) and inline code spans (`…`) with a
/// single space, preserving everything else and valid UTF-8.
///
/// Used only for mention scanning; the original text is stored verbatim.
pub fn strip_code_regions(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.char_indices().peekable();

    while let Some(&(i, ch)) = chars.peek() {
        // Fenced code block: ``` at line start (possibly after whitespace).
        if ch == '`' && content[i..].starts_with("```") {
            let is_fence_start = if i == 0 {
                true
            } else {
                let before = &content[..i];
                before.ends_with('\n')
                    || before.chars().all(|c| c.is_ascii_whitespace())
                    || before.rsplit_once('\n').is_some_and(|(_, after_nl)| {
                        after_nl.chars().all(|c| c.is_ascii_whitespace())
                    })
            };

            if is_fence_start {
                let after_fence = i + 3;
                let rest = &content[after_fence..];
                let line_end = rest
                    .find('\n')
                    .map_or(content.len(), |p| after_fence + p + 1);

                let mut search_from = line_end;
                let close_end = loop {
                    if search_from >= content.len() {
                        break content.len();
                    }
                    if let Some(pos) = content[search_from..].find("```") {
                        let abs_pos = search_from + pos;
                        let at_line_start = abs_pos == 0
                            || content.as_bytes()[abs_pos - 1] == b'\n'
                            || content[..abs_pos]
                                .rsplit_once('\n')
                                .is_some_and(|(_, after_nl)| {
                                    after_nl.chars().all(|c| c.is_ascii_whitespace())
                                });
                        if at_line_start {
                            let after_close = abs_pos + 3;
                            let end = content[after_close..]
                                .find('\n')
                                .map_or(content.len(), |p| after_close + p + 1);
                            break end;
                        }
                        search_from = abs_pos + 3;
                    } else {
                        break content.len();
                    }
                };

                out.push(' ');
                while let Some(&(ci, _)) = chars.peek() {
                    if ci >= close_end {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
        }

        // Inline code span: `…` on one line.
        if ch == '`' {
            let after_tick = i + 1;
            if after_tick < content.len()
                && let Some(rel_end) = content[after_tick..].find('`')
            {
                let close_pos = after_tick + rel_end;
                if !content[after_tick..close_pos].contains('\n') {
                    out.push(' ');
                    while let Some(&(ci, _)) = chars.peek() {
                        if ci > close_pos {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
            }
        }

        out.push(ch);
        chars.next();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(seed: u8) -> String {
        bs58::encode(&[seed; 32]).into_string()
    }

    fn profiles() -> Vec<Profile> {
        vec![
            Profile::new(pk(1), "atlas"),
            Profile::new(pk(2), "Bob"),
            Profile::new(pk(3), "john.doe"),
            Profile::new(pk(4), "mary_jane"),
            Profile::new(pk(5), "bob-smith"),
        ]
    }

    // --- extract_at_names ---------------------------------------------------

    #[test]
    fn extract_at_names_matches_basic() {
        assert_eq!(extract_at_names("hello @alice"), vec!["alice"]);
        assert_eq!(extract_at_names("@bob hello"), vec!["bob"]);
    }

    #[test]
    fn extract_at_names_lowercases_and_dedups() {
        assert_eq!(
            extract_at_names("@Alice and @alice, meet @Bob"),
            vec!["alice", "bob"]
        );
    }

    #[test]
    fn extract_at_names_allows_newline_and_tab_prefix() {
        assert_eq!(extract_at_names("line1\n@tyler line2"), vec!["tyler"]);
        assert_eq!(extract_at_names("x\t@tab"), vec!["tab"]);
    }

    #[test]
    fn extract_at_names_allows_punctuation_in_names() {
        assert_eq!(
            extract_at_names("@john.doe @mary_jane @bob-smith"),
            vec!["john.doe", "mary_jane", "bob-smith"]
        );
    }

    #[test]
    fn extract_at_names_trims_trailing_punctuation() {
        assert_eq!(extract_at_names("thanks @alice."), vec!["alice"]);
        assert_eq!(extract_at_names("@alice, @bob!"), vec!["alice", "bob"]);
        assert_eq!(extract_at_names("@alice: hi"), vec!["alice"]);
        assert_eq!(extract_at_names("(@alice)"), Vec::<String>::new()); // '(' before '@'
        assert!(extract_at_names("@...").is_empty());
    }

    #[test]
    fn extract_at_names_rejects_email_and_empty() {
        assert!(extract_at_names("").is_empty());
        assert!(extract_at_names("no mentions").is_empty());
        assert!(extract_at_names("user@example.com").is_empty());
        assert!(extract_at_names("hello @ world").is_empty());
        assert!(extract_at_names("hello @").is_empty());
        assert!(extract_at_names("@@alice").is_empty());
    }

    #[test]
    fn extract_at_names_unicode_does_not_panic() {
        assert!(extract_at_names("@日本語 hello").is_empty());
        assert_eq!(extract_at_names("こんにちは @alice 世界"), vec!["alice"]);
        assert_eq!(extract_at_names("@alice日本"), vec!["alice"]);
    }

    // --- match_names_to_profiles -------------------------------------------

    #[test]
    fn match_is_case_insensitive_and_exact() {
        let names = vec!["bob".to_string()];
        assert_eq!(match_names_to_profiles(&names, &profiles()), vec![pk(2)]);
        // "bob" does not match "bob-smith".
        let names = vec!["bo".to_string()];
        assert!(match_names_to_profiles(&names, &profiles()).is_empty());
        let names = vec!["BOB-SMITH".to_string()];
        assert_eq!(match_names_to_profiles(&names, &profiles()), vec![pk(5)]);
    }

    #[test]
    fn match_preserves_profile_order() {
        let names = vec!["john.doe".to_string(), "atlas".to_string()];
        assert_eq!(
            match_names_to_profiles(&names, &profiles()),
            vec![pk(1), pk(3)]
        );
    }

    #[test]
    fn match_returns_all_duplicates_and_skips_empty_names() {
        let ps = vec![
            Profile::new("a", "Alice"),
            Profile::new("b", "alice"),
            Profile::new("c", ""),
        ];
        let names = vec!["alice".to_string(), "".to_string()];
        assert_eq!(match_names_to_profiles(&names, &ps), vec!["a", "b"]);
        assert!(match_names_to_profiles(&[], &ps).is_empty());
    }

    // --- extract_pubkey_tokens --------------------------------------------

    #[test]
    fn pubkey_tokens_bare_and_prefixed() {
        let k = pk(9);
        assert_eq!(
            extract_pubkey_tokens(&format!("ping {k} now")),
            vec![k.clone()]
        );
        assert_eq!(
            extract_pubkey_tokens(&format!("ping @{k}")),
            vec![k.clone()]
        );
        assert_eq!(
            extract_pubkey_tokens(&format!("cc {k}, ok?")),
            vec![k.clone()]
        );
        assert_eq!(extract_pubkey_tokens(&format!("({k})")), vec![k.clone()]);
        assert_eq!(
            extract_pubkey_tokens(&format!("{k} and {k}")),
            vec![k.clone()]
        );
    }

    #[test]
    fn pubkey_tokens_reject_non_pubkeys() {
        assert!(extract_pubkey_tokens("hello world").is_empty());
        // Right alphabet, wrong decoded length (31 and 33 bytes).
        let short = bs58::encode(&[7u8; 31]).into_string();
        let long = bs58::encode(&[7u8; 33]).into_string();
        assert!(extract_pubkey_tokens(&format!("{short} {long}")).is_empty());
        // Contains a non-base58 character.
        let mut bad = pk(9);
        bad.replace_range(0..1, "0");
        assert!(extract_pubkey_tokens(&bad).is_empty());
        // A 40-char hex string is not base58 (contains 0) — and a 64-hex id
        // is too long anyway.
        assert!(extract_pubkey_tokens(&"a".repeat(64)).is_empty());
        assert!(extract_pubkey_tokens(&"0".repeat(40)).is_empty());
    }

    // --- strip_code_regions -----------------------------------------------

    #[test]
    fn strip_code_regions_removes_fenced_block() {
        let input = "before\n```rust\nlet x = 1; @alice\n```\nafter";
        let stripped = strip_code_regions(input);
        assert!(!stripped.contains("let x = 1"));
        assert!(!stripped.contains("@alice"));
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
    }

    #[test]
    fn strip_code_regions_removes_inline_code() {
        let stripped = strip_code_regions("see `@alice` here");
        assert!(!stripped.contains("alice"));
        assert!(stripped.contains("see"));
        assert!(stripped.contains("here"));
    }

    #[test]
    fn strip_code_regions_preserves_prose_and_unicode() {
        assert_eq!(
            strip_code_regions("hello @alice world"),
            "hello @alice world"
        );
        assert_eq!(strip_code_regions(""), "");
        let stripped = strip_code_regions("こんにちは `code` 世界");
        assert!(stripped.contains("こんにちは"));
        assert!(stripped.contains("世界"));
        assert!(!stripped.contains("code"));
    }

    #[test]
    fn strip_code_regions_unclosed_backtick_preserved() {
        assert!(strip_code_regions("hello `world").contains("world"));
        // Backtick pair spanning lines is not an inline span.
        let s = strip_code_regions("a `b\nc` d");
        assert!(s.contains('b') && s.contains('c'));
    }

    #[test]
    fn strip_code_regions_unclosed_fence_eats_to_end() {
        let s = strip_code_regions("intro\n```\n@alice never closed");
        assert!(s.contains("intro"));
        assert!(!s.contains("alice"));
    }

    // --- resolve_mentions -------------------------------------------------

    #[test]
    fn resolve_basic_name() {
        assert_eq!(
            resolve_mentions("@atlas please review", &profiles()),
            vec![pk(1)]
        );
        assert_eq!(resolve_mentions("hey @ATLAS!", &profiles()), vec![pk(1)]);
        assert_eq!(resolve_mentions("hey @Bob.", &profiles()), vec![pk(2)]);
    }

    #[test]
    fn resolve_multiple_names_in_profile_order_deduped() {
        let out = resolve_mentions("@john.doe and @atlas and @John.Doe", &profiles());
        assert_eq!(out, vec![pk(1), pk(3)]);
    }

    #[test]
    fn resolve_unknown_name_is_dropped() {
        assert!(resolve_mentions("@nobody here", &profiles()).is_empty());
        assert!(resolve_mentions("@atlas", &[]).is_empty());
        assert!(resolve_mentions("", &profiles()).is_empty());
        assert!(resolve_mentions("no mentions at all", &profiles()).is_empty());
    }

    #[test]
    fn resolve_skips_code_spans_and_blocks() {
        assert!(resolve_mentions("use `@atlas` literally", &profiles()).is_empty());
        assert!(resolve_mentions("```\n@atlas\n```", &profiles()).is_empty());
        assert_eq!(
            resolve_mentions("`@bob` but @atlas for real", &profiles()),
            vec![pk(1)]
        );
    }

    #[test]
    fn resolve_email_is_not_a_mention() {
        assert!(resolve_mentions("mail atlas@example.com", &profiles()).is_empty());
    }

    #[test]
    fn resolve_bare_pubkey_is_direct_mention() {
        let unknown = pk(42);
        assert_eq!(
            resolve_mentions(&format!("ping {unknown}"), &profiles()),
            vec![unknown.clone()]
        );
        assert_eq!(
            resolve_mentions(&format!("ping @{unknown}"), &profiles()),
            vec![unknown.clone()]
        );
        // Inside code it is ignored.
        assert!(resolve_mentions(&format!("`{unknown}`"), &profiles()).is_empty());
    }

    #[test]
    fn resolve_name_and_pubkey_of_same_identity_dedupe() {
        let text = format!("@atlas and {} again", pk(1));
        assert_eq!(resolve_mentions(&text, &profiles()), vec![pk(1)]);
    }

    #[test]
    fn resolve_names_come_before_bare_pubkeys() {
        let text = format!("{} then @atlas", pk(42));
        assert_eq!(resolve_mentions(&text, &profiles()), vec![pk(1), pk(42)]);
    }

    #[test]
    fn resolve_caps_at_mention_cap() {
        // 60 distinct profiles all mentioned by name.
        let ps: Vec<Profile> = (0..60u8)
            .map(|i| Profile::new(pk(i), format!("agent{i}")))
            .collect();
        let text: String = (0..60).map(|i| format!("@agent{i} ")).collect();
        let out = resolve_mentions(&text, &ps);
        assert_eq!(out.len(), MENTION_CAP);
        assert_eq!(out[0], pk(0));
        assert_eq!(out[49], pk(49));

        // Names fill the cap; bare pubkeys cannot push past it.
        let text = format!("{text} {}", pk(200));
        let out = resolve_mentions(&text, &ps);
        assert_eq!(out.len(), MENTION_CAP);
        assert!(!out.contains(&pk(200)));

        // Bare pubkeys alone are capped too.
        let text: String = (0..60u8).map(|i| format!("{} ", pk(i))).collect();
        assert_eq!(resolve_mentions(&text, &[]).len(), MENTION_CAP);
    }

    #[test]
    fn resolve_unicode_content_does_not_panic() {
        assert_eq!(
            resolve_mentions("こんにちは @atlas 世界", &profiles()),
            vec![pk(1)]
        );
        assert!(resolve_mentions("@日本語", &profiles()).is_empty());
        assert!(resolve_mentions("é@atlas", &profiles()).is_empty());
    }
}
