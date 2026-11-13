//! A Claude or Codex agent behind `AXEL_ACP_CLAUDE` / `AXEL_ACP_CODEX` runs under some other
//! ACP adapter, which does not read `_meta.axel.effort`: the host must treat the session as
//! generic ACP. This file holds one test, in its own process, because it sets those variables.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axel_acp::socket::{self, Command, Reply};
use axel_acp::{HostConfig, run_host};
use axel_store::{Effort, Harness, ItemKind, Keypair, NewSession, RuntimeMode, SessionRuntime, SessionState, Store};
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Pane(Arc<Mutex<Vec<u8>>>);

impl Write for Pane {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a hosted session of the fake agent left behind.
struct Hosted {
    /// The frames the host sent, from its frame log.
    sent: Vec<Value>,
    /// The warnings it put on the thread.
    warnings: Vec<String>,
    pane: String,
}

impl Hosted {
    fn params(&self, method: &str) -> Vec<Value> {
        self.sent.iter().filter(|f| f["method"] == method).map(|f| f["params"].clone()).collect()
    }
}

/// Host one session of `harness` at `effort` until it is idle, then stop it.
fn host(harness: Harness, effort: Effort, agent_env: &[(&str, &str)]) -> Hosted {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("axel.db");
    let store = Store::open(&db).unwrap();
    let owner = Keypair::generate();
    let agent = Keypair::generate();
    let thread = store.create_thread(&owner, "Ship it", "test", None).unwrap();
    let session = store
        .start_session(
            &owner,
            &NewSession {
                thread_id: thread.id.clone(),
                agent_pubkey: agent.public().to_base58(),
                environment_id: "env-local".into(),
                environment_label: "local".into(),
                project_repo_key: None,
                harness: harness.clone(),
                runtime_mode: RuntimeMode::Autopilot,
                resumed_from: None,
                runtime: SessionRuntime::Managed,
            },
        )
        .unwrap();
    let cwd = dir.path().join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = HostConfig {
        session_id: session.id.clone(),
        thread_id: thread.id.clone(),
        agent_pubkey: agent.public().to_base58(),
        cwd,
        harness,
        runtime_mode: RuntimeMode::Autopilot,
        disallowed_tools: vec![],
        log_path: dir.path().join("logs").join(format!("{}.jsonl", session.id)),
        socket_path: dir.path().join("run").join(format!("{}.sock", session.id)),
        env: agent_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        initial_prompt: None,
        initial_attachments: vec![],
        resume_native_id: None,
        initial_from: None,
        reply_with: None,
        effort: Some(effort),
        mcp_servers: Vec::new(),
        session_meta: None,
        model: None,
    };
    let pane = Pane::default();
    let running = {
        let (cfg, pane) = (cfg.clone(), pane.clone());
        let store = Store::open(&db).unwrap();
        let signer = Keypair::from_secret_bytes(&owner.secret_bytes());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(run_host(cfg, store, signer, pane))
        })
    };
    let start = Instant::now();
    while store.get_session(&session.id).unwrap().unwrap().state != SessionState::Idle {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for the session to idle");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(socket::send(&cfg.socket_path, &Command::Stop).unwrap(), Reply::Ok);
    running.join().unwrap().unwrap();

    let sent = std::fs::read_to_string(&cfg.log_path)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["dir"] == "out")
        .map(|v| v["frame"].clone())
        .collect();
    let warnings = store
        .items(&session.id, 0, 1000)
        .unwrap()
        .iter()
        .filter(|i| i.kind == ItemKind::Error)
        .map(|i| i.content["message"].as_str().unwrap_or_default().to_string())
        .collect();
    let pane = String::from_utf8_lossy(&pane.0.lock().unwrap()).into_owned();
    Hosted { sent, warnings, pane }
}

#[test]
fn an_overridden_claude_or_codex_adapter_gets_the_effort_as_generic_acp() {
    let fake = format!("'{}'", env!("CARGO_BIN_EXE_fake_acp_agent"));
    // SAFETY: the only test in this binary, and no other thread exists yet.
    unsafe {
        std::env::set_var("AXEL_ACP_CODEX", &fake);
        std::env::set_var("AXEL_ACP_CLAUDE", &fake);
    }

    // The other adapter advertises an effort selector: it is set there, not in `_meta`.
    let codex = host(Harness::Codex, Effort::High, &[("FAKE_AGENT_EFFORT", "category")]);
    let new = codex.params("session/new");
    assert_eq!(new.len(), 1);
    assert!(new[0].pointer("/_meta/axel/effort").is_none(), "only Axel's own adapters read it: {}", new[0]);
    assert_eq!(codex.params("session/set_config_option"), [json!({ "sessionId": "fake-session-1", "configId": "thinking-budget", "value": "high" })]);
    assert!(codex.warnings.is_empty(), "{:?}", codex.warnings);
    assert!(codex.pane.contains("effort high"), "{}", codex.pane);

    // It advertises none: the owner is told, and the session went on to idle.
    let claude = host(Harness::Claude, Effort::High, &[]);
    assert!(claude.params("session/new")[0].pointer("/_meta/axel/effort").is_none());
    assert!(claude.params("session/set_config_option").is_empty());
    assert_eq!(claude.warnings.len(), 1, "{:?}", claude.warnings);
    assert!(claude.warnings[0].contains("could not set effort high") && claude.warnings[0].contains("offers no effort option"), "{:?}", claude.warnings);
    assert!(claude.pane.contains("offers no effort option"), "{}", claude.pane);
}
