//! `axel setup`, through the binary: the defaults a first run gets with no
//! questions (`--yes`), and what it says when it can't help.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use axel_store::{AgentColor, Harness, RuntimeMode, Store};

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    /// A home whose PATH holds stand-ins for `harnesses`, with Claude Code logged in.
    fn with(harnesses: &[&str]) -> Home {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for program in harnesses {
            std::fs::write(bin.join(program), "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(bin.join(program), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.path().join(".claude.json"), r#"{"oauthAccount": {"emailAddress": "you@example.com"}}"#).unwrap();
        Home { dir }
    }

    fn setup(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display());
        Command::new(env!("CARGO_BIN_EXE_axel"))
            .env_clear()
            .env("HOME", self.dir.path())
            .env("AXEL_HOME", self.dir.path().join(".axel"))
            .env("AXEL_KEYSTORE", "file")
            .env("PATH", path)
            .arg("setup")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn store(&self) -> Store {
        Store::open(&self.dir.path().join(".axel").join("axel.db")).unwrap()
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }
}

fn said(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn yes_makes_a_ready_agent_with_the_defaults_and_asks_nothing() {
    let home = Home::with(&["claude", "codex"]);
    let first = home.setup(&["--yes"]);
    assert!(first.status.success(), "{}", said(&first));
    let text = said(&first);
    assert!(text.contains("Claude Code") && text.contains("ready"), "{text}");
    assert!(text.contains("also here: Codex"), "{text}");
    assert!(text.contains("atlas is ready") && text.contains("@atlas add a health check endpoint"), "{text}");
    assert!(!text.contains("/usr/") && !text.contains("attested"), "no paths or jargon by default: {text}");

    let atlas = home.store().agent_by_name("atlas").unwrap().expect("atlas was made");
    assert_eq!((atlas.harness, atlas.runtime_mode, atlas.color), (Harness::Claude, RuntimeMode::Autopilot, Some(AgentColor::Orange)));
    assert_eq!(atlas.skills, vec!["*".to_string()]);
    assert!(!home.path(".claude/settings.json").exists(), "--yes never changes Claude Code's own settings");

    let second = home.setup(&["--yes"]);
    assert!(said(&second).contains("You have atlas"), "{}", said(&second));
    let nova = home.store().agent_by_name("nova").unwrap().expect("the next free name");
    assert_ne!(nova.color, atlas.color, "each agent gets its own colour");
}

#[test]
fn a_harness_that_isnt_logged_in_is_flagged_before_anything_else() {
    let home = Home::with(&["claude"]);
    std::fs::write(home.path(".claude.json"), "{}").unwrap();
    let o = home.setup(&["--yes"]);
    assert!(said(&o).contains("log in once first: run `claude`, then `/login`"), "{}", said(&o));
}

#[test]
fn with_no_harness_it_says_how_to_get_one_and_makes_nothing() {
    let home = Home::with(&[]);
    let o = home.setup(&["--yes"]);
    assert!(o.status.success());
    assert!(said(&o).contains("none is installed"), "{}", said(&o));
    assert!(home.store().list_agents().unwrap().is_empty());
}

#[test]
fn without_a_terminal_it_points_at_yes() {
    let home = Home::with(&["claude"]);
    let o = home.setup(&[]);
    assert!(!o.status.success());
    assert!(said(&o).contains("axel setup --yes"), "{}", said(&o));
    let both = home.setup(&["--yes", "--custom"]);
    assert!(!both.status.success(), "--yes and --custom don't mix");
    assert!(!Path::new(&home.path(".axel/axel.db")).exists() || home.store().list_agents().unwrap().is_empty());
}
