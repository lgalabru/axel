//! End-to-end: `run_host` against the fake ACP agent (and, opt-in, real `goose acp`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axel_acp::socket::{self, Command, Reply};
use axel_acp::{HostConfig, run_host};
use axel_store::{
    Harness, ItemKind, ItemStatus, Keypair, NewSession, RuntimeMode, SessionRuntime, SessionState,
    Store,
};

/// A `Write` that appends into a shared buffer (what the host pane would show).
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

impl Pane {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    store: Store,
    owner: Keypair,
    thread_id: String,
    session_id: String,
    cfg: HostConfig,
    pane: Pane,
}

fn fixture(mode: RuntimeMode, harness: Harness, initial_prompt: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("axel.db");
    let store = Store::open(&db).unwrap();
    let owner = Keypair::generate();
    let agent = Keypair::generate();
    let thread = store
        .create_thread(&owner, "Ship it", "test", None)
        .unwrap();
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
                runtime_mode: mode,
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
        runtime_mode: mode,
        disallowed_tools: vec![],
        log_path: dir
            .path()
            .join("logs")
            .join(format!("{}.jsonl", session.id)),
        socket_path: dir.path().join("run").join(format!("{}.sock", session.id)),
        env: vec![],
        initial_prompt: initial_prompt.map(str::to_string),
        initial_attachments: vec![],
        resume_native_id: None,
        initial_from: None,
        reply_with: None,
        mcp_servers: Vec::new(),
        session_meta: None,
        model: None,
    };
    Fixture {
        _dir: dir,
        db,
        store,
        owner,
        thread_id: thread.id,
        session_id: session.id,
        cfg,
        pane: Pane::default(),
    }
}

fn fake_harness() -> Harness {
    Harness::Custom(env!("CARGO_BIN_EXE_fake_acp_agent").to_string())
}

/// Run the host on its own thread (current-thread runtime) and return its join handle.
fn spawn_host(f: &Fixture) -> std::thread::JoinHandle<anyhow::Result<()>> {
    let cfg = f.cfg.clone();
    let store = Store::open(&f.db).unwrap();
    let signer = Keypair::from_secret_bytes(&f.owner.secret_bytes());
    let pane = f.pane.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_host(cfg, store, signer, pane))
    })
}

fn wait_until(what: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let start = Instant::now();
    while !pred() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn send_ok(path: &Path, cmd: Command) {
    let reply = socket::send(path, &cmd).unwrap();
    assert_eq!(reply, Reply::Ok, "{cmd:?}");
}

fn kinds(f: &Fixture) -> Vec<(ItemKind, Option<ItemStatus>)> {
    f.store
        .items(&f.session_id, 0, 1000)
        .unwrap()
        .into_iter()
        .map(|i| (i.kind, i.status))
        .collect()
}

#[test]
fn supervised_session_asks_streams_and_records_turns() {
    let f = fixture(RuntimeMode::Supervised, fake_harness(), Some("hello"));
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();

    // Turn 1 runs from the initial prompt and parks on a permission.
    wait_until("pending permission", Duration::from_secs(20), || {
        !f.store
            .pending_items(Some(&f.thread_id))
            .unwrap()
            .is_empty()
    });
    let pending = f.store.pending_items(Some(&f.thread_id)).unwrap();
    assert_eq!(pending.len(), 1);
    let perm = &pending[0];
    assert_eq!(perm.kind, ItemKind::Permission);
    assert_eq!(perm.content["title"], "Bash");
    assert_eq!(perm.content["kind"], "execute");
    assert_eq!(perm.content["raw_input"]["command"], "cargo test");
    assert_eq!(perm.content["options"].as_array().unwrap().len(), 3);
    let status = socket::send(&sock, &Command::Status).unwrap();
    assert_eq!(
        status,
        Reply::Status {
            state: SessionState::Blocked,
            turn: 1,
            pending: 1,
            queued: 0,
        }
    );
    let live = f.store.status(&f.session_id).unwrap().unwrap();
    assert_eq!(live.state, axel_store::StatusState::Blocked);
    assert_eq!(live.kind, Some(axel_store::BlockedKind::Permission));

    // A bad answer is refused; the good one unblocks the agent.
    let bad = socket::send(
        &sock,
        &Command::Answer {
            item_id: perm.id.clone(),
            option_id: "nope".into(),
        },
    )
    .unwrap();
    assert!(matches!(bad, Reply::Error { .. }));
    send_ok(
        &sock,
        Command::Answer {
            item_id: perm.id.clone(),
            option_id: "allow".into(),
        },
    );

    wait_until("turn 1 recorded", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    wait_until("session idle", Duration::from_secs(5), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });

    let items = f.store.items(&f.session_id, 0, 1000).unwrap();
    let got = kinds(&f);
    assert_eq!(
        got,
        vec![
            (ItemKind::UserMessage, Some(ItemStatus::Completed)),
            (ItemKind::AssistantMessage, Some(ItemStatus::Completed)),
            (ItemKind::ToolCall, Some(ItemStatus::Completed)),
            (ItemKind::Permission, Some(ItemStatus::Allowed)),
            (ItemKind::AssistantMessage, Some(ItemStatus::Completed)),
            (ItemKind::TurnEnd, Some(ItemStatus::Completed)),
        ],
        "{items:#?}"
    );
    assert_eq!(items[0].content["text"], "hello");
    assert_eq!(items[1].content["text"], "Hello, world.");
    let tool = &items[2];
    assert_eq!(tool.ref_id.as_deref(), Some("call-1"));
    assert_eq!(tool.content["title"], "Bash");
    assert_eq!(tool.content["raw_output"], "test result: ok");
    assert_eq!(tool.content["locations"][0]["path"], "Cargo.toml");
    assert_eq!(items[3].content["answer"], "allow");
    assert_eq!(items[4].content["text"], "Tests pass.");
    assert_eq!(items[5].content["stop_reason"], "end_turn");
    assert_eq!(items[5].content["tool_calls"], 1);
    assert!(items.iter().all(|i| i.turn == 1));

    let turns = f.store.turns(&f.thread_id).unwrap();
    assert_eq!(turns[0].summary.ordinal, 1);
    assert_eq!(turns[0].summary.user_text, "hello");
    assert_eq!(turns[0].summary.assistant_text, "Hello, world.Tests pass.");
    assert_eq!(turns[0].summary.tool_calls, 1);
    assert_eq!(turns[0].summary.stop_reason, "end_turn");
    assert!(turns[0].summary.duration_ms >= 0);

    let session = f.store.get_session(&f.session_id).unwrap().unwrap();
    assert_eq!(
        session.harness_session_id.as_deref(),
        Some("fake-session-1")
    );
    assert_eq!(session.socket_path.as_deref(), Some(sock.as_path()));

    // Prompting while idle starts turn 2; it includes a thought.
    send_ok(
        &sock,
        Command::Prompt {
            text: "a thought".into(),
            attachments: vec![],
            steer: false,
            from: None,
        },
    );
    wait_until("turn 2 permission", Duration::from_secs(20), || {
        !f.store
            .pending_items(Some(&f.thread_id))
            .unwrap()
            .is_empty()
    });
    let perm2 = f.store.pending_items(Some(&f.thread_id)).unwrap().remove(0);
    send_ok(
        &sock,
        Command::Answer {
            item_id: perm2.id.clone(),
            option_id: "reject".into(),
        },
    );
    wait_until("turn 2 recorded", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 2
    });
    let items2: Vec<_> = f
        .store
        .items(&f.session_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|i| i.turn == 2)
        .collect();
    assert_eq!(items2[1].kind, ItemKind::Thought);
    assert_eq!(items2[1].content["text"], "Let me think.");
    let perm2 = f
        .store
        .items(&f.session_id, 0, 1000)
        .unwrap()
        .into_iter()
        .find(|i| i.id == perm2.id)
        .unwrap();
    assert_eq!(perm2.status, Some(ItemStatus::Denied));
    let tool2 = items2
        .iter()
        .find(|i| i.kind == ItemKind::ToolCall)
        .unwrap();
    assert_eq!(tool2.status, Some(ItemStatus::Failed));

    // Stop: the session ends, the socket goes away, the host returns.
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
    let session = f.store.get_session(&f.session_id).unwrap().unwrap();
    assert_eq!(session.state, SessionState::Ended);
    assert_eq!(session.ended_reason.as_deref(), Some("stopped"));
    assert!(session.socket_path.is_none());
    assert!(!sock.exists());

    // Frames were logged both ways.
    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    let frames: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        frames
            .iter()
            .any(|fr| fr["dir"] == "out" && fr["frame"]["method"] == "initialize")
    );
    assert!(
        frames
            .iter()
            .any(|fr| fr["dir"] == "out" && fr["frame"]["method"] == "session/prompt")
    );
    assert!(
        frames
            .iter()
            .any(|fr| fr["dir"] == "in" && fr["frame"]["method"] == "session/update")
    );
    assert!(
        frames
            .iter()
            .any(|fr| fr["dir"] == "in" && fr["frame"]["method"] == "session/request_permission")
    );
    assert!(frames.iter().all(|fr| fr["ts"].as_i64().unwrap() > 0));
    let init = frames
        .iter()
        .find(|fr| fr["frame"]["method"] == "initialize")
        .unwrap();
    assert_eq!(init["frame"]["params"]["protocolVersion"], 1);
    assert_eq!(
        init["frame"]["params"]["clientCapabilities"]["fs"]["readTextFile"],
        true
    );
    assert_eq!(
        init["frame"]["params"]["clientCapabilities"]["fs"]["writeTextFile"],
        true
    );

    // The pane got one line per event.
    let pane = f.pane.text();
    assert!(pane.contains("prompt 1"), "{pane}");
    assert!(pane.contains("permission: Bash"), "{pane}");
    assert!(pane.contains("turn 1 done · 1 tools"), "{pane}");
    assert!(pane.contains("thinking"), "{pane}");
    assert!(pane.contains("session ended: stopped"), "{pane}");
}

#[test]
fn autopilot_allows_and_plan_denies_by_policy() {
    // Autopilot: the permission is answered by policy, no Permission item appears.
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("go"));
    let host = spawn_host(&f);
    wait_until("turn recorded", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    let items = f.store.items(&f.session_id, 0, 100).unwrap();
    assert!(
        items.iter().all(|i| i.kind != ItemKind::Permission),
        "{items:#?}"
    );
    let tool = items.iter().find(|i| i.kind == ItemKind::ToolCall).unwrap();
    assert_eq!(tool.status, Some(ItemStatus::Completed));
    assert!(f.pane.text().contains("allow: Bash"), "{}", f.pane.text());
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();

    // Plan: execute is denied; the agent's tool call fails and a Denied Permission item records why.
    let f = fixture(RuntimeMode::Plan, fake_harness(), Some("go"));
    let host = spawn_host(&f);
    wait_until("turn recorded", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    let items = f.store.items(&f.session_id, 0, 100).unwrap();
    let perm = items
        .iter()
        .find(|i| i.kind == ItemKind::Permission)
        .unwrap();
    assert_eq!(perm.status, Some(ItemStatus::Denied));
    assert!(
        perm.content["reason"]
            .as_str()
            .unwrap()
            .contains("plan mode")
    );
    let tool = items.iter().find(|i| i.kind == ItemKind::ToolCall).unwrap();
    assert_eq!(tool.status, Some(ItemStatus::Failed));
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn fs_methods_serve_the_workspace() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    std::fs::write(f.cfg.cwd.join("notes.txt"), "line one\nline two\n").unwrap();
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("socket", Duration::from_secs(10), || sock.exists());
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });

    send_ok(
        &sock,
        Command::Prompt {
            text: "readfile:notes.txt".into(),
            attachments: vec![],
            steer: false,
            from: None,
        },
    );
    wait_until("turn 1", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    let turns = f.store.turns(&f.thread_id).unwrap();
    assert_eq!(
        turns[0].summary.assistant_text,
        "file says: line one\nline two\n"
    );

    send_ok(
        &sock,
        Command::Prompt {
            text: "writefile:out/hello.txt:hi there".into(),
            attachments: vec![],
            steer: false,
            from: None,
        },
    );
    wait_until("turn 2", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 2
    });
    assert_eq!(
        std::fs::read_to_string(f.cfg.cwd.join("out/hello.txt")).unwrap(),
        "hi there"
    );

    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();

    // Plan mode refuses writes.
    let f = fixture(
        RuntimeMode::Plan,
        fake_harness(),
        Some("writefile:nope.txt:x"),
    );
    let host = spawn_host(&f);
    wait_until("turn", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    assert_eq!(
        f.store.turns(&f.thread_id).unwrap()[0]
            .summary
            .assistant_text,
        "write denied"
    );
    assert!(!f.cfg.cwd.join("nope.txt").exists());
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn attachments_become_prompt_blocks_and_land_on_the_item() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let png = f.cfg.cwd.join("shot.png");
    std::fs::write(&png, b"\x89PNGdata").unwrap();
    let md = f.cfg.cwd.join("notes.md");
    std::fs::write(&md, "# notes").unwrap();
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    send_ok(
        &sock,
        Command::Prompt {
            text: "see these".into(),
            attachments: vec![png.clone(), md.clone()],
            steer: false,
            from: None,
        },
    );
    wait_until("turn", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();

    let user = f
        .store
        .items(&f.session_id, 0, 100)
        .unwrap()
        .into_iter()
        .find(|i| i.kind == ItemKind::UserMessage)
        .unwrap();
    assert_eq!(user.content["text"], "see these");
    assert_eq!(
        user.content["attachments"],
        serde_json::json!([png.display().to_string(), md.display().to_string()])
    );

    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    let prompt = log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|fr| fr["frame"]["method"] == "session/prompt")
        .unwrap();
    let blocks = prompt["frame"]["params"]["prompt"]
        .as_array()
        .unwrap()
        .clone();
    // The fake agent advertises image support, so the png is inlined.
    assert_eq!(blocks.len(), 4, "{blocks:#?}");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "see these");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["mimeType"], "image/png");
    assert_eq!(blocks[1]["data"], "iVBOR2RhdGE=");
    assert_eq!(blocks[2]["type"], "resource_link");
    assert_eq!(blocks[2]["uri"], format!("file://{}", md.display()));
    assert_eq!(blocks[2]["name"], "notes.md");
    assert_eq!(blocks[2]["mimeType"], "text/markdown");
    assert_eq!(blocks[3]["type"], "text");
    assert_eq!(
        blocks[3]["text"],
        format!("Attached files:\n- {}\n- {}", png.display(), md.display())
    );
}

#[test]
fn cancel_ends_the_turn_as_cancelled() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("sleep"));
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("running", Duration::from_secs(10), || {
        matches!(
            socket::send(&sock, &Command::Status),
            Ok(Reply::Status {
                state: SessionState::Running,
                turn: 1,
                ..
            })
        )
    });
    send_ok(&sock, Command::Cancel);
    wait_until("turn recorded", Duration::from_secs(20), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    assert_eq!(
        f.store.turns(&f.thread_id).unwrap()[0].summary.stop_reason,
        "cancelled"
    );
    let end = f
        .store
        .items(&f.session_id, 0, 100)
        .unwrap()
        .into_iter()
        .find(|i| i.kind == ItemKind::TurnEnd)
        .unwrap();
    assert_eq!(end.status, Some(ItemStatus::Cancelled));
    assert!(
        f.pane.text().contains("turn 1 cancelled"),
        "{}",
        f.pane.text()
    );
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn adapter_exit_ends_the_session() {
    // `true` exits immediately without speaking ACP: initialize fails, the session ends.
    let f = fixture(RuntimeMode::Autopilot, Harness::Custom("true".into()), None);
    let host = spawn_host(&f);
    host.join().unwrap().unwrap();
    let session = f.store.get_session(&f.session_id).unwrap().unwrap();
    assert_eq!(session.state, SessionState::Ended);
    let reason = session.ended_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("setup failed")
            || reason.contains("adapter exited")
            || reason.contains("connection error"),
        "{session:?}"
    );
    assert!(!f.cfg.socket_path.exists());
}

#[test]
fn missing_adapter_is_an_error() {
    let f = fixture(
        RuntimeMode::Autopilot,
        Harness::Custom("definitely-not-a-real-adapter-xyz".into()),
        None,
    );
    let host = spawn_host(&f);
    let err = host.join().unwrap().unwrap_err();
    assert!(err.to_string().contains("failed to spawn adapter"), "{err}");
}

/// `AXEL_ACP_REAL=1 cargo test -p axel-acp real_goose -- --ignored --nocapture` with `goose` on PATH.
#[test]
#[ignore = "needs goose on PATH and AXEL_ACP_REAL=1"]
fn real_goose_smoke() {
    if std::env::var("AXEL_ACP_REAL").as_deref() != Ok("1") {
        eprintln!("AXEL_ACP_REAL != 1; skipping");
        return;
    }
    if std::process::Command::new("goose")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("goose not on PATH; skipping");
        return;
    }
    let f = fixture(
        RuntimeMode::Plan,
        Harness::Goose,
        Some("Reply with exactly the word pong and nothing else."),
    );
    let host = spawn_host(&f);
    wait_until("a turn", Duration::from_secs(180), || {
        f.store.turns(&f.thread_id).unwrap().len() == 1
    });
    let turns = f.store.turns(&f.thread_id).unwrap();
    eprintln!(
        "goose said: {:?} ({})",
        turns[0].summary.assistant_text, turns[0].summary.stop_reason
    );
    assert!(
        f.store
            .items(&f.session_id, 0, 100)
            .unwrap()
            .iter()
            .any(|i| i.kind == ItemKind::AssistantMessage)
    );
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
    eprintln!("{}", f.pane.text());
}

fn queued_texts(f: &Fixture) -> Vec<String> {
    f.store
        .items(&f.session_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == ItemKind::UserMessage && i.status == Some(ItemStatus::Pending))
        .map(|i| i.content["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn user_texts(f: &Fixture) -> Vec<(String, Option<ItemStatus>)> {
    f.store
        .items(&f.session_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == ItemKind::UserMessage)
        .map(|i| (i.content["text"].as_str().unwrap_or_default().to_string(), i.status))
        .collect()
}

#[test]
fn prompts_sent_mid_turn_queue_and_steer_interrupts() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("sleep"));
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("running", Duration::from_secs(10), || {
        matches!(
            socket::send(&sock, &Command::Status),
            Ok(Reply::Status { state: SessionState::Running, turn: 1, .. })
        )
    });
    let prompt = |text: &str, steer: bool| {
        socket::send(&sock, &Command::Prompt { text: text.into(), attachments: vec![], steer, from: None }).unwrap()
    };
    // Two prompts mid-turn are queued as pending user messages, in order.
    let Reply::Queued { .. } = prompt("sleep", false) else { panic!("expected queued") };
    let Reply::Queued { item_id: second } = prompt("hello later", false) else { panic!("expected queued") };
    let Reply::Queued { item_id: dropped } = prompt("never mind", false) else { panic!("expected queued") };
    assert_eq!(queued_texts(&f), vec!["sleep", "hello later", "never mind"]);
    assert!(matches!(socket::send(&sock, &Command::Status), Ok(Reply::Status { queued: 3, .. })));

    // Dequeue removes one; unknown ids are refused.
    send_ok(&sock, Command::Dequeue { item_id: dropped.clone() });
    assert_eq!(queued_texts(&f), vec!["sleep", "hello later"]);
    assert!(matches!(socket::send(&sock, &Command::Dequeue { item_id: dropped }), Ok(Reply::Error { .. })));

    // Steering the second one cancels turn 1 and runs it next, ahead of the first.
    send_ok(&sock, Command::Steer { item_id: second });
    wait_until("steered turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    let turns = f.store.turns(&f.thread_id).unwrap();
    assert_eq!(turns[0].summary.stop_reason, "cancelled");
    assert_eq!(turns[1].summary.user_text, "hello later");
    assert_eq!(turns[1].summary.stop_reason, "end_turn");

    // The remaining queued prompt starts by itself once the steered turn ends.
    wait_until("queued turn running", Duration::from_secs(20), || {
        matches!(
            socket::send(&sock, &Command::Status),
            Ok(Reply::Status { state: SessionState::Running, turn: 3, queued: 0, .. })
        )
    });
    assert!(queued_texts(&f).is_empty());
    // Placeholders are replaced by the real message at turn start, so the order is the run order.
    assert_eq!(
        user_texts(&f),
        vec![
            ("sleep".into(), Some(ItemStatus::Completed)),
            ("hello later".into(), Some(ItemStatus::Completed)),
            ("sleep".into(), Some(ItemStatus::Completed)),
        ]
    );

    // A steer prompt from the composer jumps the queue and interrupts; a queued
    // prompt left behind at stop is kept as cancelled.
    let Reply::Queued { .. } = prompt("left behind", false) else { panic!("expected queued") };
    let Reply::Queued { .. } = prompt("sleep", true) else { panic!("expected queued") };
    wait_until("turn 4 running", Duration::from_secs(20), || {
        matches!(
            socket::send(&sock, &Command::Status),
            Ok(Reply::Status { state: SessionState::Running, turn: 4, queued: 1, .. })
        )
    });
    assert_eq!(f.store.turns(&f.thread_id).unwrap()[2].summary.stop_reason, "cancelled");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
    assert!(user_texts(&f).contains(&("left behind".into(), Some(ItemStatus::Cancelled))), "{:?}", user_texts(&f));
    assert!(queued_texts(&f).is_empty(), "nothing stays pending after the host stops");
}

#[test]
fn set_mode_mid_session_reaches_policy_writes_and_the_session_row() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    send_ok(&sock, Command::SetMode { mode: RuntimeMode::Plan });
    wait_until("mode recorded", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().runtime_mode == RuntimeMode::Plan
    });
    send_ok(
        &sock,
        Command::Prompt { text: "writefile:nope.txt:x".into(), attachments: vec![], steer: false, from: None },
    );
    wait_until("turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    assert_eq!(f.store.turns(&f.thread_id).unwrap()[0].summary.assistant_text, "write denied");
    assert!(!f.cfg.cwd.join("nope.txt").exists());
    // And back: writes work again.
    send_ok(&sock, Command::SetMode { mode: RuntimeMode::Autopilot });
    send_ok(
        &sock,
        Command::Prompt { text: "writefile:yes.txt:ok".into(), attachments: vec![], steer: false, from: None },
    );
    wait_until("turn 2", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    assert_eq!(std::fs::read_to_string(f.cfg.cwd.join("yes.txt")).unwrap(), "ok");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn agent_models_resolve_against_the_adapters_list() {
    use axel_acp::resolve_model;
    let listed: Vec<serde_json::Value> = serde_json::from_str(
        r#"[{"modelId":"default","name":"Default (recommended)"},{"modelId":"opus","name":"Opus 5.5"},
            {"modelId":"claude-opus-5","name":"Opus 5"},{"modelId":"fable","name":"Fable 5.1"},
            {"modelId":"haiku","name":"Haiku 5.5"}]"#,
    )
    .unwrap();
    assert_eq!(resolve_model("fable", &listed).as_deref(), Some("fable"));
    assert_eq!(resolve_model("Fable", &listed).as_deref(), Some("fable"), "any case");
    assert_eq!(resolve_model("opus 5", &listed).as_deref(), Some("claude-opus-5"), "by display name");
    assert_eq!(resolve_model("opus", &listed).as_deref(), Some("opus"), "exact id before a substring");
    assert_eq!(resolve_model("Fable 5.1", &listed).as_deref(), Some("fable"));
    assert_eq!(resolve_model("claude-fable-5-1", &listed), None, "unlisted ids go through unchanged");
}

#[test]
fn prompts_from_another_agent_are_framed_for_the_agent_and_tagged_on_the_item() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    let from = axel_acp::socket::Sender::new("PKnova", "nova");
    send_ok(
        &sock,
        Command::Prompt { text: "can you check auth.rs?".into(), attachments: vec![], steer: false, from: Some(from) },
    );
    wait_until("turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    // The thread shows the message as written, tagged with its sender.
    let item = f
        .store
        .thread_items(&f.thread_id, 100)
        .unwrap()
        .into_iter()
        .find(|it| it.kind == ItemKind::UserMessage)
        .unwrap();
    assert_eq!(item.content["text"], "can you check auth.rs?");
    assert_eq!(item.content["from"], "PKnova");
    // The agent reads who wrote it and how to answer.
    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    assert!(log.contains("Message from nova, another Axel agent"), "{log}");
    assert!(log.contains("your answer goes back to nova by itself"), "{log}");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn each_turn_starts_with_what_the_agent_missed_and_notes_wait_for_it() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    // Another agent, nova, already worked on the thread: the owner asked it
    // something and it answered. Only the owner saw that.
    let nova = Keypair::generate();
    f.store
        .insert_agent(&axel_store::NewAgent {
            pubkey: nova.public().to_base58(),
            name: "nova".into(),
            harness: Harness::Claude,
            model: None,
            skills: vec![],
            prompt: None,
            color: None,
            runtime_mode: RuntimeMode::default(),
            auth_tag: None,
        })
        .unwrap();
    let nova_session = f
        .store
        .start_session(
            &f.owner,
            &NewSession {
                thread_id: f.thread_id.clone(),
                agent_pubkey: nova.public().to_base58(),
                environment_id: "env-local".into(),
                environment_label: "local".into(),
                project_repo_key: None,
                harness: Harness::Claude,
                runtime_mode: RuntimeMode::Autopilot,
                resumed_from: None,
                runtime: SessionRuntime::Managed,
            },
        )
        .unwrap();
    let said = |kind: ItemKind, text: &str| {
        f.store
            .push_item(&axel_store::NewItem {
                session_id: nova_session.id.clone(),
                thread_id: f.thread_id.clone(),
                turn: 1,
                kind,
                status: Some(ItemStatus::Completed),
                ref_id: None,
                content: serde_json::json!({ "text": text }),
            })
            .unwrap();
    };
    said(ItemKind::UserMessage, "compare the two reports");
    said(ItemKind::AssistantMessage, "Harmonic is ahead on A1.6 to A1.8.");

    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    send_ok(&sock, Command::Prompt { text: "@atlas what do you think?".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 1", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);

    // The agent read nova's answer and who else is here before the question.
    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    assert!(log.contains("You're joining a thread that's already under way"), "{log}");
    assert!(log.contains(r"[The owner, to nova]\ncompare the two reports"), "{log}");
    assert!(log.contains(r"[nova replied]\nHarmonic is ahead on A1.6 to A1.8."), "{log}");
    assert!(log.contains("Also on this thread: nova (idle)"), "{log}");
    assert!(log.contains(r"---\n\n@atlas what do you think?"), "the question comes last: {log}");
    // The thread shows the question as typed.
    assert!(user_texts(&f).iter().any(|(t, _)| t == "@atlas what do you think?"), "{:?}", user_texts(&f));

    // A note from nova starts no turn; it shows on the thread.
    let from = axel_acp::socket::Sender::new(nova.public().to_base58(), "nova");
    send_ok(&sock, Command::Note { text: "FYI: the QEDIT build passes now.".into(), from });
    wait_until("the note on the thread", Duration::from_secs(5), || {
        user_texts(&f).iter().any(|(t, _)| t == "FYI: the QEDIT build passes now.")
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.store.turns(&f.thread_id).unwrap().len(), 1, "a note doesn't start a turn");
    said(ItemKind::AssistantMessage, "Second finding: A2 needs the two-run form.");

    // The next turn carries only what's new: the note and nova's new reply.
    send_ok(&sock, Command::Prompt { text: "and now?".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 2", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    assert!(log.contains("Since your last turn on this thread"), "{log}");
    assert!(log.contains(r"[nova left you a note (no reply needed)]\nFYI: the QEDIT build passes now."), "{log}");
    assert!(log.contains(r"[nova replied]\nSecond finding: A2 needs the two-run form."), "{log}");
    assert_eq!(log.matches("Harmonic is ahead").count(), 1, "what it already saw isn't repeated: {log}");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn the_host_reports_program_status_to_its_pane_as_osc_7501() {
    use base64::Engine;
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    send_ok(&sock, Command::Prompt { text: "fix the login bug".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    // Finished: done, a result the owner hasn't seen, with the reply's first line.
    // (Recorded just after the turn: wait for it.)
    wait_until("done", Duration::from_secs(10), || {
        f.store.status(&f.session_id).unwrap().is_some_and(|st| st.state == axel_store::StatusState::Done)
    });
    let st = f.store.status(&f.session_id).unwrap().unwrap();
    assert_eq!(st.state, axel_store::StatusState::Done);
    assert_eq!(st.msg.as_deref(), Some("Hello, world.Tests pass."));
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();

    let b64 = |s: &str| base64::prelude::BASE64_STANDARD.encode(s);
    let pane = f.pane.text();
    let reports: Vec<&str> = pane.split("\x1b]7501;").skip(1).map(|r| r.split("\x1b\\").next().unwrap()).collect();
    let states: Vec<&str> = reports.iter().map(|r| r.split(':').next().unwrap()).collect();
    // working while the turn runs, done when its result is ready, cleared when the session ends.
    let working = states.iter().position(|s| *s == "state=working").expect("working");
    let done = states.iter().position(|s| *s == "state=done").expect("done");
    assert!(working < done && states.last() == Some(&"state=clear"), "{states:?}");
    // Every report is whole: app and the agent's name as title, every time.
    for r in &reports {
        assert!(r.contains(":app=axel") && r.contains(&format!(":title={}", b64("agent"))), "{r}");
    }
    assert!(reports[working].contains(&format!(":msg={}", b64("fix the login bug"))), "{}", reports[working]);
}

#[test]
fn a_question_waits_for_the_owner_even_in_autopilot() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    send_ok(&sock, Command::Prompt { text: "ask".into(), attachments: vec![], steer: false, from: None });
    // Autopilot would allow a permission at once; a question waits for the owner.
    let question = || f.store.items(&f.session_id, 0, 1000).unwrap().into_iter().find(|i| i.kind == ItemKind::Question);
    wait_until("the question", Duration::from_secs(10), || question().is_some_and(|q| q.status == Some(ItemStatus::Pending)));
    std::thread::sleep(Duration::from_millis(200));
    let q = question().unwrap();
    assert_eq!(q.status, Some(ItemStatus::Pending), "nobody answered for the owner");
    assert_eq!(q.content["title"], "Database: Which one?");
    assert_eq!(f.store.get_session(&f.session_id).unwrap().unwrap().state, SessionState::Blocked);
    assert!(f.pane.text().contains("state=blocked:kind=question"), "Rex hears it's a question");

    send_ok(&sock, Command::Answer { item_id: q.id.clone(), option_id: "answer:1".into() });
    wait_until("turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    assert_eq!(question().unwrap().status, Some(ItemStatus::Answered));
    let said = f.store.items(&f.session_id, 0, 1000).unwrap().into_iter().filter(|i| i.kind == ItemKind::AssistantMessage).map(|i| i.content["text"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    assert!(said.iter().any(|t| t.contains("picked answer:1")), "the agent got the pick: {said:?}");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

/// A stand-in `axel` that records how `send --reply` was called.
fn reply_recorder(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("reply.log");
    let exe = dir.join("axel-stub");
    std::fs::write(&exe, format!("#!/bin/sh\nprintf '%s|' \"$@\" >> '{}'\necho \" thread=$AXEL_THREAD agent=$AXEL_AGENT\" >> '{}'\n", log.display(), log.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    (exe, log)
}

#[test]
fn a_turn_another_agent_started_returns_its_answer_to_that_agent() {
    let mut f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let (exe, log) = reply_recorder(f._dir.path());
    f.cfg.reply_with = Some(exe);
    f.cfg.env = vec![("AXEL_THREAD".into(), f.thread_id.clone()), ("AXEL_AGENT".into(), "PKatlas".into())];
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    // The owner's prompt: nothing to return.
    send_ok(&sock, Command::Prompt { text: "hi".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 1", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    assert!(!log.exists(), "the owner reads the answer on the thread");
    // nova's message: the answer goes back to nova, as this agent, on this thread.
    let nova = axel_acp::socket::Sender::new("PKnova", "nova");
    send_ok(&sock, Command::Prompt { text: "can you check?".into(), attachments: vec![], steer: false, from: Some(nova.clone()) });
    wait_until("the answer returned", Duration::from_secs(20), || log.exists());
    let called = std::fs::read_to_string(&log).unwrap();
    // The turn's last message, not the working-out-loud before its tool call.
    assert!(called.starts_with("send|--reply|--to|nova|--|Tests pass.|"), "{called}");
    assert!(called.contains(&format!("thread={}", f.thread_id)) && called.contains("agent=PKatlas"), "{called}");
    // nova's answer starts a turn: nothing goes back, or they'd answer each other forever.
    std::fs::remove_file(&log).unwrap();
    let answer = axel_acp::socket::Sender { reply: true, ..nova };
    send_ok(&sock, Command::Prompt { text: "here you go".into(), attachments: vec![], steer: false, from: Some(answer) });
    wait_until("turn 3", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 3);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!log.exists(), "an answer isn't answered");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

/// The host on Axel's own Claude adapter, with `fake_claude` as Claude Code.
fn claude_fixture(initial_prompt: Option<&str>) -> (Fixture, PathBuf) {
    let mut f = fixture(RuntimeMode::Autopilot, Harness::Custom(env!("CARGO_BIN_EXE_claude_acp").to_string()), initial_prompt);
    let home = f._dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = home.join("claude.log");
    f.cfg.env = vec![
        ("CLAUDE_CODE_EXECUTABLE".into(), env!("CARGO_BIN_EXE_fake_claude").into()),
        ("FAKE_CLAUDE_LOG".into(), log.display().to_string()),
        ("HOME".into(), home.display().to_string()),
        ("CLAUDE_CONFIG_DIR".into(), home.join(".claude").display().to_string()),
    ];
    (f, log)
}

fn item_of(f: &Fixture, id: &str) -> Option<axel_store::ConversationItem> {
    f.store.item(id).unwrap()
}

fn running_turn(sock: &Path, turn: i64) {
    wait_until(&format!("turn {turn} running"), Duration::from_secs(20), || {
        matches!(socket::send(sock, &Command::Status), Ok(Reply::Status { state: SessionState::Running, turn: t, .. }) if t == turn)
    });
}

#[test]
fn a_message_sent_mid_turn_goes_into_it_and_can_be_edited_until_read() {
    let (f, claude_log) = claude_fixture(Some("work"));
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    running_turn(&sock, 1);
    let prompt = |text: &str| socket::send(&sock, &Command::Prompt { text: text.into(), attachments: vec![], steer: false, from: None }).unwrap();

    // Into the running turn, not after it: pending until the agent reads it.
    let Reply::Steering { item_id: first } = prompt("also this") else { panic!("expected steering") };
    let it = item_of(&f, &first).unwrap();
    assert_eq!((it.status, it.content["steering"].clone()), (Some(ItemStatus::Pending), serde_json::json!(true)));
    assert!(matches!(socket::send(&sock, &Command::Status), Ok(Reply::Status { turn: 1, queued: 1, .. })), "no new turn");

    // Not read yet: it can be edited, in place.
    send_ok(&sock, Command::Edit { item_id: first.clone(), text: "also this, edited".into() });
    assert_eq!(item_of(&f, &first).unwrap().content["text"], "also this, edited");
    assert!(std::fs::read_to_string(&claude_log).unwrap().contains("cancel_async_message"), "withdrawn from Claude's queue, then sent again");

    // The agent reads both at its next step; one turn for all of it.
    let Reply::Steering { item_id: second } = prompt("fold") else { panic!("expected steering") };
    wait_until("turn 1 ended", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    let turns = f.store.turns(&f.thread_id).unwrap();
    assert_eq!(turns[0].summary.stop_reason, "end_turn");
    assert!(turns[0].summary.assistant_text.contains("read: also this, edited, fold"), "{:?}", turns[0].summary.assistant_text);
    for id in [&first, &second] {
        let it = item_of(&f, id).unwrap();
        assert_eq!(it.status, Some(ItemStatus::Completed), "read: {:?}", it.content);
        assert_eq!(it.content.get("steering"), None);
        assert_eq!(it.content.get("queued"), None);
    }
    // Read: too late to edit.
    assert!(matches!(socket::send(&sock, &Command::Edit { item_id: first, text: "x".into() }), Ok(Reply::Error { .. })));

    // Withdrawn before it's read: gone.
    let Reply::Ok = prompt("work") else { panic!("a new turn") };
    running_turn(&sock, 2);
    let Reply::Steering { item_id: oops } = prompt("oops") else { panic!("expected steering") };
    send_ok(&sock, Command::Dequeue { item_id: oops.clone() });
    assert!(item_of(&f, &oops).is_none());

    // Cancelled with one not read yet: it runs next, as a turn of its own.
    let Reply::Steering { item_id: next } = prompt("next please") else { panic!("expected steering") };
    send_ok(&sock, Command::Cancel);
    wait_until("turn 3 ended", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 3);
    let turns = f.store.turns(&f.thread_id).unwrap();
    assert_eq!(turns[1].summary.stop_reason, "cancelled");
    assert_eq!(turns[2].summary.user_text, "next please");
    assert!(item_of(&f, &next).is_none(), "its placeholder gave way to the turn's own message");

    // Another agent's message mid-turn still waits for its own turn, so its answer goes back to it.
    let Reply::Ok = prompt("work") else { panic!("a new turn") };
    running_turn(&sock, 4);
    let from = axel_acp::socket::Sender::new("PKnova", "nova");
    let reply = socket::send(&sock, &Command::Prompt { text: "from nova".into(), attachments: vec![], steer: false, from: Some(from) }).unwrap();
    assert!(matches!(reply, Reply::Queued { .. }), "{reply:?}");
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn a_turns_spend_and_context_fill_land_in_the_store() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("spend"));
    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("turn 1", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    let u = f.store.usage(&f.session_id).unwrap().expect("usage recorded");
    assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens, u.turns), (10, 50, 100, 1));
    assert!((u.cost_usd_est - 0.5).abs() < 1e-9, "the reported cost, not an estimate");
    let ctx = f.store.session_contexts().unwrap();
    assert_eq!((ctx[0].used, ctx[0].size), (1234, 200_000));
    let limits = f.store.plan_limits().unwrap();
    assert_eq!((limits[0].window, limits[0].used_pct, limits[0].resets_at), (axel_store::LimitWindow::FiveHour, 30.0, Some(1000)));
    assert_eq!(limits[0].harness, fake_harness(), "kept under the harness that said it");

    // Usage not in Axel's terms isn't summed.
    send_ok(&sock, Command::Prompt { text: "spend-unmarked".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 2", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    assert_eq!(f.store.usage(&f.session_id).unwrap().unwrap().turns, 1);
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn custom_agents_select_models_with_config_options_and_keep_legacy_support() {
    for api in ["config", "legacy"] {
        let mut f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("hello"));
        f.cfg.env.push(("FAKE_AGENT_MODELS".into(), api.into()));
        f.cfg.model = Some("Flash".into());
        let host = spawn_host(&f);
        wait_until("configured turn", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
        assert!(f.pane.text().contains("model provider/flash"), "{api}: {}", f.pane.text());
        assert!(!f.store.items(&f.session_id, 0, 1000).unwrap().iter().any(|i| i.kind == ItemKind::Error), "{api}: {}", f.pane.text());
        send_ok(&f.cfg.socket_path, Command::Stop);
        host.join().unwrap().unwrap();
    }
}

#[test]
fn standard_usage_records_cumulative_usd_cost_once_without_billing_context_tokens() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("spend-standard:0.125"));
    let host = spawn_host(&f);
    wait_until("first cost", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    send_ok(&f.cfg.socket_path, Command::Prompt { text: "spend-standard:0.2".into(), attachments: vec![], steer: false, from: None });
    wait_until("second cost", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    let usage = f.store.usage(&f.session_id).unwrap().unwrap();
    assert!((usage.cost_usd_est - 0.2).abs() < 1e-9);
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.turns), (0, 0, 2));
    let context = f.store.session_contexts().unwrap();
    assert_eq!((context[0].used, context[0].size), (4763, 1000000));
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn standard_cost_is_not_added_again_from_extensions_or_prompt_replies() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("spend-mixed"));
    let host = spawn_host(&f);
    wait_until("mixed usage", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    let usage = f.store.usage(&f.session_id).unwrap().unwrap();
    assert!((usage.cost_usd_est - 0.5).abs() < 1e-9);
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.turns), (10, 50, 1));
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn restarting_a_host_does_not_charge_its_existing_cumulative_cost_again() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("spend-standard:0.2"));
    f.store.add_usage(&f.owner, &f.session_id, &axel_store::UsageDelta {
        cost_usd_est: 0.125,
        ..Default::default()
    }, None).unwrap();
    let host = spawn_host(&f);
    wait_until("continued usage", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    assert!((f.store.usage(&f.session_id).unwrap().unwrap().cost_usd_est - 0.2).abs() < 1e-9);
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}

#[test]
fn a_turn_in_a_git_workspace_leaves_checkpoints_and_what_it_changed() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), None);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&f.cfg.cwd).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
    ] {
        git(args);
    }
    std::fs::write(f.cfg.cwd.join("notes.md"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "base"]);
    let head = git(&["rev-parse", "HEAD"]);

    let host = spawn_host(&f);
    let sock = f.cfg.socket_path.clone();
    wait_until("idle after setup", Duration::from_secs(10), || {
        f.store.get_session(&f.session_id).unwrap().unwrap().state == SessionState::Idle
    });
    // Turn 1: the agent rewrites notes.md through the client's fs (no edit tool call).
    send_ok(&sock, Command::Prompt { text: "writefile:notes.md:one\ntwo\n".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 1", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 1);
    // The owner adds a file between turns; turn 2 changes nothing itself.
    std::fs::write(f.cfg.cwd.join("between.txt"), "owner\n").unwrap();
    send_ok(&sock, Command::Prompt { text: "fix the login bug".into(), attachments: vec![], steer: false, from: None });
    wait_until("turn 2", Duration::from_secs(20), || f.store.turns(&f.thread_id).unwrap().len() == 2);
    send_ok(&sock, Command::Stop);
    host.join().unwrap().unwrap();

    let turns = f.store.turns(&f.thread_id).unwrap();
    let c1 = turns[0].summary.changes.as_ref().expect("turn 1 left checkpoints");
    assert_eq!(c1.before, head, "a clean tree before the turn is HEAD itself");
    assert_ne!(c1.after, head);
    assert_eq!(c1.files.len(), 1, "{:?}", c1.files);
    let f1 = &c1.files[0];
    assert_eq!((f1.path.as_str(), f1.added, f1.removed, f1.authored), ("notes.md", Some(1), Some(0), false));
    assert!(!c1.between_turns && !c1.truncated);
    let c2 = turns[1].summary.changes.as_ref().expect("turn 2 left checkpoints");
    assert_ne!(c2.before, c1.after, "the owner's file makes the tree at the start of turn 2 another one…");
    assert!(c2.between_turns, "…which the turn says");
    // The owner's file is already in turn 2's `before`, so the turn's own diff is empty.
    assert!(c2.files.is_empty(), "{:?}", c2.files);
    assert_eq!(c2.before, c2.after);

    // Both ends of both turns are kept as refs; HEAD, the index and the branches are untouched.
    let refs = git(&["for-each-ref", "--format=%(refname)", "refs/axel/checkpoints"]);
    let prefix = format!("refs/axel/checkpoints/{}/{}", f.thread_id, f.session_id);
    assert_eq!(
        refs.lines().collect::<Vec<_>>(),
        vec![format!("{prefix}/1/after"), format!("{prefix}/1/before"), format!("{prefix}/2/after"), format!("{prefix}/2/before")]
    );
    assert_eq!(git(&["rev-parse", "HEAD"]), head);
    assert!(git(&["diff", "--cached", "--name-only"]).is_empty());
    assert_eq!(git(&["branch", "--list"]).lines().count(), 1);
    // The local TurnEnd item carries the same, for the thread view.
    let ends: Vec<_> = f.store.items(&f.session_id, 0, 500).unwrap().into_iter().filter(|i| i.kind == ItemKind::TurnEnd).collect();
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0].content["changes"]["files"][0]["path"], "notes.md");
    assert_eq!(ends[1].content["changes"]["between_turns"], true);
}

#[test]
fn the_owner_message_is_visible_while_the_before_checkpoint_is_still_running() {
    let f = fixture(RuntimeMode::Autopilot, fake_harness(), Some("publish before repository work"));
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(&f.cfg.cwd).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    std::fs::write(f.cfg.cwd.join(".gitattributes"), "*.slow filter=axel-test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "base"]);
    std::fs::write(f.cfg.cwd.join(".git/slow-filter"), "touch .git/filter-started\nwhile [ ! -e .git/filter-release ]; do sleep 0.02; done\ncat\n").unwrap();
    git(&["config", "filter.axel-test.clean", "sh .git/slow-filter"]);
    std::fs::write(f.cfg.cwd.join("new.slow"), "content\n").unwrap();
    struct Release(PathBuf);
    impl Drop for Release { fn drop(&mut self) { let _ = std::fs::write(&self.0, "release"); } }
    let release = Release(f.cfg.cwd.join(".git/filter-release"));
    let host = spawn_host(&f);
    wait_until("checkpoint filter started", Duration::from_secs(5), || f.cfg.cwd.join(".git/filter-started").exists());
    let messages = f.store.items(&f.session_id, 0, 100).unwrap();
    assert!(messages.iter().any(|i| i.kind == ItemKind::UserMessage && i.content["text"] == "publish before repository work"));
    let log = std::fs::read_to_string(&f.cfg.log_path).unwrap();
    assert!(!log.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).any(|v| v["dir"] == "out" && v["frame"]["method"] == "session/prompt"), "the agent must still wait for the before snapshot");
    drop(release);
    wait_until("turn completed", Duration::from_secs(5), || !f.store.turns(&f.thread_id).unwrap().is_empty());
    send_ok(&f.cfg.socket_path, Command::Stop);
    host.join().unwrap().unwrap();
}
