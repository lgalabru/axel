//! Web panes (RFD 0007) through the binary: `axel web pane` serves the
//! thread and hands its address to Ladybird's terminal mode; without
//! Ladybird, or when it can't draw, the pane falls back to the terminal
//! view; `axel panes` turns web panes on.  A stand-in Ladybird
//! (`$AXEL_LADYBIRD`) loads the page the way the real one would, with `curl`,
//! and keeps the Ladybird installed on the machine out of the tests.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A stand-in Ladybird: records its arguments, then loads the page
/// it was given (following the secret's redirect, keeping the cookie) and
/// saves what it got.
const FAKE_BROWSER: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/browser.argv"
url=""
for a in "$@"; do url="$a"; done
curl -s -L -c "$HOME/jar" -b "$HOME/jar" "$url" > "$HOME/page.html"
base=$(printf '%s' "$url" | sed 's|/?k=.*||')
curl -s -b "$HOME/jar" "$base/api/thread" > "$HOME/thread.json"
curl -s -o /dev/null -w '%{http_code}' "$base/api/thread" > "$HOME/no-cookie.status"
"#;

/// A Ladybird that can't draw here: it gives up at once.
const REFUSING_BROWSER: &str = "#!/bin/sh\necho 'no kitty graphics here' >&2\nexit 1\n";

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

    fn browser(&self, body: &str) {
        let path = self.path().join("bin").join("ladybird");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run(&self, args: &[&str], panes: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
        c.env_clear()
            .env("HOME", self.path())
            .env("AXEL_HOME", self.path().join(".axel"))
            .env("AXEL_KEYSTORE", "file")
            .env("PATH", format!("{}:/usr/bin:/bin", self.path().join("bin").display()))
            .env("AXEL_LADYBIRD", self.path().join("bin").join("ladybird"))
            .args(args)
            .stdin(Stdio::null());
        if let Some(p) = panes {
            c.env("AXEL_PANES", p);
        }
        c.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args, None);
        assert!(o.status.success(), "axel {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn thread(&self) -> String {
        self.ok(&["agent", "new", "opal", "claude"]);
        self.ok(&["thread", "new", "add retries", "--agent", "opal", "--scratch", "--no-launch"]);
        let store = axel_store::Store::open(&self.path().join(".axel").join("axel.db")).unwrap();
        store.list_threads(axel_store::ThreadFilter::default()).unwrap().pop().unwrap().id
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).unwrap_or_default()
    }
}

#[test]
fn a_web_pane_serves_its_thread_to_ladybird_and_only_to_it() {
    let home = Home::new();
    home.browser(FAKE_BROWSER);
    let thread = home.thread();
    let o = home.run(&["web", "pane", &thread, "--view", "composer"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let argv: Vec<String> = home.read("browser.argv").lines().map(String::from).collect();
    assert_eq!(argv[..3], ["--headless=terminal", "--temporary-profile", "--device-pixel-ratio"], "drawn in this pane, with a profile of its own: {argv:?}");
    assert_eq!(argv[3], "1", "no pixel size to read outside a terminal: CSS pixels as they are");
    assert!(argv[4].starts_with("http://127.0.0.1:") && argv[4].contains("/?k="), "{argv:?}");
    let page = home.read("page.html");
    assert!(page.contains("<title>Axel</title>") && page.contains(r#"data-view="composer""#), "the composer's page loaded: {page}");
    let view: serde_json::Value = serde_json::from_str(&home.read("thread.json")).unwrap();
    assert_eq!(view["title"], "add retries");
    assert_eq!(view["id"], thread.as_str());
    assert_eq!(home.read("no-cookie.status"), "403", "nothing without the pane's cookie");
}

#[test]
fn a_pane_without_ladybird_or_where_it_cant_draw_falls_back_to_text() {
    let home = Home::new();
    let thread = home.thread();
    // The composer's fallback, the terminal composer, needs a terminal and
    // says so at once; the conversation's would follow the thread for good.
    let missing = home.run(&["web", "pane", &thread, "--view", "composer"], None);
    let said = String::from_utf8_lossy(&missing.stderr);
    assert!(said.contains("Ladybird isn't installed; showing it as terminal text"), "{said}");

    home.browser(REFUSING_BROWSER);
    let refused = home.run(&["web", "pane", &thread, "--view", "composer"], None);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("Ladybird couldn't draw here") && said.contains("showing it as terminal text"), "{said}");
    assert!(!home.run(&["web", "pane", &thread, "--view", "sideways"], None).status.success(), "only the three views");
}

#[test]
fn web_panes_are_off_until_turned_on_and_need_ladybird() {
    let home = Home::new();
    let shown = home.ok(&["panes"]);
    assert!(shown.contains("panes: terminal") && shown.contains("Ladybird: not installed"), "off by default: {shown}");
    let set = home.ok(&["panes", "web"]);
    assert!(set.contains("Ladybird isn't installed"), "it says web needs Ladybird: {set}");
    assert!(home.ok(&["panes"]).contains("panes: web"));
    home.browser(FAKE_BROWSER);
    assert!(home.ok(&["panes"]).contains(&format!("Ladybird: {}", home.path().join("bin").join("ladybird").display())));
    let o = home.run(&["panes"], Some("terminal"));
    assert!(String::from_utf8_lossy(&o.stdout).contains("panes: terminal"), "AXEL_PANES wins");
    assert!(!home.run(&["panes", "auto"], None).status.success(), "only web or terminal");
    let saved: serde_json::Value = serde_json::from_str(&home.read(".axel/settings.json")).unwrap();
    assert_eq!(saved["panes"], "web");
}

#[test]
fn a_web_pane_for_a_thread_that_doesnt_exist_says_so() {
    let home = Home::new();
    home.browser(FAKE_BROWSER);
    let o = home.run(&["web", "pane", "nope"], None);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("no thread nope"));
    assert_eq!(home.read("browser.argv"), "", "the browser never started");
}

/// A stand-in Ladybird on a terminal: records its arguments and the size
/// its terminal says, draws a "frame" at the top and a status line on its
/// last row, the way terminal mode does, then waits to be ended.
const STATUS_LINE_BROWSER: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/browser.argv"
size=$(stty size)
rows=${size%% *}
printf '\033[1;1Hframe %s\n' "$size"
printf '\033[%s;1H\033[2K\033[2m status http://127.0.0.1:1234/ ctrl-l url\033[0m' "$rows"
printf '\033[3;1Hafter\n'
sleep 30
"#;

/// The rows, columns and pixels of the pane a test runs a web pane in: 20
/// rows of 34-pixel cells, a Retina pane.
const PANE: [u16; 4] = [20, 80, 1280, 680];

/// Run `axel` on a pseudo-terminal of `PANE`'s size and return what it drew
/// within `secs`, then hang the pane up.
fn on_a_pane(home: &Home, args: &[&str], secs: u64) -> String {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize { ws_row: PANE[0], ws_col: PANE[1], ws_xpixel: PANE[2], ws_ypixel: PANE[3] };
    // Darwin declares the input size mutable; Linux declares it const.
    // SAFETY: openpty writes the descriptors and reads this live size value.
    assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut size) }, 0);
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: a flag on a descriptor owned here: the pane must not hold our end.
    assert_eq!(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
    let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
    c.env_clear()
        .env("HOME", home.path())
        .env("AXEL_HOME", home.path().join(".axel"))
        .env("AXEL_KEYSTORE", "file")
        .env("PATH", format!("{}:/usr/bin:/bin", home.path().join("bin").display()))
        .env("AXEL_LADYBIRD", home.path().join("bin").join("ladybird"))
        .args(args)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: setsid and TIOCSCTTY are async-signal-safe; they make the
    // pseudo-terminal this child's controlling terminal, as a pane's is.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = c.spawn().unwrap();
    // The command holds the pane's end too: dropped, closing ours hangs it up.
    drop(c);
    drop(slave);
    let fd = master.as_raw_fd();
    let mut reader = std::fs::File::from(master);
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, owned here, for the duration of the call.
        if unsafe { libc::poll(&mut poll, 1, 100) } > 0 {
            let mut chunk = [0_u8; 4096];
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
    }
    // Closing the pane hangs it up, as Rex does.
    drop(reader);
    let _ = child.wait();
    String::from_utf8_lossy(&seen).into_owned()
}

#[test]
fn ladybirds_status_line_never_reaches_the_pane_and_the_page_fills_it() {
    let started = std::time::Instant::now();
    let home = Home::new();
    home.browser(STATUS_LINE_BROWSER);
    let thread = home.thread();
    let drawn = on_a_pane(&home, &["web", "pane", &thread, "--view", "messages"], 3);
    assert!(drawn.contains("frame 21 80"), "Ladybird is told one row more than the pane: {drawn:?}");
    assert!(!drawn.contains("status") && !drawn.contains("127.0.0.1"), "its status line is dropped: {drawn:?}");
    assert!(drawn.contains("\x1b[3;1Hafter"), "what follows it is kept: {drawn:?}");
    let argv = home.read("browser.argv");
    assert!(argv.contains("--device-pixel-ratio\n2\n"), "a Retina pane draws at 2: {argv}");
    // The pane hung up well before the stand-in's 30 seconds: none is left.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let left = Command::new("pgrep").args(["-f", &home.path().join("bin").join("ladybird").display().to_string()]).output().unwrap();
    assert!(left.stdout.is_empty(), "Ladybird ends with its pane: {}", String::from_utf8_lossy(&left.stdout));
    assert!(started.elapsed() < std::time::Duration::from_secs(15), "ended by the hangup, not by its own end");
}

/// One HTTP request to a served page, with its cookie; the reply's body.
fn http(port: &str, method: &str, path: &str, cookie: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nCookie: axel_web={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    let reply = String::from_utf8_lossy(&reply).into_owned();
    reply.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or(reply)
}

/// The cookie a served page's address earns, the way Ladybird earns it.
fn cookie_for(port: &str, token: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    stream.write_all(format!("GET /?k={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let set = reply.lines().find_map(|l| l.strip_prefix("Set-Cookie: axel_web=")).unwrap_or_else(|| panic!("no cookie: {reply}"));
    set.split(';').next().unwrap().to_string()
}

#[test]
fn the_web_thread_controls_ask_first_then_settle_the_thread() {
    let home = Home::new();
    let thread = home.thread();
    let mut serve = Command::new(env!("CARGO_BIN_EXE_axel"))
        .env_clear()
        .env("HOME", home.path())
        .env("AXEL_HOME", home.path().join(".axel"))
        .env("AXEL_KEYSTORE", "file")
        .env("PATH", "/usr/bin:/bin")
        .args(["web", "serve", &thread, "--view", "controls"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(serve.stdout.take().unwrap()), &mut line).unwrap();
    let (base, token) = line.trim().split_once("/?k=").unwrap();
    let port = base.rsplit(':').next().unwrap().to_string();
    let cookie = cookie_for(&port, token);
    let secret = cookie.as_str();

    let page: serde_json::Value = serde_json::from_str(&http(&port, "GET", "/api/thread", secret, "")).unwrap();
    assert_eq!(page["view"], "controls");
    assert_eq!(page["title"], "add retries");
    assert!(page["actions"].as_array().unwrap().iter().any(|a| a["action"] == "settle"), "{page}");

    let asked: serde_json::Value = serde_json::from_str(&http(&port, "POST", "/api/control", secret, r#"{"outcome":{"op":"settle"}}"#)).unwrap();
    assert_eq!(asked["confirm"], "settle this thread?", "asked first, as the terminal pane asks");
    let refused = http(&port, "POST", "/api/control", secret, r#"{"outcome":{"op":"reset","agent":"mallory"},"confirmed":true}"#);
    assert!(refused.contains("isn't something the controls offer"), "{refused}");
    let done: serde_json::Value = serde_json::from_str(&http(&port, "POST", "/api/control", secret, r#"{"outcome":{"op":"settle"},"confirmed":true}"#)).unwrap();
    assert_eq!(done["note"], "settled");
    let _ = serve.kill();
    let _ = serve.wait();
    let store = axel_store::Store::open(&home.path().join(".axel").join("axel.db")).unwrap();
    assert_eq!(store.get_thread(&thread).unwrap().unwrap().state, axel_store::ThreadState::Done);
}
