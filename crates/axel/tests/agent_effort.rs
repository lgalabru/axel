//! `axel agent new|edit --effort`, `axel agent show` and `axel agent ls`, through the binary.
use axel_identity::Keypair;
use axel_store::{Effort, NewAgent, Store};
use std::process::{Command, Output};

struct Fixture {
    dir: tempfile::TempDir,
    agent: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("axel.db")).unwrap();
        let agent = Keypair::generate().public().to_string();
        store.insert_agent(&NewAgent { name: "alice".into(), pubkey: agent.clone(), ..Default::default() }).unwrap();
        Self { dir, agent }
    }
    fn command(&self, args: &[&str], caller: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
        // Harnesses are looked up in `bin` only: see `pane_argv`.
        let path = format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display());
        c.env_clear().env("HOME", self.dir.path()).env("AXEL_HOME", self.dir.path()).env("AXEL_KEYSTORE", "file").env("PATH", path).args(args);
        if let Some(caller) = caller {
            c.env("AXEL_AGENT", caller);
        }
        c.output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.command(args, None);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }
    /// The effort on the agent's record.
    fn effort(&self, agent: &str) -> Option<Effort> {
        Store::open(&self.dir.path().join("axel.db")).unwrap().agent_by_name(agent).unwrap().unwrap().effort
    }
    /// Start a pane session of `agent` outside Rex, where the harness takes
    /// over the process: the arguments a stand-in harness was started with,
    /// and what Axel said on stderr before that.
    #[cfg(unix)]
    fn pane_argv(&self, agent: &str, harness: &str) -> (Vec<String>, String) {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let argv = self.dir.path().join(format!("{agent}.argv"));
        let script = bin.join(harness);
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", argv.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let o = self.command(&["thread", "new", "fix it", "--agent", agent, "--scratch", "--runtime", "pane"], None);
        let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(o.status.success(), "{stderr}");
        (std::fs::read_to_string(&argv).unwrap_or_else(|e| panic!("{harness} was not started ({e}): {stderr}")).lines().map(String::from).collect(), stderr)
    }
    /// Start a managed session of `agent` outside Rex, where this process
    /// becomes the session host: the requests it sent to a stand-in ACP agent
    /// (the agent's harness), which advertises an effort selector and leaves
    /// at the first prompt, ending the session.
    #[cfg(unix)]
    fn managed_requests(&self, agent: &str) -> Vec<serde_json::Value> {
        let received = self.dir.path().join(format!("{agent}.received"));
        let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
        c.env_clear().env("HOME", self.dir.path()).env("AXEL_HOME", self.dir.path()).env("AXEL_KEYSTORE", "file").env("PATH", "/usr/bin:/bin");
        c.env("ACP_RECEIVED", &received).args(["thread", "new", "fix it", "--agent", agent, "--scratch", "--runtime", "managed"]);
        let mut host = c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let started = std::time::Instant::now();
        while host.try_wait().unwrap().is_none() {
            if started.elapsed() > std::time::Duration::from_secs(30) {
                host.kill().ok();
                panic!("the session host did not end");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let lines = std::fs::read_to_string(&received).unwrap_or_else(|e| panic!("the ACP agent was not started: {e}"));
        lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
    /// A stand-in ACP agent, as a harness command.
    #[cfg(unix)]
    fn acp_agent(&self) -> String {
        use std::os::unix::fs::PermissionsExt;
        let script = self.dir.path().join("acp-agent");
        std::fs::write(&script, ACP_AGENT).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.to_string_lossy().into_owned()
    }
    /// The agent's line in `agent ls`, as its columns.
    fn listed(&self, agent: &str) -> Vec<String> {
        let ls = self.ok(&["agent", "ls"]);
        let line = ls.lines().find(|l| l.split_whitespace().next() == Some(agent)).unwrap_or_else(|| panic!("{agent} is not listed: {ls}"));
        line.split_whitespace().map(String::from).collect()
    }
}

#[test]
fn the_owner_sets_shows_and_clears_an_effort_on_the_agents_record() {
    let f = Fixture::new();
    // An agent with no effort shows none.
    let shown = f.ok(&["agent", "show", "alice"]);
    assert!(shown.contains(r#""effort": null"#) && !shown.contains("  effort  "), "{shown}");
    assert_eq!(f.listed("alice")[1..4], ["claude", "-", "-"], "harness, model, effort");

    let edited = f.ok(&["agent", "edit", "alice", "--effort", "high"]);
    assert!(edited.contains("  effort  high"), "{edited}");
    assert_eq!(f.effort("alice"), Some(Effort::High));
    let shown = f.ok(&["agent", "show", "alice"]);
    assert!(shown.contains(r#""effort": "high""#) && shown.contains("  effort  high"), "{shown}");
    assert_eq!(f.listed("alice")[1..4], ["claude", "-", "high"]);

    let created = f.ok(&["agent", "new", "bob", "codex/gpt-5.5", "--effort", "xhigh"]);
    assert!(created.contains("  effort  xhigh"), "{created}");
    assert_eq!(f.effort("bob"), Some(Effort::Xhigh));
    assert_eq!(f.listed("bob")[1..4], ["codex", "gpt-5.5", "xhigh"]);
    let shown = f.ok(&["agent", "show", "bob"]);
    assert!(shown.contains("owner attestation: valid"), "{shown}");

    f.ok(&["agent", "edit", "alice", "--effort", "default"]);
    assert_eq!(f.effort("alice"), None);
    assert!(!f.ok(&["agent", "show", "alice"]).contains("  effort  "));
    // Other edits leave it alone, and so does a rename.
    f.ok(&["agent", "edit", "bob", "--model", "gpt-5.5-codex"]);
    f.ok(&["agent", "edit", "bob", "--name", "bobby"]);
    assert_eq!(f.effort("bobby"), Some(Effort::Xhigh));
    assert!(f.ok(&["agent", "show", "bobby"]).contains("owner attestation: valid"));
    // An agent made without the flag has none.
    f.ok(&["agent", "new", "carol"]);
    assert_eq!(f.effort("carol"), None);
}

#[test]
fn claude_account_selection_survives_renaming_and_can_be_cleared() {
    let f = Fixture::new();
    let profile = f.dir.path().join("Claude profile");
    std::fs::create_dir(&profile).unwrap();
    let profile = profile.to_str().unwrap();
    f.ok(&["agent", "new", "orbit-1-opus-medium", "claude/opus", "--claude-config-dir", profile]);
    f.ok(&["agent", "edit", "orbit-1-opus-medium", "--name", "orbit-1-opus-high"]);
    let shown = f.ok(&["agent", "show", "orbit-1-opus-high"]);
    assert!(shown.contains(&format!("Claude config directory: {profile}")));
    assert!(shown.contains("owner attestation: valid"));
    f.ok(&["agent", "edit", "orbit-1-opus-high", "--claude-config-dir", "default"]);
    assert!(f.ok(&["agent", "show", "orbit-1-opus-high"]).contains("Claude config directory: default"));
    f.ok(&["agent", "edit", "orbit-1-opus-high", "codex", "--inherit-claude-config"]);
    assert!(!f.ok(&["agent", "show", "orbit-1-opus-high"]).contains("Claude config directory:"));
    let result = f.command(&["agent", "new", "bad", "codex", "--claude-config-dir", profile], None);
    assert!(!result.status.success());
    assert!(!f.ok(&["agent", "ls"]).contains("bad"));
}

#[test]
fn an_unknown_level_is_refused_when_the_command_is_parsed() {
    let f = Fixture::new();
    for args in [vec!["agent", "edit", "alice", "--effort", "ultra"], vec!["agent", "new", "zed", "--effort", "HIGH!"]] {
        let o = f.command(&args, None);
        assert_eq!(o.status.code(), Some(2), "a usage error");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("possible values: low, medium, high, xhigh, max, default"), "{err}");
    }
    assert_eq!(f.effort("alice"), None);
    assert!(!f.ok(&["agent", "ls"]).contains("zed"), "nothing was created");
}

#[test]
fn agents_cannot_set_an_effort() {
    let f = Fixture::new();
    for args in [vec!["agent", "edit", "alice", "--effort", "max"], vec!["agent", "new", "zed", "--effort", "max"]] {
        let o = f.command(&args, Some(&f.agent));
        assert!(!o.status.success());
        assert!(String::from_utf8_lossy(&o.stderr).contains("owner"));
    }
    assert_eq!(f.effort("alice"), None);
    assert!(!f.ok(&["agent", "ls"]).contains("zed"));
}

#[test]
fn new_edit_and_show_warn_where_the_effort_cannot_be_applied() {
    let f = Fixture::new();
    // Claude takes every level: no warning.
    f.ok(&["agent", "edit", "alice", "--effort", "max"]);
    assert!(!f.ok(&["agent", "show", "alice"]).contains("warning"));
    // Codex stops at xhigh.
    let cody = f.ok(&["agent", "new", "cody", "codex", "--effort", "max"]);
    assert!(cody.contains("warning: Codex has no `max` reasoning effort"), "{cody}");
    assert!(f.ok(&["agent", "show", "cody"]).contains("warning: Codex has no `max` reasoning effort"));
    let edited = f.ok(&["agent", "edit", "cody", "--effort", "xhigh"]);
    assert!(!edited.contains("warning"), "{edited}");
    assert!(!f.ok(&["agent", "show", "cody"]).contains("warning"));
    // A harness whose own CLI has no effort flag: only a managed session can apply it.
    f.ok(&["agent", "new", "gus", "goose", "--effort", "high"]);
    let gus = f.ok(&["agent", "show", "gus"]);
    assert!(gus.contains("warning: Goose panes cannot be started at effort high"), "{gus}");
    // Switching harness is told the same.
    let moved = f.ok(&["agent", "edit", "alice", "codex"]);
    assert!(moved.contains("  effort  max") && moved.contains("warning: Codex has no `max` reasoning effort"), "{moved}");
}

/// Answers `initialize`, `session/new` (with a `thought_level` selector) and
/// `session/set_config_option`, records every request, and exits at the first prompt.
#[cfg(unix)]
const ACP_AGENT: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$ACP_RECEIVED"
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\("[^"]*"\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1","configOptions":[{"id":"depth","name":"Depth","category":"thought_level","type":"select","currentValue":"medium","options":[{"value":"medium","name":"Medium"},{"value":"high","name":"High"}]}]}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id" ;;
    *'"method":"session/prompt"'*) exit 0 ;;
  esac
done
"#;

#[cfg(unix)]
#[test]
fn a_managed_session_is_hosted_at_the_effort_on_the_agents_record() {
    let f = Fixture::new();
    let harness = f.acp_agent();
    let options_set = |requests: &[serde_json::Value]| -> Vec<serde_json::Value> {
        requests.iter().filter(|r| r["method"] == "session/set_config_option").map(|r| serde_json::json!([r["params"]["configId"], r["params"]["value"]])).collect()
    };

    f.ok(&["agent", "new", "gus", &harness, "--effort", "high"]);
    let requests = f.managed_requests("gus");
    assert!(requests.iter().any(|r| r["method"] == "session/prompt"), "the session ran: {requests:?}");
    assert_eq!(options_set(&requests), [serde_json::json!(["depth", "high"])]);

    // No effort: the harness is asked for nothing.
    f.ok(&["agent", "new", "hal", &harness]);
    let requests = f.managed_requests("hal");
    assert!(requests.iter().any(|r| r["method"] == "session/prompt"), "the session ran: {requests:?}");
    assert!(options_set(&requests).is_empty(), "{requests:?}");
}

#[cfg(unix)]
#[test]
fn a_pane_session_starts_the_harness_at_the_effort_on_the_agents_record() {
    let f = Fixture::new();
    f.ok(&["agent", "new", "opal", "claude", "--effort", "high"]);
    let (argv, stderr) = f.pane_argv("opal", "claude");
    let at = argv.iter().position(|a| a == "--effort").unwrap_or_else(|| panic!("no --effort: {argv:?}"));
    assert_eq!(argv[at + 1], "high");
    assert!(!stderr.contains("effort"), "{stderr}");

    // Codex takes it as a config override, ahead of everything else.
    f.ok(&["agent", "new", "cody", "codex", "--effort", "xhigh"]);
    let (argv, _) = f.pane_argv("cody", "codex");
    assert_eq!(argv[..2], ["-c", "model_reasoning_effort=xhigh"], "{argv:?}");

    // A level the harness does not have: said at launch, and the pane starts without it.
    f.ok(&["agent", "edit", "cody", "--effort", "max"]);
    let (argv, stderr) = f.pane_argv("cody", "codex");
    assert!(!argv.iter().any(|a| a.contains("reasoning_effort")), "{argv:?}");
    assert!(stderr.contains("axel: Codex has no `max` reasoning effort"), "{stderr}");

    // No effort: the harness starts as it always did, and nothing is said.
    f.ok(&["agent", "edit", "opal", "--effort", "default"]);
    let (argv, stderr) = f.pane_argv("opal", "claude");
    assert!(!argv.contains(&"--effort".to_string()), "{argv:?}");
    assert!(!stderr.contains("effort"), "{stderr}");
}
