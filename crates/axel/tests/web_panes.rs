//! Web panes (RFD 0007) through the binary: `axel web pane` serves the
//! thread and hands its address to `terminal-browser`; without one, or when
//! it can't draw, the pane falls back to the terminal view; `axel panes`
//! chooses.  A stand-in `terminal-browser` loads the page the way the real
//! one would, with `curl`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A stand-in terminal-browser: records its arguments, then loads the page
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

/// A terminal-browser that can't draw here: it gives up at once.
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
        let path = self.path().join("bin").join("terminal-browser");
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
fn a_web_pane_serves_its_thread_to_terminal_browser_and_only_to_it() {
    let home = Home::new();
    home.browser(FAKE_BROWSER);
    let thread = home.thread();
    let o = home.run(&["web", "pane", &thread], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let argv: Vec<String> = home.read("browser.argv").lines().map(String::from).collect();
    assert_eq!(argv[..2], ["open", "--no-merge"], "its own browser, not a tab in another: {argv:?}");
    assert!(argv[2].starts_with("http://127.0.0.1:") && argv[2].contains("/?k="), "{argv:?}");
    assert!(home.read("page.html").contains("<title>Axel</title>"), "the page loaded");
    let view: serde_json::Value = serde_json::from_str(&home.read("thread.json")).unwrap();
    assert_eq!(view["title"], "add retries");
    assert_eq!(view["id"], thread.as_str());
    assert_eq!(home.read("no-cookie.status"), "403", "nothing without the pane's cookie");
}

#[test]
fn a_pane_without_terminal_browser_or_where_it_cant_draw_falls_back_to_text() {
    let home = Home::new();
    let thread = home.thread();
    let missing = home.run(&["web", "pane", &thread], None);
    let said = String::from_utf8_lossy(&missing.stderr);
    assert!(said.contains("terminal-browser isn't installed; showing the thread as terminal text"), "{said}");

    home.browser(REFUSING_BROWSER);
    let refused = home.run(&["web", "pane", &thread], None);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("terminal-browser couldn't draw here") && said.contains("showing the thread as terminal text"), "{said}");
}

#[test]
fn the_pane_choice_is_saved_and_the_environment_overrides_it() {
    let home = Home::new();
    let shown = home.ok(&["panes"]);
    assert!(shown.contains("panes: auto") && shown.contains("terminal-browser: not installed"), "{shown}");
    let set = home.ok(&["panes", "web"]);
    assert!(set.contains("isn't installed"), "it says web needs terminal-browser: {set}");
    assert!(home.ok(&["panes"]).contains("panes: web"));
    let o = home.run(&["panes"], Some("terminal"));
    assert!(String::from_utf8_lossy(&o.stdout).contains("panes: terminal"), "AXEL_PANES wins");
    assert!(!home.run(&["panes", "sideways"], None).status.success());
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
