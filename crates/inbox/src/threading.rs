//! NIP-10 style thread markers on `e` tags.
//!
//! Ported from Buzz `nip10.rs`. One parser for the `root`/`reply` markers so
//! every consumer reads ancestry the same way.
//!
//! A marker counts only when its id is exactly 64 ASCII-hex characters (the
//! shape of an envelope id). A malformed id such as `["e","bad","","reply"]`
//! is ignored, never treated as a thread link.

/// The `root` and `reply` ids parsed from an envelope's `e` tags.
///
/// Each is `Some(id)` only when a marker of that kind carried a valid 64-hex
/// id. The last valid occurrence of each marker wins.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ThreadMarkers {
    /// Id from a valid `["e", <64-hex>, <relay>, "root"]` tag.
    pub root: Option<String>,
    /// Id from a valid `["e", <64-hex>, <relay>, "reply"]` tag.
    pub reply: Option<String>,
}

impl ThreadMarkers {
    /// Collapse the markers into a reply's `(root_id, parent_id)`.
    ///
    /// - `root` + `reply` → `(root, reply)`: a nested reply names both.
    /// - `reply` only → `(reply, reply)`: a direct reply to the root.
    /// - `root` only or neither → `None`: no `reply` marker means the message
    ///   is top-level (a lone `root` tag never anchors a reply).
    pub fn resolve(&self) -> Option<(String, String)> {
        match (&self.root, &self.reply) {
            (Some(root), Some(reply)) => Some((root.clone(), reply.clone())),
            (None, Some(reply)) => Some((reply.clone(), reply.clone())),
            (Some(_), None) | (None, None) => None,
        }
    }
}

/// True when `id` is exactly 64 ASCII-hex characters.
pub fn is_message_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse the `root`/`reply` markers from a tag list.
///
/// Only `e` tags with a marker (`len() >= 4`) and a valid 64-hex id are
/// considered; everything else is ignored.
pub fn parse_thread_markers<S: AsRef<str>>(tags: &[Vec<S>]) -> ThreadMarkers {
    let mut markers = ThreadMarkers::default();
    for parts in tags {
        if parts.len() >= 4 && parts[0].as_ref() == "e" && is_message_id(parts[1].as_ref()) {
            match parts[3].as_ref() {
                "root" => markers.root = Some(parts[1].as_ref().to_string()),
                "reply" => markers.reply = Some(parts[1].as_ref().to_string()),
                _ => {}
            }
        }
    }
    markers
}

/// Build the two `e` tags for a reply: `["e", root, "", "root"]` and
/// `["e", parent, "", "reply"]`.
pub fn reply_tags(root: &str, parent: &str) -> [Vec<String>; 2] {
    [
        vec!["e".into(), root.into(), String::new(), "root".into()],
        vec!["e".into(), parent.into(), String::new(), "reply".into()],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        "a".repeat(64)
    }

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_e_tags_yields_no_markers() {
        let tags: Vec<Vec<String>> = vec![tag(&["p", "abc"])];
        assert_eq!(parse_thread_markers(&tags), ThreadMarkers::default());
        let empty: Vec<Vec<String>> = vec![];
        assert_eq!(parse_thread_markers(&empty), ThreadMarkers::default());
    }

    #[test]
    fn root_and_reply_both_parsed() {
        let tags = vec![
            tag(&["e", &id(), "", "root"]),
            tag(&["e", &"b".repeat(64), "", "reply"]),
        ];
        let m = parse_thread_markers(&tags);
        assert_eq!(m.root.as_deref(), Some(id().as_str()));
        assert_eq!(m.reply.as_deref(), Some("b".repeat(64).as_str()));
    }

    #[test]
    fn reply_only_marker_parsed() {
        let tags = vec![tag(&["e", &id(), "", "reply"])];
        let m = parse_thread_markers(&tags);
        assert_eq!(m.reply.as_deref(), Some(id().as_str()));
        assert!(m.root.is_none());
    }

    #[test]
    fn bare_e_tag_without_marker_is_ignored() {
        let tags = vec![tag(&["e", &id()]), tag(&["e", &id(), ""])];
        assert_eq!(parse_thread_markers(&tags), ThreadMarkers::default());
    }

    #[test]
    fn unknown_marker_is_ignored() {
        let tags = vec![tag(&["e", &id(), "", "mention"])];
        assert_eq!(parse_thread_markers(&tags), ThreadMarkers::default());
    }

    #[test]
    fn malformed_id_is_ignored_for_both_markers() {
        let tags = vec![
            tag(&["e", "bad", "", "reply"]),
            tag(&["e", "also-bad", "", "root"]),
            tag(&["e", &"g".repeat(64), "", "root"]),
            tag(&["e", &"a".repeat(63), "", "root"]),
        ];
        assert_eq!(parse_thread_markers(&tags), ThreadMarkers::default());
    }

    #[test]
    fn valid_root_with_malformed_reply_is_top_level() {
        let tags = vec![
            tag(&["e", &id(), "", "root"]),
            tag(&["e", "bad", "", "reply"]),
        ];
        let m = parse_thread_markers(&tags);
        assert_eq!(m.root.as_deref(), Some(id().as_str()));
        assert!(m.reply.is_none());
        assert_eq!(m.resolve(), None);
    }

    #[test]
    fn last_valid_marker_wins() {
        let tags = vec![
            tag(&["e", &"1".repeat(64), "", "reply"]),
            tag(&["e", &"2".repeat(64), "", "reply"]),
        ];
        assert_eq!(parse_thread_markers(&tags).reply.unwrap(), "2".repeat(64));
    }

    #[test]
    fn resolve_root_and_reply_keeps_both() {
        let m = ThreadMarkers {
            root: Some("1".repeat(64)),
            reply: Some("2".repeat(64)),
        };
        assert_eq!(m.resolve(), Some(("1".repeat(64), "2".repeat(64))));
    }

    #[test]
    fn resolve_reply_only_is_direct_reply_to_root() {
        let m = ThreadMarkers {
            root: None,
            reply: Some(id()),
        };
        assert_eq!(m.resolve(), Some((id(), id())));
    }

    #[test]
    fn resolve_root_only_or_nothing_is_top_level() {
        let m = ThreadMarkers {
            root: Some(id()),
            reply: None,
        };
        assert_eq!(m.resolve(), None);
        assert_eq!(ThreadMarkers::default().resolve(), None);
    }

    #[test]
    fn reply_tags_roundtrip_through_parser() {
        let tags = reply_tags(&"1".repeat(64), &"2".repeat(64)).to_vec();
        let m = parse_thread_markers(&tags);
        assert_eq!(m.resolve(), Some(("1".repeat(64), "2".repeat(64))));
    }

    #[test]
    fn works_with_str_slices_too() {
        let a = id();
        let tags: Vec<Vec<&str>> = vec![vec!["e", &a, "", "root"]];
        assert_eq!(
            parse_thread_markers(&tags).root.as_deref(),
            Some(a.as_str())
        );
    }
}
