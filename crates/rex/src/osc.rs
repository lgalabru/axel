//! OSC 7501 — the Program Status Protocol, revision 0.4
//! (<https://www.superlogical.com/rex/docs/build/program-status>).
//!
//! ```text
//! ESC ] 7501 ; state=S[:id=..][:kind=..][:progress=N][:app=..][:title=B64][:msg=B64] ESC \
//! ```
//!
//! [`encode`] never produces a report a conforming terminal would discard:
//! free text is sanitised to one line and truncated to the decoded limits,
//! `kind` is sent only with `blocked`, `progress` only with `working` /
//! `blocked`, and an `id` / `app` outside its grammar is dropped rather than
//! sent (a bad `id` would make the whole report ignored).

use std::io::Write;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;

/// Decoded byte limit for `title`.
pub const TITLE_MAX_BYTES: usize = 192;
/// Decoded byte limit for `msg`.
pub const MSG_MAX_BYTES: usize = 2048;
/// Byte limit for `app` and for each `id` segment.
pub const SEGMENT_MAX_BYTES: usize = 32;
/// Byte limit for the whole `id`.
pub const ID_MAX_BYTES: usize = 128;
/// Maximum number of `/`-separated segments in an `id`.
pub const ID_MAX_DEPTH: usize = 8;

/// The feature-detection query; a supporting terminal replies with `OSC 7501 ; ? ... ST`.
pub const QUERY: &str = "\x1b]7501;?\x1b\\";

/// What the program says it is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum State {
    #[default]
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// Remove the record (and its children).
    Clear,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Blocked => "blocked",
            State::Error => "error",
            State::Clear => "clear",
        }
    }

    /// Parse one of the wire names.
    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "idle" => State::Idle,
            "working" => State::Working,
            "done" => State::Done,
            "blocked" => State::Blocked,
            "error" => State::Error,
            "clear" => State::Clear,
            _ => return None,
        })
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a `blocked` program is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Approval to do something.
    Permission,
    /// The user must type an answer.
    Question,
    /// A login, token or credential.
    Auth,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Question => "question",
            Kind::Auth => "auth",
        }
    }

    /// Parse one of the wire names.
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "permission" => Kind::Permission,
            "question" => Kind::Question,
            "auth" => Kind::Auth,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One status report.  Only `state` is required.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report<'a> {
    pub state: State,
    /// Record id, `segment("/"segment)*`; `None` addresses the root record.
    pub id: Option<&'a str>,
    /// Sent only when `state` is [`State::Blocked`].
    pub kind: Option<Kind>,
    /// 0–100, sent only when `state` is [`State::Working`] or [`State::Blocked`]; clamped.
    pub progress: Option<u8>,
    /// Stable program identifier such as `axel` or `claude-code`; `[A-Za-z0-9_.+-]{1,32}`.
    pub app: Option<&'a str>,
    /// Short label, ≤192 bytes decoded.
    pub title: Option<&'a str>,
    /// One line of text, ≤2048 bytes decoded.
    pub msg: Option<&'a str>,
}

impl<'a> Report<'a> {
    /// A root-record report with just a state.
    pub fn new(state: State) -> Report<'a> {
        Report {
            state,
            ..Report::default()
        }
    }
    pub fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }
    pub fn kind(mut self, kind: Kind) -> Self {
        self.kind = Some(kind);
        self
    }
    pub fn progress(mut self, pct: u8) -> Self {
        self.progress = Some(pct);
        self
    }
    pub fn app(mut self, app: &'a str) -> Self {
        self.app = Some(app);
        self
    }
    pub fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }
    pub fn msg(mut self, msg: &'a str) -> Self {
        self.msg = Some(msg);
        self
    }
}

fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-')
}

/// `[A-Za-z0-9_.+-]{1,32}`
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= SEGMENT_MAX_BYTES && s.chars().all(is_segment_char)
}

/// `segment ("/" segment)*`, ≤128 bytes, ≤8 levels.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= ID_MAX_BYTES && {
        let mut depth = 0;
        s.split('/').all(|seg| {
            depth += 1;
            depth <= ID_MAX_DEPTH && valid_segment(seg)
        })
    }
}

/// Control characters the protocol refuses in decoded text.
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}')
}

/// Make `text` a single line (newlines/tabs become spaces, other controls are
/// dropped, runs of whitespace are collapsed) and cut it to `max_bytes` on a
/// character boundary.
pub fn sanitize(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    let mut pending_space = false;
    for c in text.chars() {
        let c = match c {
            '\n' | '\r' | '\t' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => ' ',
            c if is_control(c) => continue,
            c => c,
        };
        if c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    truncate_bytes(&mut out, max_bytes);
    out
}

fn truncate_bytes(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Encode `r` as a complete `OSC 7501 ... ST` sequence.
pub fn encode(r: &Report) -> String {
    let mut body = format!("state={}", r.state.as_str());
    if let Some(id) = r.id
        && valid_id(id)
    {
        body.push_str(":id=");
        body.push_str(id);
    }
    if r.state == State::Blocked
        && let Some(kind) = r.kind
    {
        body.push_str(":kind=");
        body.push_str(kind.as_str());
    }
    if matches!(r.state, State::Working | State::Blocked)
        && let Some(p) = r.progress
    {
        body.push_str(&format!(":progress={}", p.min(100)));
    }
    if let Some(app) = r.app
        && valid_segment(app)
    {
        body.push_str(":app=");
        body.push_str(app);
    }
    if let Some(title) = r.title {
        let clean = sanitize(title, TITLE_MAX_BYTES);
        if !clean.is_empty() {
            body.push_str(":title=");
            body.push_str(&BASE64_STANDARD.encode(clean));
        }
    }
    if let Some(msg) = r.msg {
        let clean = sanitize(msg, MSG_MAX_BYTES);
        if !clean.is_empty() {
            body.push_str(":msg=");
            body.push_str(&BASE64_STANDARD.encode(clean));
        }
    }
    format!("\x1b]7501;{body}\x1b\\")
}

/// Write `r` to the controlling terminal (`/dev/tty`) so the hosting Rex block
/// updates its indicator.  Never fails: without a tty (cron, CI, a pipe) or on
/// a write error the report is simply dropped, which is what a status
/// indicator should do.
pub fn emit_to_tty(r: &Report) -> std::io::Result<()> {
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else {
        return Ok(());
    };
    let _ = tty
        .write_all(encode(r).as_bytes())
        .and_then(|()| tty.flush());
    Ok(())
}

/// Write `r` to any writer (for tests, or a PTY you already hold).
pub fn emit_to<W: Write>(w: &mut W, r: &Report) -> std::io::Result<()> {
    w.write_all(encode(r).as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        BASE64_STANDARD.encode(s)
    }

    #[test]
    fn minimal_report() {
        assert_eq!(
            encode(&Report::new(State::Working)),
            "\x1b]7501;state=working\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Clear)),
            "\x1b]7501;state=clear\x1b\\"
        );
        assert_eq!(QUERY, "\x1b]7501;?\x1b\\");
    }

    #[test]
    fn full_report_key_order() {
        let r = Report::new(State::Blocked)
            .id("build/test")
            .kind(Kind::Permission)
            .progress(40)
            .app("claude-code")
            .title("US East")
            .msg("Approve deploy?");
        assert_eq!(
            encode(&r),
            format!(
                "\x1b]7501;state=blocked:id=build/test:kind=permission:progress=40:app=claude-code:title={}:msg={}\x1b\\",
                b64("US East"),
                b64("Approve deploy?")
            )
        );
    }

    #[test]
    fn matches_spec_example() {
        // From the spec: state=blocked:kind=permission:app=terraform:msg=<b64("Apply 3 to add, 1 to change, 0 to destroy?")>
        let r = Report::new(State::Blocked)
            .kind(Kind::Permission)
            .app("terraform")
            .msg("Apply 3 to add, 1 to change, 0 to destroy?");
        assert_eq!(
            encode(&r),
            "\x1b]7501;state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/\x1b\\"
        );
    }

    #[test]
    fn kind_only_when_blocked() {
        let r = Report::new(State::Working).kind(Kind::Question);
        assert_eq!(encode(&r), "\x1b]7501;state=working\x1b\\");
        let r = Report::new(State::Blocked).kind(Kind::Auth);
        assert_eq!(encode(&r), "\x1b]7501;state=blocked:kind=auth\x1b\\");
    }

    #[test]
    fn progress_only_when_working_or_blocked_and_clamped() {
        assert_eq!(
            encode(&Report::new(State::Done).progress(50)),
            "\x1b]7501;state=done\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Idle).progress(50)),
            "\x1b]7501;state=idle\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Working).progress(250)),
            "\x1b]7501;state=working:progress=100\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Blocked).progress(0)),
            "\x1b]7501;state=blocked:progress=0\x1b\\"
        );
    }

    #[test]
    fn app_and_id_charset() {
        assert_eq!(
            encode(&Report::new(State::Idle).app("ok_app.v1+x-y")),
            "\x1b]7501;state=idle:app=ok_app.v1+x-y\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Idle).app("bad app")),
            "\x1b]7501;state=idle\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Idle).app("bad:app")),
            "\x1b]7501;state=idle\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Idle).app(&"a".repeat(33))),
            "\x1b]7501;state=idle\x1b\\"
        );
        assert_eq!(
            encode(&Report::new(State::Idle).app("")),
            "\x1b]7501;state=idle\x1b\\"
        );

        assert_eq!(
            encode(&Report::new(State::Idle).id("a/b/c")),
            "\x1b]7501;state=idle:id=a/b/c\x1b\\"
        );
        let nine_deep = ["x"; 9].join("/");
        for bad in ["", "a//b", "/a", "a/", "a b", nine_deep.as_str(), "é"] {
            assert_eq!(
                encode(&Report::new(State::Idle).id(bad)),
                "\x1b]7501;state=idle\x1b\\",
                "{bad:?}"
            );
        }
        // 8 levels ok, 9 not.
        assert!(valid_id(&["s"; 8].join("/")));
        assert!(!valid_id(&["s"; 9].join("/")));
        assert!(!valid_id(&"a".repeat(129)));
    }

    #[test]
    fn free_text_is_single_line_and_capped() {
        let r = Report::new(State::Working).msg("line one\nline\ttwo\r\n  three \u{7}");
        assert_eq!(
            encode(&r),
            format!(
                "\x1b]7501;state=working:msg={}\x1b\\",
                b64("line one line two three")
            )
        );

        let long = "x".repeat(5000);
        let enc = encode(&Report::new(State::Working).msg(&long));
        let payload = enc
            .strip_prefix("\x1b]7501;state=working:msg=")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let decoded = BASE64_STANDARD.decode(payload).unwrap();
        assert_eq!(decoded.len(), MSG_MAX_BYTES);

        let long_title = "é".repeat(200); // 400 bytes, must cut on a char boundary
        let enc = encode(&Report::new(State::Working).title(&long_title));
        let payload = enc
            .strip_prefix("\x1b]7501;state=working:title=")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let decoded = String::from_utf8(BASE64_STANDARD.decode(payload).unwrap()).unwrap();
        assert_eq!(decoded.len(), TITLE_MAX_BYTES);
        assert_eq!(decoded.chars().count(), 96);

        // Whitespace-only text is omitted entirely.
        assert_eq!(
            encode(&Report::new(State::Working).msg("  \n ")),
            "\x1b]7501;state=working\x1b\\"
        );
    }

    #[test]
    fn encoded_output_is_within_wire_limits() {
        let id = ["abcdefghijklmnop"; 7].join("/");
        let app = "a".repeat(32);
        let title = "t".repeat(300);
        let msg = "m".repeat(3000);
        let r = Report::new(State::Blocked)
            .id(&id)
            .kind(Kind::Question)
            .progress(99)
            .app(&app)
            .title(&title)
            .msg(&msg);
        let enc = encode(&r);
        assert!(enc.len() <= 4096, "{}", enc.len());
        // Body uses only the value charset (plus ':' and '=' separators).
        let body = enc
            .strip_prefix("\x1b]7501;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        assert!(
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.,+/=-:".contains(c))
        );
    }

    #[test]
    fn parse_roundtrips() {
        for s in [
            State::Idle,
            State::Working,
            State::Done,
            State::Blocked,
            State::Error,
            State::Clear,
        ] {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        for k in [Kind::Permission, Kind::Question, Kind::Auth] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(State::parse("nope"), None);
        assert_eq!(Kind::parse(""), None);
    }

    #[test]
    fn emit_to_writer() {
        let mut buf = Vec::new();
        emit_to(&mut buf, &Report::new(State::Done)).unwrap();
        assert_eq!(buf, b"\x1b]7501;state=done\x1b\\");
        // emit_to_tty is deliberately not exercised here: under `cargo test`
        // /dev/tty is the developer's real terminal.
    }
}
