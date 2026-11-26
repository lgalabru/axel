//! OSC 7577, the Program Resume Protocol, revision 0.5
//! (<https://www.superlogical.com/rex/docs/build/program-resume>).
//!
//! A program tells its terminal how to bring it back after the terminal
//! restarts:
//!
//! ```text
//! ESC ] 7577 ; pid=P:time=T:argv=B64,B64…[:cwd=B64]:mac=HEX ESC \
//! ESC ] 7577 ; pid=P:time=T:clear=1:mac=HEX ESC \
//! ```
//!
//! `mac` is the SHA-256 of the body before `:mac=`, then `:`, then the token
//! the terminal put in `$OSC7577_TOKEN`, as lowercase hex.  Without the
//! token there is no support and nothing is sent.  The token itself is never
//! written, and a program that reports takes it out of the environment of
//! every program it starts, so they don't report in its place.

use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use sha2::{Digest, Sha256};

/// Where the terminal puts the token; its presence is the feature detection.
pub const TOKEN_ENV: &str = "OSC7577_TOKEN";

/// The longest report every terminal must accept, the whole sequence
/// included (spec "Limits"): longer ones may be dropped, so we never send them.
pub const REPORT_BYTES_MAX: usize = 2048;

/// The most `argv` elements every terminal must accept (spec "Limits").
pub const ARGV_ELEMENTS_MAX: usize = 64;

/// The token's length, as the spec bounds it.
const TOKEN_CHARS_MIN: usize = 22;
const TOKEN_CHARS_MAX: usize = 64;

/// What a report asks of the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    /// Run `argv` again (in `cwd`) when the terminal comes back.
    Resume { argv: &'a [String], cwd: Option<&'a Path> },
    /// Forget this process's record: the user ended it.
    Clear,
}

/// Why a command can't be reported; the caller sends [`Action::Clear`]
/// instead, so an older record never brings back the wrong thing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unreportable {
    #[error("a command needs a program")]
    NoProgram,
    #[error("the program must be a bare name or an absolute path, not {0:?}")]
    RelativeProgram(String),
    #[error("the working directory must be an absolute path, not {0:?}")]
    RelativeCwd(String),
    #[error("an argument or directory holds a control character or isn't UTF-8")]
    ControlCharacter,
    #[error("more than {ARGV_ELEMENTS_MAX} arguments")]
    TooManyArguments,
    #[error("the report would take {0} bytes, more than {REPORT_BYTES_MAX}")]
    TooLong(usize),
    #[error("the token isn't one a terminal gives")]
    BadToken,
}

/// The complete `OSC 7577 … ST` sequence for `action`, from process `pid`,
/// at `time_secs` (Unix seconds), signed with `token`.
pub fn encode(pid: u32, time_secs: u64, action: Action<'_>, token: &str) -> Result<String, Unreportable> {
    if !valid_token(token) {
        return Err(Unreportable::BadToken);
    }
    assert!(pid > 0, "a report names a real process");
    assert!(time_secs > 0, "a report has a time");
    let mut body = format!("pid={pid}:time={time_secs}");
    match action {
        Action::Clear => body.push_str(":clear=1"),
        Action::Resume { argv, cwd } => {
            check_argv(argv)?;
            let elements: Vec<String> = argv.iter().map(|a| BASE64_STANDARD.encode(a)).collect();
            body.push_str(":argv=");
            body.push_str(&elements.join(","));
            if let Some(cwd) = cwd {
                let text = cwd.to_str().ok_or(Unreportable::ControlCharacter)?;
                if !cwd.is_absolute() {
                    return Err(Unreportable::RelativeCwd(text.to_string()));
                }
                check_text(text)?;
                body.push_str(":cwd=");
                body.push_str(&BASE64_STANDARD.encode(text));
            }
        }
    }
    let encoded = format!("\x1b]7577;{body}:mac={}\x1b\\", mac(&body, token));
    if encoded.len() > REPORT_BYTES_MAX {
        return Err(Unreportable::TooLong(encoded.len()));
    }
    debug_assert!(!encoded.contains(token), "the token is never sent");
    Ok(encoded)
}

/// `mac` over `body` (everything before `:mac=`) and `token`.
fn mac(body: &str, token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hasher.update(b":");
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    debug_assert_eq!(hex.len(), 64);
    hex
}

fn valid_token(token: &str) -> bool {
    (TOKEN_CHARS_MIN..=TOKEN_CHARS_MAX).contains(&token.len())
        && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn check_argv(argv: &[String]) -> Result<(), Unreportable> {
    let Some(program) = argv.first() else { return Err(Unreportable::NoProgram) };
    if argv.len() > ARGV_ELEMENTS_MAX {
        return Err(Unreportable::TooManyArguments);
    }
    let bare = !program.is_empty() && !program.contains('/') && !program.starts_with('-');
    if !(bare || Path::new(program).is_absolute()) {
        return Err(Unreportable::RelativeProgram(program.clone()));
    }
    argv.iter().try_for_each(|a| check_text(a))
}

/// The spec refuses C0 and C1 controls and DEL in what it decodes.
fn check_text(text: &str) -> Result<(), Unreportable> {
    let control = |c: char| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}');
    if text.chars().any(control) { Err(Unreportable::ControlCharacter) } else { Ok(()) }
}

/// The token this process was given, once [`take_token`] has run.
static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Read the token, and take it out of this process's environment, so
/// nothing this process starts inherits it: a program that reports keeps
/// the programs it runs from reporting in its place (spec "Security").
///
/// # Safety
///
/// Call it first thing in `main`, before any other thread exists: it edits
/// the process environment.
pub unsafe fn take_token() {
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    // SAFETY: the caller guarantees no other thread reads the environment yet.
    unsafe { std::env::remove_var(TOKEN_ENV) };
    // A second call finds the variable gone; the first value stays.
    let _ = TOKEN.set(token);
}

/// The token to sign with: the one [`take_token`] kept, else (in a process
/// that never took it) the environment's.
pub fn token() -> Option<String> {
    match TOKEN.get() {
        Some(kept) => kept.clone(),
        None => std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty()),
    }
}

/// Tell the terminal how to bring this process back, or that it shouldn't.
///
/// Returns whether a report went out: without `$OSC7577_TOKEN` (a terminal
/// without the feature) or a controlling terminal, nothing is sent.  A
/// command that can't be reported sends `clear` instead.
pub fn report(action: Action<'_>) -> std::io::Result<bool> {
    let Some(token) = token() else { return Ok(false) };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now == 0 {
        return Ok(false);
    }
    let pid = std::process::id();
    let sequence = match encode(pid, now, action, &token) {
        Ok(s) => s,
        Err(Unreportable::BadToken) => return Ok(false),
        Err(_) => match encode(pid, now, Action::Clear, &token) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        },
    };
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else { return Ok(false) };
    tty.write_all(sequence.as_bytes())?;
    tty.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "dummy-token-replace-me";
    const PID: u32 = 74877;
    const TIME: u64 = 1_795_543_200;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_the_specs_examples_byte_for_byte() {
        let nvim = args(&["nvim", "-S", "/home/user/.local/state/nvim/sessions/8f2c1d.vim"]);
        assert_eq!(
            encode(PID, TIME, Action::Resume { argv: &nvim, cwd: None }, TOKEN).unwrap(),
            "\x1b]7577;pid=74877:time=1795543200:argv=bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt:mac=9dbb99bfb856749a87563839d7ce9062818d78cf25909db93f309e0cd3b1909b\x1b\\"
        );
        assert_eq!(
            encode(PID, TIME, Action::Clear, TOKEN).unwrap(),
            "\x1b]7577;pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93\x1b\\"
        );
        let lazygit = args(&["lazygit"]);
        let cwd = Path::new("/home/user/src/project");
        assert_eq!(
            encode(PID, TIME, Action::Resume { argv: &lazygit, cwd: Some(cwd) }, TOKEN).unwrap(),
            "\x1b]7577;pid=74877:time=1795543200:argv=bGF6eWdpdA==:cwd=L2hvbWUvdXNlci9zcmMvcHJvamVjdA==:mac=35cb909805c380336efd3689a125acc774a9dec8422a1d21492ebec06ca758e1\x1b\\"
        );
    }

    #[test]
    fn refuses_what_a_terminal_would_reject() {
        let resume = |argv: &[String], cwd: Option<&Path>| encode(PID, TIME, Action::Resume { argv, cwd }, TOKEN);
        assert_eq!(resume(&[], None), Err(Unreportable::NoProgram));
        assert_eq!(resume(&args(&["./run.sh"]), None), Err(Unreportable::RelativeProgram("./run.sh".into())));
        assert_eq!(resume(&args(&["-x"]), None), Err(Unreportable::RelativeProgram("-x".into())));
        assert_eq!(resume(&args(&["nvim"]), Some(Path::new("project"))), Err(Unreportable::RelativeCwd("project".into())));
        assert_eq!(resume(&args(&["echo", "two\nlines"]), None), Err(Unreportable::ControlCharacter));
        assert_eq!(resume(&args(&["echo", "\u{9b}"]), None), Err(Unreportable::ControlCharacter), "C1 controls too");
        let many: Vec<String> = std::iter::once("echo".to_string()).chain((0..ARGV_ELEMENTS_MAX).map(|i| i.to_string())).collect();
        assert_eq!(resume(&many, None), Err(Unreportable::TooManyArguments));
        let long = args(&["echo", &"x".repeat(REPORT_BYTES_MAX)]);
        assert!(matches!(resume(&long, None), Err(Unreportable::TooLong(_))));
        assert_eq!(encode(PID, TIME, Action::Clear, "short"), Err(Unreportable::BadToken));
        assert_eq!(encode(PID, TIME, Action::Clear, &format!("{TOKEN};x")), Err(Unreportable::BadToken));
    }

    #[test]
    fn an_empty_argument_after_the_program_is_kept_and_nothing_is_shell_text() {
        let argv = args(&["/usr/bin/axel", "", "$(reboot)", "a; b"]);
        let sequence = encode(PID, TIME, Action::Resume { argv: &argv, cwd: None }, TOKEN).unwrap();
        let body = sequence.trim_start_matches("\x1b]7577;").trim_end_matches("\x1b\\");
        let elements = body.split(':').find_map(|p| p.strip_prefix("argv=")).unwrap();
        let decoded: Vec<String> =
            elements.split(',').map(|e| String::from_utf8(BASE64_STANDARD.decode(e).unwrap()).unwrap()).collect();
        assert_eq!(decoded, argv);
        assert!(!sequence.contains(TOKEN));
    }
}
