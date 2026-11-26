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
