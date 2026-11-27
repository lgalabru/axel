//! OSC 7577 (Program Resume) through the binary: what Axel tells the
//! terminal, signed with the token it was given, and what `axel session
//! resume` does when the terminal runs that again.  Each run gets a real
//! pseudo-terminal, so the report reaches `/dev/tty` as it would in Rex.
#![cfg(unix)]

use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axel_store::{SessionFilter, SessionUpdate, Store};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;

const TOKEN: &str = "test-token-0123456789abcdef";

/// Longest a run may take before the test gives up on it.
const RUN_SECS_MAX: u64 = 30;

/// A stand-in Claude Code for pane sessions: records its arguments and
/// whether it saw the token, then exits.
const FAKE_CLAUDE: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/claude.argv\"\nprintenv OSC7577_TOKEN > \"$HOME/claude.token\" || true\n";

/// A stand-in ACP agent that can load a conversation: it records every
/// request, and leaves at the first prompt or once a conversation is loaded.
const ACP_AGENT: &str = r#"#!/bin/sh
printenv OSC7577_TOKEN > "$HOME/agent.token" || true
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$HOME/agent.received"
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\("[^"]*"\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"native-1"}}\n' "$id" ;;
    *'"method":"session/load"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; sleep 1; exit 0 ;;
    *'"method":"session/prompt"'*) exit 0 ;;
  esac
done
"#;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        Home { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path().join("bin").join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
        c.env_clear()
            .env("HOME", self.path())
            .env("AXEL_HOME", self.path().join(".axel"))
            .env("AXEL_KEYSTORE", "file")
            .env("PATH", format!("{}:/usr/bin:/bin", self.path().join("bin").display()))
            .env("TERM", "xterm-256color")
            .current_dir(self.path())
            .args(args);
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.command(args).stdin(Stdio::null()).output().unwrap();
        assert!(o.status.success(), "axel {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn store(&self) -> Store {
        Store::open(&self.path().join(".axel").join("axel.db")).unwrap()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).unwrap_or_default()
    }
}

/// Run `cmd` on a fresh pseudo-terminal as its controlling terminal, with
/// `token` in its environment, and return everything it wrote there.
fn in_terminal(mut cmd: Command, token: Option<&str>) -> String {
    let (master, slave) = open_terminal();
    if let Some(token) = token {
        cmd.env("OSC7577_TOKEN", token);
    }
    run_on(cmd, master, slave, None)
}

/// Like [`in_terminal`], for a program that runs until its pane closes: it
/// is stopped once its output holds `until`.
fn in_terminal_until(mut cmd: Command, token: &str, until: &str) -> String {
    let (master, slave) = open_terminal();
    cmd.env("OSC7577_TOKEN", token);
    run_on(cmd, master, slave, Some(until))
}

fn open_terminal() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two descriptors into the integers given.
    let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(opened, 0, "openpty");
    // SAFETY: both descriptors were just opened and are owned here.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn run_on(mut cmd: Command, master: OwnedFd, slave: OwnedFd, until: Option<&str>) -> String {
    cmd.stdin(slave.try_clone().unwrap()).stdout(slave.try_clone().unwrap()).stderr(slave.try_clone().unwrap());
    // SAFETY: setsid and TIOCSCTTY are async-signal-safe; they make the
    // pseudo-terminal this child's controlling terminal, as a shell's is.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(slave);
    let mut reader = std::fs::File::from(master);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let output = std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        // The read ends with EIO once every writer is gone: that is the end.
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    let started = Instant::now();
    let pid = i32::try_from(child.id()).expect("a pid fits an i32");
    // The child leads its own session (setsid above), so its pid is its
    // process group: killing the group takes whatever it started too.
    let kill_all = || {
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    };
    while child.try_wait().unwrap().is_none() {
        let text = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
        if until.is_some_and(|u| text.contains(u)) {
            break;
        }
        if started.elapsed() > Duration::from_secs(RUN_SECS_MAX) {
            kill_all();
            panic!("the command did not finish: {text:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    kill_all();
    child.wait().ok();
    // A process that left the group can still hold the terminal open, and
    // then the read never ends: wait a moment for the last output, then go.
    let drained = Instant::now();
    while !output.is_finished() && drained.elapsed() < Duration::from_millis(DRAIN_MS_MAX) {
        std::thread::sleep(Duration::from_millis(20));
    }
    String::from_utf8_lossy(&seen.lock().unwrap()).into_owned()
}

/// How long the last output may take to arrive once the command is done.
const DRAIN_MS_MAX: u64 = 2000;

/// Every OSC 7577 report in `output`, checked against `token`, as the
/// decoded argv (empty for a `clear`) and cwd.
fn reports(output: &str, token: &str) -> Vec<(Vec<String>, Option<String>)> {
    output
        .split("\x1b]7577;")
        .skip(1)
        .map(|rest| {
            let body = &rest[..rest.find("\x1b\\").expect("a report is terminated")];
            let (signed, mac) = body.rsplit_once(":mac=").expect("mac comes last");
            assert_eq!(mac, sha256_hex(&format!("{signed}:{token}")), "the mac verifies");
            assert!(!body.contains(token), "the token is never sent");
            let field = |key: &str| signed.split(':').find_map(|p| p.strip_prefix(&format!("{key}=")).map(str::to_string));
            let decode = |b: &str| String::from_utf8(BASE64_STANDARD.decode(b).unwrap()).unwrap();
            let pid: u32 = field("pid").unwrap().parse().unwrap();
            assert!(pid > 0);
            let argv = field("argv").map(|a| a.split(',').map(decode).collect()).unwrap_or_default();
            (argv, field("cwd").map(|c| decode(&c)))
        })
        .collect()
}

fn sha256_hex(text: &str) -> String {
    let out = Command::new("/usr/bin/shasum").args(["-a", "256"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().and_then(|mut c| {
        use std::io::Write;
        c.stdin.take().unwrap().write_all(text.as_bytes())?;
        c.wait_with_output()
    });
    let out = out.or_else(|_| {
        let mut c = Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        use std::io::Write;
        c.stdin.take().unwrap().write_all(text.as_bytes())?;
        c.wait_with_output()
    });
    String::from_utf8(out.unwrap().stdout).unwrap()[..64].to_string()
}

fn ends_with(argv: &[String], tail: &[&str]) -> bool {
    argv.len() > tail.len() && argv[argv.len() - tail.len()..].iter().zip(tail).all(|(a, b)| a == b)
}

fn sessions(home: &Home) -> Vec<axel_store::Session> {
    let mut all = home.store().list_sessions(SessionFilter::default()).unwrap();
    all.sort_by_key(|s| s.started_at);
    all
}

/// Give `session` a harness conversation id, as its harness would have.
fn set_native_id(home: &Home, session: &axel_store::Session, native: &str) {
    let keys = axel_identity::Keystore::open_at(home.path().join(".axel").join("secrets.json"), false).unwrap();
    let key = keys.agent(&session.agent_pubkey.parse().unwrap()).unwrap().unwrap();
    let update = SessionUpdate { harness_session_id: Some(native.into()), ..Default::default() };
    home.store().update_session(&key, &session.id, &update).unwrap();
}

#[test]
fn a_pane_session_reports_how_to_resume_it_and_its_harness_never_sees_the_token() {
    let home = Home::new();
    home.script("claude", FAKE_CLAUDE);
    home.ok(&["agent", "new", "opal", "claude"]);
    let out = in_terminal(home.command(&["thread", "new", "fix it", "--agent", "opal", "--scratch", "--runtime", "pane"]), Some(TOKEN));
    let first = sessions(&home).pop().expect("a session");
    let sent = reports(&out, TOKEN);
    assert!(sent.iter().any(|(argv, cwd)| ends_with(argv, &["session", "resume", &first.id]) && cwd.as_deref().is_some_and(|c| c.starts_with('/'))), "{sent:?}");
    assert_eq!(home.read("claude.token"), "", "the harness got no token");

    // Rex comes back and runs it: the harness resumes its conversation, and
    // the new session reports in turn.
    set_native_id(&home, &first, "native-pane-1");
    let out = in_terminal(home.command(&["session", "resume", &first.id]), Some(TOKEN));
    let argv = home.read("claude.argv");
    assert!(argv.lines().collect::<Vec<_>>().windows(2).any(|w| w == ["--resume", "native-pane-1"]), "{argv}");
    assert!(!argv.contains("fix it"), "the intent isn't sent again: {argv}");
    let all = sessions(&home);
    let second = all.last().unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(all.iter().find(|s| s.id == first.id).unwrap().state, axel_store::SessionState::Ended);
    assert!(reports(&out, TOKEN).iter().any(|(argv, _)| ends_with(argv, &["session", "resume", &second.id])), "{out}");
}

#[test]
fn a_managed_session_comes_back_in_its_pane_without_its_prompt_sent_again() {
    let home = Home::new();
    let agent = home.script("acp-agent", ACP_AGENT);
    home.ok(&["agent", "new", "gus", agent.to_str().unwrap()]);
    let out = in_terminal(home.command(&["thread", "new", "fix it", "--agent", "gus", "--scratch", "--runtime", "managed"]), Some(TOKEN));
    let first = sessions(&home).pop().expect("a session");
    assert!(reports(&out, TOKEN).iter().any(|(argv, _)| ends_with(argv, &["session", "resume", &first.id])), "{out}");
    assert_eq!(home.read("agent.token"), "", "the ACP agent got no token");
    set_native_id(&home, &first, "native-1");
    std::fs::remove_file(home.path().join("agent.received")).ok();

    let out = in_terminal(home.command(&["session", "resume", &first.id]), Some(TOKEN));
    let received = home.read("agent.received");
    assert!(received.contains(r#""method":"session/load""#) && received.contains("native-1"), "{received}");
    assert!(!received.contains(r#""method":"session/prompt""#), "nothing is said again: {received}");
    let second = sessions(&home).pop().unwrap();
    assert_ne!(second.id, first.id);
    assert!(reports(&out, TOKEN).iter().any(|(argv, _)| ends_with(argv, &["session", "resume", &second.id])), "{out}");
}

#[test]
fn without_a_token_nothing_is_reported_and_a_session_with_no_conversation_isnt_resumed() {
    let home = Home::new();
    home.script("claude", FAKE_CLAUDE);
    home.ok(&["agent", "new", "opal", "claude"]);
    let out = in_terminal(home.command(&["thread", "new", "fix it", "--agent", "opal", "--scratch", "--runtime", "pane"]), None);
    assert!(!out.contains("\x1b]7577;"), "{out:?}");
    let first = sessions(&home).pop().unwrap();
    let refused = home.command(&["session", "resume", &first.id]).stdin(Stdio::null()).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no conversation to continue"));
}

#[test]
fn the_thread_views_panes_come_back_as_they_are() {
    let home = Home::new();
    home.script("claude", FAKE_CLAUDE);
    home.ok(&["agent", "new", "opal", "claude"]);
    home.command(&["thread", "new", "fix it", "--agent", "opal", "--scratch", "--runtime", "pane"]).stdin(Stdio::null()).output().unwrap();
    let thread = sessions(&home).pop().unwrap().thread_id;
    for pane in [vec!["thread", "stream", thread.as_str()], vec!["ui", "--screen", "usage", "--thread", thread.as_str()]] {
        let out = in_terminal_until(home.command(&pane), TOKEN, "\x1b\\");
        let sent = reports(&out, TOKEN);
        assert!(sent.iter().any(|(argv, _)| ends_with(argv, &pane)), "{pane:?}: {sent:?}");
    }
}
