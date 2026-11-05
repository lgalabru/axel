use std::path::{Path, PathBuf};

use axel_identity::{Keypair, attest};
use axel_inbox::{Envelope, EnvelopeBuilder};
use rusqlite::Connection;

use super::*;

fn store() -> Store {
    Store::open_memory().expect("open_memory")
}

fn pk(kp: &Keypair) -> String {
    kp.public().to_base58()
}

fn agent_spec(name: &str, kp: &Keypair, owner: Option<&Keypair>) -> NewAgent {
    NewAgent {
        pubkey: pk(kp),
        name: name.to_string(),
        harness: Harness::Claude,
        model: Some("sonnet".to_string()),
        skills: vec!["*".to_string()],
        prompt: None,
        color: Some(AgentColor::Gray),
        runtime_mode: RuntimeMode::default(),
        auth_tag: owner.map(|o| attest(o, &kp.public(), "").unwrap().to_tag()),
    }
}

fn note(signer: &Keypair, content: &str, mentions: &[&str], thread: Option<&str>) -> Envelope {
    let mut b = EnvelopeBuilder::new(kind::NOTE, content);
    for m in mentions {
        b = b.mention(m);
    }
    if let Some(t) = thread {
        b = b.thread(t);
    }
    b.sign(signer).unwrap()
}

fn new_session(thread_id: &str, agent: &Keypair, env_label: &str) -> NewSession {
    NewSession {
        thread_id: thread_id.to_string(),
        agent_pubkey: pk(agent),
        environment_id: format!("env-{env_label}"),
        environment_label: env_label.to_string(),
        project_repo_key: Some("github.com/acme/axel".into()),
        harness: Harness::Claude,
        runtime_mode: RuntimeMode::Autopilot,
        resumed_from: None,
        runtime: SessionRuntime::Pane,
    }
}

fn item(
    session: &Session,
    turn: i64,
    kind: ItemKind,
    status: Option<ItemStatus>,
    ref_id: Option<&str>,
    content: serde_json::Value,
) -> NewItem {
    NewItem {
        session_id: session.id.clone(),
        thread_id: session.thread_id.clone(),
        turn,
        kind,
        status,
        ref_id: ref_id.map(str::to_string),
        content,
    }
}

fn ws(kind: WorkspaceKind, branch: Option<&str>) -> ThreadWorkspace {
    ThreadWorkspace {
        kind,
        branch: branch.map(str::to_string),
        worktree_path: branch.map(|b| PathBuf::from(format!("/wt/{b}"))),
    }
}

fn ids(v: &[Envelope]) -> Vec<String> {
    v.iter().map(|e| e.id.clone()).collect()
}

/// Deterministic xorshift shuffle (no rand dependency).
fn shuffle<T>(v: &mut [T], mut seed: u64) {
    for i in (1..v.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        v.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

// --- schema -------------------------------------------------------------------

#[test]
fn migrations_run_and_are_idempotent() {
    let s = store();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let mut conn = Connection::open_in_memory().unwrap();
    schema::migrate(&mut conn).unwrap();
    schema::migrate(&mut conn).unwrap();
    assert_eq!(schema::current_version(&conn).unwrap(), SCHEMA_VERSION);
    let applied: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(applied, SCHEMA_VERSION);
}

#[test]
fn newer_schema_is_refused() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
        .unwrap();
    assert!(matches!(
        schema::migrate(&mut conn),
        Err(Error::SchemaTooNew { .. })
    ));
}

#[test]
fn open_on_disk_sets_wal_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("axel.db");
    let owner = Keypair::generate();
    let thread_id = {
        let s = Store::open(&path).unwrap();
        let mode: String = s
            .connection()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = s
            .connection()
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        s.create_thread(&owner, "persist", "survive reopen", None)
            .unwrap()
            .id
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(s.get_thread(&thread_id).unwrap().unwrap().title, "persist");
    assert!(Store::default_path().ends_with("axel.db"));
}

// --- local-only ---------------------------------------------------------------

#[test]
fn local_environment_is_created_once() {
    let s = store();
    let a = s.local_environment().unwrap();
    let b = s.local_environment().unwrap();
    assert_eq!(a, b);
    assert_eq!(a.id.len(), 26, "ulid");
    assert!(a.is_local);
    assert!(!a.label.is_empty());
    s.set_environment_rex_host(&a.id, "host-123").unwrap();
    assert_eq!(
        s.local_environment().unwrap().rex_host_id.as_deref(),
        Some("host-123")
    );
    assert!(matches!(
        s.set_environment_rex_host("nope", "x"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(s.list_environments().unwrap().len(), 1);
}

#[test]
fn project_upsert_touches_last_opened_at_and_repo_key() {
    let s = store();
    let env = s.local_environment().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p1 = s
        .upsert_project(&env.id, dir.path(), Some("github.com/acme/axel"))
        .unwrap();
    assert_eq!(p1.name, dir.path().file_name().unwrap().to_string_lossy());
    assert!(!p1.has_axel_md);
    assert_eq!(p1.repo_key.as_deref(), Some("github.com/acme/axel"));

    s.connection()
        .execute(
            "UPDATE projects SET last_opened_at = last_opened_at - 1000 WHERE id = ?1",
            [&p1.id],
        )
        .unwrap();
    std::fs::write(dir.path().join("AXEL.md"), "---\n---\n").unwrap();
    let p2 = s.upsert_project(&env.id, dir.path(), None).unwrap();
    assert_eq!(p2.id, p1.id);
    assert!(p2.has_axel_md);
    assert!(p2.last_opened_at >= p1.last_opened_at);
    assert_eq!(
        p2.repo_key.as_deref(),
        Some("github.com/acme/axel"),
        "None keeps the stored repo_key"
    );

    let other = s
        .upsert_project(&env.id, Path::new("/tmp/older"), None)
        .unwrap();
    s.connection()
        .execute(
            "UPDATE projects SET last_opened_at = last_opened_at - 5000 WHERE id = ?1",
            [&other.id],
        )
        .unwrap();
    let recent: Vec<String> = s
        .recent_projects(10)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(recent, vec![p2.id.clone(), other.id.clone()]);
    assert_eq!(
        s.get_project(&other.id).unwrap().unwrap().path,
        PathBuf::from("/tmp/older")
    );
    assert_eq!(
        s.project_by_repo_key(&env.id, "github.com/acme/axel")
            .unwrap()
            .unwrap()
            .id,
        p1.id
    );
    assert!(s.project_by_repo_key(&env.id, "nope").unwrap().is_none());
    assert!(matches!(
        s.upsert_project("missing-env", Path::new("/x"), None),
        Err(Error::Sqlite(_))
    ));
}

#[test]
fn an_agent_can_switch_harness_and_model() {
    let s = store();
    let kp = Keypair::generate();
    s.insert_agent(&agent_spec("astra", &kp, None)).unwrap();
    s.set_agent_harness(&pk(&kp), &Harness::Goose, Some("gpt-5.5")).unwrap();
    let a = s.agent_by_name("astra").unwrap().unwrap();
    assert_eq!((a.harness, a.model.as_deref()), (Harness::Goose, Some("gpt-5.5")));
    s.set_agent_harness(&pk(&kp), &Harness::Codex, None).unwrap();
    let a = s.agent_by_name("astra").unwrap().unwrap();
    assert_eq!((a.harness, a.model), (Harness::Codex, None), "no model is the harness's default");
    assert!(matches!(s.set_agent_harness("nobody", &Harness::Claude, None), Err(Error::NotFound(_))));
}

#[test]
fn agent_registry() {
    let s = store();
    let owner = Keypair::generate();
    let atlas = Keypair::generate();
    let a = s
        .insert_agent(&agent_spec("atlas", &atlas, Some(&owner)))
        .unwrap();
    assert_eq!(a.runtime_mode, RuntimeMode::Autopilot);
    assert_eq!(a.auth_tag.as_ref().unwrap().len(), 4);
    assert_eq!(s.agent_by_name("atlas").unwrap().unwrap(), a);
    assert_eq!(s.agent_by_pubkey(&pk(&atlas)).unwrap().unwrap(), a);
    assert_eq!(
        s.known_identity_name(&pk(&atlas)).unwrap().as_deref(),
        Some("atlas")
    );
    assert!(s.known_identity_name(&pk(&owner)).unwrap().is_none());

    let mut dup = agent_spec("atlas", &Keypair::generate(), None);
    dup.runtime_mode = RuntimeMode::Plan;
    assert!(
        matches!(s.insert_agent(&dup), Err(Error::Conflict(_))),
        "duplicate name"
    );
    assert!(
        matches!(
            s.insert_agent(&agent_spec("other", &atlas, None)),
            Err(Error::Conflict(_))
        ),
        "duplicate pubkey"
    );

    let zed = Keypair::generate();
    let mut spec = agent_spec("zed", &zed, None);
    spec.runtime_mode = RuntimeMode::Plan;
    assert_eq!(
        s.insert_agent(&spec).unwrap().runtime_mode,
        RuntimeMode::Plan
    );
    s.touch_agent(&pk(&zed)).unwrap();
    let names: Vec<String> = s
        .list_agents()
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, vec!["zed", "atlas"], "seen agents first");
    s.archive_agent(&pk(&atlas)).unwrap();
    assert_eq!(s.list_agents().unwrap().len(), 1);
    assert!(
        s.agent_by_name("atlas")
            .unwrap()
            .unwrap()
            .archived_at
            .is_some()
    );
    assert!(matches!(s.touch_agent("ghost"), Err(Error::NotFound(_))));
}

// --- the log ------------------------------------------------------------------

#[test]
fn append_is_idempotent_and_rejects_bad_signatures() {
    let s = store();
    let owner = Keypair::generate();
    let env = note(&owner, "hello", &[&pk(&owner)], None);

    let first = s.append(&env).unwrap();
    assert_eq!(
        first,
        Appended {
            inserted: true,
            projected: true,
            rejected_reason: None
        }
    );
    let second = s.append(&env).unwrap();
    assert_eq!(
        second,
        Appended {
            inserted: false,
            projected: true,
            rejected_reason: None
        }
    );
    assert_eq!(s.events_since(0, 100).unwrap().len(), 1);
    assert_eq!(s.inbox(&pk(&owner), 10).unwrap().len(), 1);

    let mut tampered = env.clone();
    tampered.content = "evil".into();
    assert!(matches!(s.append(&tampered), Err(Error::Envelope(_))));
    let mut bad_sig = env.clone();
    bad_sig.sig = "00".repeat(64);
    assert!(matches!(s.append(&bad_sig), Err(Error::Envelope(_))));
    let mut wrong_signer = env.clone();
    wrong_signer.from = pk(&Keypair::generate());
    assert!(matches!(s.append(&wrong_signer), Err(Error::Envelope(_))));
    assert_eq!(
        s.events_since(0, 100).unwrap().len(),
        1,
        "nothing invalid was stored"
    );

    // Usage is a sum of deltas, so a duplicate delivery must not double count.
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let sess = s
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    s.add_usage(
        &owner,
        &sess.id,
        &UsageDelta {
            input_tokens: 10,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let usage_env = s
        .thread_events(&t.id)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == kind::USAGE)
        .unwrap();
    assert!(!s.append(&usage_env).unwrap().inserted);
    assert_eq!(s.usage(&sess.id).unwrap().unwrap().input_tokens, 10);
}

#[test]
fn events_since_cursor_and_thread_events_order() {
    let s = store();
    let owner = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    s.update_thread(&owner, &t.id, Some("t2"), None).unwrap();
    s.update_thread(&owner, &t.id, Some("t3"), None).unwrap();
    let all = s.events_since(0, 100).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "seq ascending");
    let rest = s.events_since(all[0].0, 100).unwrap();
    assert_eq!(rest.len(), 2);
    assert_eq!(s.events_since(0, 1).unwrap().len(), 1);
    let te = s.thread_events(&t.id).unwrap();
    assert_eq!(te.len(), 3);
    assert!(
        te.windows(2)
            .all(|w| (w[0].created_at, &w[0].id) < (w[1].created_at, &w[1].id))
    );
    assert_eq!(
        s.get_thread(&t.id).unwrap().unwrap().title,
        "t3",
        "later local write wins even within a second"
    );
    assert_eq!(
        s.get_event(&t.id).unwrap().unwrap().kind,
        kind::THREAD_CREATE
    );
}

// --- authorization ------------------------------------------------------------

#[test]
fn non_participant_is_rejected_until_added_then_revived() {
    let s = store();
    let owner = Keypair::generate();
    let stranger = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();

    let n = note(&stranger, "let me in", &[&pk(&owner)], Some(&t.id));
    let a = s.append(&n).unwrap();
    assert!(a.inserted && !a.projected);
    assert!(
        a.rejected_reason
            .as_deref()
            .unwrap()
            .contains("not a participant")
    );
    assert!(s.thread_messages(&t.id).unwrap().is_empty());
    assert_eq!(
        s.inbox(&pk(&owner), 10).unwrap().len(),
        0,
        "rejected notes stay out of the inbox"
    );
    assert_eq!(
        s.events_since(0, 10).unwrap().len(),
        2,
        "but are kept in the log"
    );
    assert!(matches!(s.post(&n), Err(Error::Unauthorized(_))));

    // A member may not add participants.
    let member = Keypair::generate();
    s.add_participant(&owner, &t.id, &pk(&member), ParticipantRole::Member)
        .unwrap();
    assert!(matches!(
        s.add_participant(&member, &t.id, &pk(&stranger), ParticipantRole::Member),
        Err(Error::Unauthorized(_))
    ));

    s.add_participant(&owner, &t.id, &pk(&stranger), ParticipantRole::Member)
        .unwrap();
    s.rebuild_projections().unwrap();
    assert_eq!(
        ids(&s.thread_messages(&t.id).unwrap()),
        vec![n.id.clone()],
        "revived after the add"
    );
    assert_eq!(s.inbox(&pk(&owner), 10).unwrap().len(), 1);
    assert!(s.rejected_reason(&n.id).unwrap().is_none());
    let roles: Vec<(String, ParticipantRole)> = s
        .participants(&t.id)
        .unwrap()
        .into_iter()
        .map(|p| (p.pubkey, p.role))
        .collect();
    assert_eq!(roles[0], (pk(&owner), ParticipantRole::Owner));
    assert_eq!(roles.len(), 3);

    // Self-removal is allowed; the member's note then stops being projected.
    let mn = note(&member, "bye", &[], Some(&t.id));
    s.post(&mn).unwrap();
    s.remove_participant(&member, &t.id, &pk(&member)).unwrap();
    assert_eq!(s.participants(&t.id).unwrap().len(), 2);
    assert!(
        s.rejected_reason(&mn.id).unwrap().is_some(),
        "current participant set decides"
    );
}

#[test]
fn own_agent_status_is_accepted_locally_and_attested_agents_everywhere() {
    let a = store();
    let b = store();
    let owner = Keypair::generate();
    let unattested = Keypair::generate();
    let attested = Keypair::generate();
    a.insert_agent(&agent_spec("plain", &unattested, None))
        .unwrap();
    a.insert_agent(&agent_spec("vouched", &attested, Some(&owner)))
        .unwrap();

    let t = a.create_thread(&owner, "t", "i", None).unwrap();
    // Neither agent is a participant.  The unattested one may still run and
    // report on this replica (own-agent rule) ...
    let s1 = a
        .start_session(&unattested, &new_session(&t.id, &unattested, "mac"))
        .unwrap();
    a.record_status(
        &unattested,
        &s1.id,
        &StatusReport {
            state: StatusState::Working,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        a.status(&s1.id).unwrap().unwrap().state,
        StatusState::Working
    );
    // ... but not post notes.
    assert!(matches!(
        a.post(&note(&unattested, "hi", &[], Some(&t.id))),
        Err(Error::Unauthorized(_))
    ));
    // The attested agent may do both, because its owner is a participant.
    let s2 = a
        .start_session(&attested, &new_session(&t.id, &attested, "mac"))
        .unwrap();
    let report = a
        .sign_with_attestation(
            &attested,
            EnvelopeBuilder::new(kind::NOTE, "report")
                .mention(&pk(&owner))
                .thread(&t.id),
            Some(&t.id),
        )
        .unwrap();
    assert!(report.tags.iter().any(|t| t[0] == "auth"));
    a.post(&report).unwrap();
    // Without the attestation the same note is accepted here only through the local registry.
    a.post(&note(&attested, "local only", &[], Some(&t.id)))
        .unwrap();

    // Replicate to B, which knows no agents at all.
    for (_, e) in a.events_since(0, 100).unwrap() {
        b.append(&e).unwrap();
    }
    assert!(
        b.get_session(&s1.id).unwrap().is_none(),
        "unattested own-agent events mean nothing elsewhere"
    );
    assert_eq!(
        b.get_session(&s2.id).unwrap().unwrap().agent_pubkey,
        pk(&attested),
        "auth tag travels in the envelope"
    );
    assert_eq!(
        ids(&b.thread_messages(&t.id).unwrap()),
        vec![report.id.clone()],
        "only the attested note crosses"
    );
    assert_eq!(b.inbox(&pk(&owner), 10).unwrap().len(), 1);

    // Registering the agent on B (with the attestation) revives its events on rebuild.
    b.insert_agent(&agent_spec("plain", &unattested, None))
        .unwrap();
    b.rebuild_projections().unwrap();
    assert_eq!(
        b.status(&s1.id).unwrap().unwrap().state,
        StatusState::Working
    );
    b.insert_agent(&agent_spec("vouched", &attested, Some(&owner)))
        .unwrap();
    b.rebuild_projections().unwrap();
    assert_eq!(
        b.thread_messages(&t.id).unwrap().len(),
        2,
        "registry attestation revives the bare note too"
    );
}

#[test]
fn events_before_root_arrives_are_held_then_projected() {
    let a = store();
    let b = store();
    let owner = Keypair::generate();
    let t = a.create_thread(&owner, "root", "i", None).unwrap();
    a.update_thread(&owner, &t.id, Some("renamed"), None)
        .unwrap();
    let events: Vec<Envelope> = a
        .events_since(0, 10)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    let update = events
        .iter()
        .find(|e| e.kind == kind::THREAD_UPDATE)
        .unwrap();
    let r = b.append(update).unwrap();
    assert!(!r.projected);
    assert!(r.rejected_reason.unwrap().contains("root"));
    assert!(b.get_thread(&t.id).unwrap().is_none());
    b.append(&events[0]).unwrap();
    assert_eq!(
        b.get_thread(&t.id).unwrap().unwrap().title,
        "renamed",
        "held event applied once the root arrived"
    );
}

// --- multiplayer --------------------------------------------------------------

/// A realistic thread history on `a`, returned as the envelopes to replicate.
fn build_history(a: &Store) -> (String, Vec<Envelope>, Keypair, Keypair) {
    let alice = Keypair::generate();
    let bob = Keypair::generate();
    let atlas = Keypair::generate();
    let nova = Keypair::generate();
    a.insert_agent(&agent_spec("atlas", &atlas, Some(&alice)))
        .unwrap();

    let t = a
        .create_thread(&alice, "Ship v2", "Release the thing", None)
        .unwrap();
    a.update_thread(&alice, &t.id, Some("Ship v2.0"), None)
        .unwrap();
    a.attach_project(
        &alice,
        &t.id,
        Some("github.com/acme/axel"),
        Path::new("/w/axel"),
        &ws(WorkspaceKind::Worktree, Some("axel/ship")),
    )
    .unwrap();
    a.attach_project(
        &alice,
        &t.id,
        None,
        Path::new("/w/docs"),
        &ws(WorkspaceKind::InPlace, None),
    )
    .unwrap();
    a.detach_project(&alice, &t.id, "/w/docs").unwrap();
    a.add_participant(&alice, &t.id, &pk(&bob), ParticipantRole::Member)
        .unwrap();
    a.add_participant(&alice, &t.id, &pk(&nova), ParticipantRole::Agent)
        .unwrap();
    a.post(&note(&bob, "on it", &[&pk(&alice)], Some(&t.id)))
        .unwrap();

    let s1 = a
        .start_session(&atlas, &new_session(&t.id, &atlas, "mac"))
        .unwrap();
    let s2 = a
        .start_session(&nova, &new_session(&t.id, &nova, "studio"))
        .unwrap();
    a.update_session(
        &atlas,
        &s1.id,
        &SessionUpdate {
            state: Some(SessionState::Running),
            harness_session_id: Some("claude-abc".into()),
            transcript_path: Some("/t/abc.jsonl".into()),
            rex: Some((
                Some("session:1".into()),
                Some("window:2".into()),
                Some("block:3".into()),
            )),
            runtime_mode: Some(RuntimeMode::Plan),
        },
    )
    .unwrap();
    // A mid-session mode switch is projected onto the session row.
    assert_eq!(a.get_session(&s1.id).unwrap().unwrap().runtime_mode, RuntimeMode::Plan);
    a.record_status(
        &atlas,
        &s1.id,
        &StatusReport {
            state: StatusState::Working,
            msg: Some("building".into()),
            progress: Some(30),
            ..Default::default()
        },
    )
    .unwrap();
    a.record_status(
        &nova,
        &s2.id,
        &StatusReport {
            state: StatusState::Blocked,
            kind: Some(BlockedKind::Permission),
            msg: Some("rm -rf?".into()),
            ..Default::default()
        },
    )
    .unwrap();
    a.add_usage(
        &atlas,
        &s1.id,
        &UsageDelta {
            input_tokens: 100,
            output_tokens: 20,
            turns: 1,
            cost_usd_est: 0.5,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    a.add_usage(
        &atlas,
        &s1.id,
        &UsageDelta {
            input_tokens: 100,
            output_tokens: 20,
            turns: 1,
            cost_usd_est: 0.5,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    a.add_usage(
        &nova,
        &s2.id,
        &UsageDelta {
            input_tokens: 7,
            output_tokens: 3,
            turns: 1,
            cost_usd_est: 0.1,
            cache_read_tokens: 4,
            cache_write_tokens: 1,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    a.record_status(
        &atlas,
        &s1.id,
        &StatusReport {
            state: StatusState::Done,
            ..Default::default()
        },
    )
    .unwrap();
    a.end_session(&atlas, &s1.id, Some("completed")).unwrap();

    let req = EnvelopeBuilder::new(kind::PERMISSION_REQUEST, "may I push?")
        .thread(&t.id)
        .tag(vec!["s".into(), s2.id.clone()])
        .mention(&pk(&alice))
        .sign(&nova)
        .unwrap();
    a.post(&req).unwrap();
    let reply = EnvelopeBuilder::new(kind::PERMISSION_REPLY, "allow")
        .thread(&t.id)
        .reply_to(&req.id, &req.id)
        .mention(&pk(&nova))
        .sign(&alice)
        .unwrap();
    a.post(&reply).unwrap();
    a.update_thread(&alice, &t.id, None, Some(ThreadState::Done))
        .unwrap();

    let events = a
        .events_since(0, 1000)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    (t.id, events, alice, nova)
}

#[test]
fn replicas_converge_regardless_of_arrival_order() {
    let a = store();
    let (thread_id, events, alice, nova) = build_history(&a);
    assert!(events.len() >= 18, "{}", events.len());
    let expected = a.thread_summary(&thread_id).unwrap().unwrap();

    // Sanity on the content itself.
    assert_eq!(expected.thread.title, "Ship v2.0");
    assert_eq!(expected.thread.state, ThreadState::Done);
    assert!(expected.thread.done_at.is_some());
    assert_eq!(expected.projects.len(), 1);
    assert_eq!(
        expected.projects[0].repo_key.as_deref(),
        Some("github.com/acme/axel")
    );
    assert_eq!(
        expected.projects[0].workspace.branch.as_deref(),
        Some("axel/ship")
    );
    assert_eq!(expected.participants.len(), 3);
    assert_eq!(expected.sessions_total, 2);
    assert_eq!(expected.sessions_live.len(), 1);
    assert_eq!(expected.sessions_live[0].agent_pubkey, pk(&nova));
    assert_eq!(expected.sessions_live[0].state, SessionState::Blocked);
    assert_eq!(expected.agents.len(), 2);
    assert_eq!(
        expected.environments,
        vec!["mac".to_string(), "studio".to_string()]
    );
    assert_eq!(
        (
            expected.usage.input_tokens,
            expected.usage.output_tokens,
            expected.usage.turns
        ),
        (207, 43, 3)
    );
    assert!((expected.usage.cost_usd_est - 1.1).abs() < 1e-9);
    let live = expected.live.as_ref().unwrap();
    assert_eq!(
        (live.state, live.kind),
        (StatusState::Blocked, Some(BlockedKind::Permission))
    );
    assert_eq!(
        a.pending_requests().unwrap().len(),
        0,
        "the request was answered"
    );
    assert_eq!(a.thread_messages(&thread_id).unwrap().len(), 3);

    for seed in [0x9E3779B97F4A7C15u64, 42, 7_777_777] {
        let mut shuffled = events.clone();
        shuffle(&mut shuffled, seed);
        let b = store();
        for e in &shuffled {
            b.append(e).unwrap();
        }
        assert_eq!(
            b.thread_summary(&thread_id).unwrap().unwrap(),
            expected,
            "seed {seed}"
        );
        assert_eq!(
            ids(&b.thread_messages(&thread_id).unwrap()),
            ids(&a.thread_messages(&thread_id).unwrap())
        );
        assert_eq!(
            ids(&b.inbox(&pk(&alice), 50).unwrap()),
            ids(&a.inbox(&pk(&alice), 50).unwrap())
        );
        assert_eq!(b.pending_requests().unwrap().len(), 0);
        assert_eq!(b.statuses().unwrap(), a.statuses().unwrap());
        assert_eq!(b.usages().unwrap(), a.usages().unwrap());
        assert_eq!(
            b.thread_events(&thread_id).unwrap(),
            a.thread_events(&thread_id).unwrap()
        );

        b.rebuild_projections().unwrap();
        assert_eq!(
            b.thread_summary(&thread_id).unwrap().unwrap(),
            expected,
            "seed {seed} after rebuild"
        );
    }

    // Reverse order, and a rebuild of the origin itself.
    let c = store();
    for e in events.iter().rev() {
        c.append(e).unwrap();
    }
    assert_eq!(c.thread_summary(&thread_id).unwrap().unwrap(), expected);
    a.rebuild_projections().unwrap();
    assert_eq!(a.thread_summary(&thread_id).unwrap().unwrap(), expected);
    assert_eq!(
        a.events_since(0, 1000).unwrap().len(),
        events.len(),
        "rebuild never touches the log"
    );
}

// --- sessions -----------------------------------------------------------------

#[test]
fn session_lifecycle() {
    let s = store();
    let owner = Keypair::generate();
    let atlas = Keypair::generate();
    s.insert_agent(&agent_spec("atlas", &atlas, Some(&owner)))
        .unwrap();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();

    let sess = s
        .start_session(&atlas, &new_session(&t.id, &atlas, "mac"))
        .unwrap();
    assert_eq!(sess.id.len(), 26);
    assert_eq!(sess.state, SessionState::Starting);
    assert_eq!(sess.thread_id, t.id);
    assert_eq!(sess.environment_label, "mac");
    assert_eq!(sess.runtime_mode, RuntimeMode::Autopilot);
    assert_eq!(
        s.latest_session_for_agent(&pk(&atlas)).unwrap().unwrap().id,
        sess.id
    );

    s.update_session(
        &atlas,
        &sess.id,
        &SessionUpdate {
            state: Some(SessionState::Running),
            harness_session_id: Some("claude-1".into()),
            transcript_path: Some("/t/1.jsonl".into()),
            rex: Some((
                Some("session:a".into()),
                Some("window:b".into()),
                Some("block:c".into()),
            )),
            runtime_mode: None,
        },
    )
    .unwrap();
    let got = s.get_session(&sess.id).unwrap().unwrap();
    assert_eq!(got.state, SessionState::Running);
    assert_eq!(got.harness_session_id.as_deref(), Some("claude-1"));
    assert_eq!(got.transcript_path.as_deref(), Some("/t/1.jsonl"));
    assert_eq!(got.rex_block_id.as_deref(), Some("block:c"));
    s.update_session(&atlas, &sess.id, &SessionUpdate::default())
        .unwrap();
    assert_eq!(
        s.get_session(&sess.id)
            .unwrap()
            .unwrap()
            .harness_session_id
            .as_deref(),
        Some("claude-1"),
        "None keeps"
    );

    s.record_status(
        &atlas,
        &sess.id,
        &StatusReport {
            state: StatusState::Blocked,
            kind: Some(BlockedKind::Question),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().state,
        SessionState::Blocked,
        "status drives live state"
    );
    assert_eq!(s.statuses().unwrap().len(), 1);
    assert_eq!(
        s.thread_summary(&t.id).unwrap().unwrap().live.unwrap().kind,
        Some(BlockedKind::Question)
    );

    // Local-only transcript offset survives replays and rebuilds.
    s.add_usage(
        &atlas,
        &sess.id,
        &UsageDelta {
            input_tokens: 5,
            ..Default::default()
        },
        Some(4096),
    )
    .unwrap();
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().transcript_offset,
        4096
    );
    s.record_status(
        &atlas,
        &sess.id,
        &StatusReport {
            state: StatusState::Working,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().transcript_offset,
        4096
    );
    s.rebuild_projections().unwrap();
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().transcript_offset,
        4096
    );
    assert!(
        s.thread_events(&t.id)
            .unwrap()
            .iter()
            .all(|e| !e.content.contains("4096")),
        "offset is not in the log"
    );

    s.end_session(&atlas, &sess.id, Some("done")).unwrap();
    let ended = s.get_session(&sess.id).unwrap().unwrap();
    assert_eq!(ended.state, SessionState::Ended);
    assert_eq!(ended.ended_reason.as_deref(), Some("done"));
    assert!(ended.ended_at.is_some());
    assert!(
        s.statuses().unwrap().is_empty(),
        "ended sessions drop out of statuses()"
    );
    assert!(
        s.status(&sess.id).unwrap().is_some(),
        "but the row is still readable"
    );
    let summary = s.thread_summary(&t.id).unwrap().unwrap();
    assert_eq!(summary.sessions_total, 1);
    assert!(summary.sessions_live.is_empty());
    assert!(summary.live.is_none());
    s.record_status(
        &atlas,
        &sess.id,
        &StatusReport {
            state: StatusState::Working,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().state,
        SessionState::Ended,
        "status never resurrects a session"
    );

    let resumed = s
        .start_session(
            &atlas,
            &NewSession {
                resumed_from: Some(sess.id.clone()),
                ..new_session(&t.id, &atlas, "studio")
            },
        )
        .unwrap();
    assert_eq!(resumed.resumed_from.as_deref(), Some(sess.id.as_str()));
    let filt = |f: SessionFilter| {
        s.list_sessions(f)
            .unwrap()
            .into_iter()
            .map(|x| x.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        filt(SessionFilter {
            thread_id: Some(t.id.clone()),
            ..Default::default()
        }),
        vec![resumed.id.clone(), sess.id.clone()]
    );
    assert_eq!(
        filt(SessionFilter {
            environment_id: Some("env-studio".into()),
            ..Default::default()
        }),
        vec![resumed.id.clone()]
    );
    assert_eq!(
        filt(SessionFilter {
            states: Some(vec![SessionState::Ended]),
            ..Default::default()
        }),
        vec![sess.id.clone()]
    );
    assert!(
        filt(SessionFilter {
            states: Some(vec![]),
            ..Default::default()
        })
        .is_empty()
    );
    assert!(matches!(
        s.end_session(&atlas, "nope", None),
        Err(Error::NotFound(_))
    ));
}

// --- usage --------------------------------------------------------------------

#[test]
fn usage_sums_across_sessions() {
    let s = store();
    let owner = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let other = s.create_thread(&owner, "other", "i", None).unwrap();
    let s1 = s
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    let s2 = s
        .start_session(&owner, &new_session(&t.id, &owner, "studio"))
        .unwrap();
    let s3 = s
        .start_session(&owner, &new_session(&other.id, &owner, "mac"))
        .unwrap();

    let d = |i: i64, o: i64| UsageDelta {
        input_tokens: i,
        output_tokens: o,
        turns: 1,
        cost_usd_est: 0.25,
        ..Default::default()
    };
    let u1 = s.add_usage(&owner, &s1.id, &d(100, 10), None).unwrap();
    assert_eq!((u1.input_tokens, u1.turns), (100, 1));
    let u1 = s.add_usage(&owner, &s1.id, &d(100, 10), None).unwrap();
    assert_eq!(
        (u1.input_tokens, u1.output_tokens, u1.turns),
        (200, 20, 2),
        "identical deltas both count"
    );
    s.add_usage(&owner, &s2.id, &d(50, 5), None).unwrap();
    s.add_usage(&owner, &s3.id, &d(1000, 1), None).unwrap();

    let total = s.thread_usage(&t.id).unwrap();
    assert_eq!(total.session_id, "");
    assert_eq!(
        (total.input_tokens, total.output_tokens, total.turns),
        (250, 25, 3)
    );
    assert!((total.cost_usd_est - 0.75).abs() < 1e-9);
    assert_eq!(s.thread_usage(&other.id).unwrap().input_tokens, 1000);
    assert_eq!(s.thread_usage("nope").unwrap(), Usage::default());
    assert_eq!(s.usages().unwrap().len(), 3);
    assert_eq!(s.usage(&s2.id).unwrap().unwrap().input_tokens, 50);

    assert_eq!(s.usage_samples(Some(&s1.id), 0).unwrap().len(), 2);
    assert_eq!(s.usage_samples(None, 0).unwrap().len(), 4);
    assert_eq!(s.thread_usage_samples(&t.id, 0).unwrap().len(), 3);
    assert!(s.usage_samples(None, i64::MAX).unwrap().is_empty());

    // Samples older than 24h are not materialised; totals are unaffected.
    let old = EnvelopeBuilder::new(kind::USAGE, r#"{"input": 9, "output": 9}"#)
        .thread(&t.id)
        .tag(vec!["s".into(), s1.id.clone()])
        .created_at(1_000_000)
        .sign(&owner)
        .unwrap();
    assert!(s.append(&old).unwrap().projected);
    assert_eq!(s.usage(&s1.id).unwrap().unwrap().input_tokens, 209);
    assert_eq!(s.usage_samples(Some(&s1.id), 0).unwrap().len(), 2);
    assert_eq!(
        s.thread_summary(&t.id).unwrap().unwrap().usage.input_tokens,
        259
    );
}

// --- inbox over events --------------------------------------------------------

#[test]
fn inbox_unread_mark_read_pending_and_replies() {
    let s = store();
    let owner = Keypair::generate();
    let atlas = Keypair::generate();
    let me = pk(&owner);
    for (i, ts) in [10u64, 20, 20, 30].into_iter().enumerate() {
        let e = EnvelopeBuilder::new(kind::NOTE, format!("n{i}"))
            .mention(&me)
            .created_at(ts)
            .sign(&atlas)
            .unwrap();
        s.post(&e).unwrap();
    }
    s.post(
        &EnvelopeBuilder::new(kind::NOTE, "other")
            .mention(&pk(&atlas))
            .created_at(99)
            .sign(&owner)
            .unwrap(),
    )
    .unwrap();

    let inbox = s.inbox(&me, 10).unwrap();
    assert_eq!(inbox.len(), 4);
    assert!(
        inbox
            .windows(2)
            .all(|w| (w[0].created_at, &w[0].id) > (w[1].created_at, &w[1].id)),
        "newest first"
    );
    assert_eq!(s.inbox(&me, 2).unwrap().len(), 2);
    assert_eq!(s.unread_count(&me).unwrap(), 4);
    let b = inbox
        .iter()
        .filter(|e| e.created_at == 20)
        .min_by(|x, y| x.id.cmp(&y.id))
        .unwrap();
    s.mark_read(&me, 20, &b.id).unwrap();
    assert_eq!(
        s.unread_count(&me).unwrap(),
        2,
        "same ts greater id + ts 30"
    );
    s.mark_read(&me, 10, "x").unwrap();
    assert_eq!(s.unread_count(&me).unwrap(), 2, "frontier never moves back");
    s.mark_read(&me, 30, &inbox[0].id).unwrap();
    assert_eq!(s.unread_count(&me).unwrap(), 0);
    assert_eq!(s.unread_count(&pk(&atlas)).unwrap(), 1);

    // Requests live on a thread; the agent is a participant.
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    s.add_participant(&owner, &t.id, &pk(&atlas), ParticipantRole::Agent)
        .unwrap();
    let req = EnvelopeBuilder::new(kind::PERMISSION_REQUEST, "rm?")
        .thread(&t.id)
        .mention(&me)
        .sign(&atlas)
        .unwrap();
    let q = EnvelopeBuilder::new(kind::QUESTION, "which branch?")
        .thread(&t.id)
        .mention(&me)
        .sign(&atlas)
        .unwrap();
    s.post(&req).unwrap();
    s.post(&q).unwrap();
    assert_eq!(ids(&s.pending_requests().unwrap()).len(), 2);
    assert_eq!(s.unread_count(&me).unwrap(), 2);

    // A note referencing the request does not resolve it.
    let n = EnvelopeBuilder::new(kind::NOTE, "hmm")
        .thread(&t.id)
        .reply_to(&req.id, &req.id)
        .sign(&owner)
        .unwrap();
    s.post(&n).unwrap();
    assert_eq!(s.pending_requests().unwrap().len(), 2);
    let reply = EnvelopeBuilder::new(kind::PERMISSION_REPLY, "deny")
        .thread(&t.id)
        .reply_to(&req.id, &req.id)
        .mention(&pk(&atlas))
        .sign(&owner)
        .unwrap();
    s.post(&reply).unwrap();
    assert_eq!(ids(&s.pending_requests().unwrap()), vec![q.id.clone()]);
    let answer = EnvelopeBuilder::new(kind::ANSWER, "main")
        .thread(&t.id)
        .reply_to(&q.id, &q.id)
        .sign(&owner)
        .unwrap();
    s.post(&answer).unwrap();
    assert!(s.pending_requests().unwrap().is_empty());
    assert_eq!(ids(&s.replies_to(&req.id).unwrap()).len(), 2);
    assert!(s.replies_to("nothing").unwrap().is_empty());
    assert_eq!(
        s.thread_messages(&t.id).unwrap().len(),
        5,
        "requests, note, reply, answer; no control events"
    );

    // A reply from a non-participant does not resolve anything.
    let stranger = Keypair::generate();
    let q2 = EnvelopeBuilder::new(kind::QUESTION, "?")
        .thread(&t.id)
        .sign(&atlas)
        .unwrap();
    s.post(&q2).unwrap();
    let fake = EnvelopeBuilder::new(kind::ANSWER, "42")
        .thread(&t.id)
        .reply_to(&q2.id, &q2.id)
        .sign(&stranger)
        .unwrap();
    assert!(!s.append(&fake).unwrap().projected);
    assert_eq!(ids(&s.pending_requests().unwrap()), vec![q2.id.clone()]);
}

// --- threads ------------------------------------------------------------------

#[test]
fn thread_filters_and_state_transitions() {
    let s = store();
    let alice = Keypair::generate();
    let bob = Keypair::generate();
    let atlas = Keypair::generate();
    let t1 = s.create_thread(&alice, "one", "i1", None).unwrap();
    let t2 = s
        .create_thread(&alice, "two", "i2", Some((&t1.id, Relationship::Fork)))
        .unwrap();
    let t3 = s.create_thread(&bob, "three", "i3", None).unwrap();
    assert_eq!(t2.parent_thread_id.as_deref(), Some(t1.id.as_str()));
    assert_eq!(t2.relationship, Some(Relationship::Fork));
    assert_eq!(t1.created_by, pk(&alice));
    assert_eq!(t1.state, ThreadState::Open);

    s.add_participant(&alice, &t1.id, &pk(&bob), ParticipantRole::Member)
        .unwrap();
    s.attach_project(
        &alice,
        &t2.id,
        Some("gh/acme/x"),
        Path::new("/x"),
        &ws(WorkspaceKind::Scratch, None),
    )
    .unwrap();
    s.start_session(&alice, &new_session(&t1.id, &atlas, "mac"))
        .unwrap();
    s.update_thread(&alice, &t2.id, None, Some(ThreadState::Done))
        .unwrap();
    s.update_thread(&bob, &t3.id, None, Some(ThreadState::Archived))
        .unwrap();

    let l = |f: ThreadFilter| {
        s.list_threads(f)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(l(ThreadFilter::default()).len(), 3);
    assert_eq!(
        l(ThreadFilter {
            states: Some(vec![ThreadState::Open]),
            ..Default::default()
        }),
        vec![t1.id.clone()]
    );
    assert_eq!(
        l(ThreadFilter {
            states: Some(vec![ThreadState::Done, ThreadState::Archived]),
            ..Default::default()
        })
        .len(),
        2
    );
    assert!(
        l(ThreadFilter {
            states: Some(vec![]),
            ..Default::default()
        })
        .is_empty()
    );
    assert_eq!(
        l(ThreadFilter {
            agent_pubkey: Some(pk(&atlas)),
            ..Default::default()
        }),
        vec![t1.id.clone()]
    );
    let bobs = l(ThreadFilter {
        participant: Some(pk(&bob)),
        ..Default::default()
    });
    assert_eq!(bobs.len(), 2);
    assert!(bobs.contains(&t1.id) && bobs.contains(&t3.id));
    assert_eq!(
        l(ThreadFilter {
            repo_key: Some("gh/acme/x".into()),
            ..Default::default()
        }),
        vec![t2.id.clone()]
    );
    assert_eq!(
        l(ThreadFilter {
            limit: Some(1),
            ..Default::default()
        })
        .len(),
        1
    );
    assert_eq!(
        s.thread_summaries(ThreadFilter::default()).unwrap().len(),
        3
    );

    let done = s.get_thread(&t2.id).unwrap().unwrap();
    assert!(done.done_at.is_some());
    s.update_thread(&alice, &t2.id, None, Some(ThreadState::Archived))
        .unwrap();
    assert_eq!(
        s.get_thread(&t2.id).unwrap().unwrap().done_at,
        done.done_at,
        "archiving keeps done_at"
    );
    s.update_thread(&alice, &t2.id, None, Some(ThreadState::Open))
        .unwrap();
    assert!(
        s.get_thread(&t2.id).unwrap().unwrap().done_at.is_none(),
        "reopening clears it"
    );
    assert!(s.get_thread(&t2.id).unwrap().unwrap().updated_at >= done.updated_at);

    // Project resolution against the local registry.
    let env = s.local_environment().unwrap();
    let local = s
        .upsert_project(&env.id, Path::new("/elsewhere/x"), Some("gh/acme/x"))
        .unwrap();
    let tp = s.thread_projects(&t2.id).unwrap();
    assert_eq!(tp.len(), 1);
    assert_eq!(
        tp[0].local_project_id.as_deref(),
        Some(local.id.as_str()),
        "resolved by repo_key"
    );
    assert_eq!(tp[0].workspace.kind, WorkspaceKind::Scratch);
    s.detach_project(&alice, &t2.id, "gh/acme/x").unwrap();
    assert!(s.thread_projects(&t2.id).unwrap().is_empty());

    assert!(matches!(
        s.update_thread(&alice, "nope", Some("x"), None),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.update_thread(&bob, &t2.id, Some("hijack"), None),
        Err(Error::Unauthorized(_))
    ));
    assert_eq!(s.get_thread(&t2.id).unwrap().unwrap().title, "two");
}

#[test]
fn enum_strings_roundtrip() {
    for st in ThreadState::ALL {
        assert_eq!(st.as_str().parse::<ThreadState>().unwrap(), *st);
        assert_eq!(
            serde_json::to_string(st).unwrap(),
            format!("\"{}\"", st.as_str())
        );
    }
    for st in SessionState::ALL {
        assert_eq!(st.as_str().parse::<SessionState>().unwrap(), *st);
    }
    for wk in WorkspaceKind::ALL {
        assert_eq!(wk.as_str().parse::<WorkspaceKind>().unwrap(), *wk);
        assert_eq!(
            serde_json::to_string(wk).unwrap(),
            format!("\"{}\"", wk.as_str())
        );
    }
    for r in ParticipantRole::ALL {
        assert_eq!(r.as_str().parse::<ParticipantRole>().unwrap(), *r);
    }
    for m in RuntimeMode::ALL {
        assert_eq!(m.as_str().parse::<RuntimeMode>().unwrap(), *m);
        assert_eq!(
            serde_json::to_string(m).unwrap(),
            format!("\"{}\"", m.as_str())
        );
    }
    for st in StatusState::ALL {
        assert_eq!(st.as_str().parse::<StatusState>().unwrap(), *st);
    }
    for k in BlockedKind::ALL {
        assert_eq!(k.as_str().parse::<BlockedKind>().unwrap(), *k);
    }
    for r in Relationship::ALL {
        assert_eq!(r.as_str().parse::<Relationship>().unwrap(), *r);
    }
    for c in AgentColor::ALL {
        assert_eq!(c.as_str().parse::<AgentColor>().unwrap(), *c);
    }
    assert_eq!(RuntimeMode::default(), RuntimeMode::Autopilot);
    assert_eq!(StatusState::default(), StatusState::Idle);
    assert_eq!(
        format!("{:<8}|", ThreadState::Open),
        "open    |",
        "Display honours padding"
    );
    assert!("bogus".parse::<ThreadState>().is_err());
    assert!("bogus".parse::<RuntimeMode>().is_err());
    assert!(kind::is_thread_kind(kind::USAGE) && kind::is_thread_kind(kind::SESSION_END));
    assert!(!kind::is_thread_kind(kind::NOTE));
    assert!(kind::is_own_agent_kind(kind::STATUS) && !kind::is_own_agent_kind(kind::THREAD_UPDATE));
}

#[test]
fn harness_and_model_are_one_argument() {
    use crate::parse_harness_model as p;
    assert_eq!(p("claude"), (Harness::Claude, None));
    assert_eq!(p("claude/opus"), (Harness::Claude, Some("opus".into())));
    assert_eq!(p(" Codex/gpt-5.5 "), (Harness::Codex, Some("gpt-5.5".into())));
    assert_eq!(p("opencode/anthropic/claude-opus"), (Harness::Opencode, Some("anthropic/claude-opus".into())), "the model keeps its own slashes");
    assert_eq!(p("claude/"), (Harness::Claude, None), "an empty model is the default");
    assert_eq!(p("cursor-agent/sonnet"), (Harness::Cursor, Some("sonnet".into())), "a CLI's own name works");
    assert_eq!(p("/usr/local/bin/agent"), (Harness::Custom("/usr/local/bin/agent".into()), None), "a path is a command");
    assert_eq!(p("./my-agent/x"), (Harness::Custom("./my-agent/x".into()), None));
}

#[test]
fn every_builtin_harness_says_how_to_run_and_install_it() {
    for h in &Harness::BUILTIN {
        assert!(!h.label().is_empty() && !h.program().is_empty() && !h.install().is_empty(), "{h}");
        match h {
            Harness::Claude | Harness::Codex => assert!(h.acp_command().is_none(), "{h}: adapter built into axel"),
            _ => assert!(h.acp_command().is_some(), "{h} has an ACP command"),
        }
    }
    assert_eq!(Harness::parse("goose"), Harness::Goose, "an agent stored as the custom command goose is now the builtin");
    assert_eq!(Harness::Cursor.program(), "cursor-agent");
    assert_eq!(Harness::Custom("python agent.py".into()).program(), "python");
}

#[test]
fn harness_is_text_on_the_wire() {
    for h in &Harness::BUILTIN {
        assert_eq!(&Harness::parse(h.as_str()), h);
        assert!(h.is_builtin());
        assert_eq!(
            serde_json::to_string(h).unwrap(),
            format!("\"{}\"", h.as_str())
        );
    }
    assert_eq!(Harness::parse(" Claude "), Harness::Claude);
    let custom = Harness::parse("/usr/bin/true");
    assert_eq!(custom, Harness::Custom("/usr/bin/true".into()));
    assert!(!custom.is_builtin());
    assert_eq!(custom.as_str(), "/usr/bin/true");
    assert_eq!(serde_json::to_string(&custom).unwrap(), "\"/usr/bin/true\"");
    assert_eq!(
        serde_json::from_str::<Harness>("\"codex\"").unwrap(),
        Harness::Codex
    );
    assert_eq!(format!("{:<9}|", Harness::Codex), "codex    |");
    assert_eq!(Harness::default(), Harness::Claude);

    // A custom harness survives the store round trip as its command.
    let s = store();
    let kp = Keypair::generate();
    let mut spec = agent_spec("echo", &kp, None);
    spec.harness = custom.clone();
    spec.color = None;
    let a = s.insert_agent(&spec).unwrap();
    assert_eq!(a.harness, custom);
    assert_eq!(s.agent_by_name("echo").unwrap().unwrap().harness, custom);
}

#[test]
fn unknown_enum_text_reads_leniently() {
    // Wire: a status with a state and kind this build does not know still parses.
    let r: StatusReport =
        serde_json::from_str(r#"{"state":"meditating","kind":"vibes","msg":"hm"}"#).unwrap();
    assert_eq!(r.state, StatusState::Idle);
    assert_eq!(r.kind, None);
    assert_eq!(r.msg.as_deref(), Some("hm"));
    let r: StatusReport = serde_json::from_str(r#"{"state":"blocked","kind":"auth"}"#).unwrap();
    assert_eq!(
        (r.state, r.kind),
        (StatusState::Blocked, Some(BlockedKind::Auth))
    );
    // Serialisation keeps the snake_case strings older logs were written with.
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"state":"blocked","kind":"auth"}"#
    );
    let ss: wire::SessionStart =
        serde_json::from_str(r#"{"harness":"claude","runtime_mode":""}"#).unwrap();
    assert_eq!(
        ss.runtime_mode,
        RuntimeMode::Autopilot,
        "empty mode is the default"
    );
    let ss: wire::SessionStart =
        serde_json::from_str(r#"{"harness":"/bin/x","runtime_mode":"yolo"}"#).unwrap();
    assert_eq!(ss.harness, Harness::Custom("/bin/x".into()));
    assert_eq!(ss.runtime_mode, RuntimeMode::Autopilot);

    // DB: rows written by another build read with defaults instead of failing.
    let s = store();
    let owner = Keypair::generate();
    let atlas = Keypair::generate();
    s.insert_agent(&agent_spec("atlas", &atlas, Some(&owner)))
        .unwrap();
    s.connection()
        .execute(
            "UPDATE agents SET runtime_mode = 'yolo', color = 'teal', harness = 'zed'",
            [],
        )
        .unwrap();
    let a = s.agent_by_name("atlas").unwrap().unwrap();
    assert_eq!(a.runtime_mode, RuntimeMode::Autopilot);
    assert_eq!(a.color, None);
    assert_eq!(a.harness, Harness::Custom("zed".into()));

    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let sess = s
        .start_session(&atlas, &new_session(&t.id, &atlas, "mac"))
        .unwrap();
    s.record_status(
        &atlas,
        &sess.id,
        &StatusReport {
            state: StatusState::Working,
            ..Default::default()
        },
    )
    .unwrap();
    s.connection()
        .execute_batch(
            "UPDATE status SET state = 'meditating', kind = 'vibes'; UPDATE threads SET relationship = 'cousin'; UPDATE sessions SET runtime_mode = 'yolo';",
        )
        .unwrap();
    let st = s.status(&sess.id).unwrap().unwrap();
    assert_eq!((st.state, st.kind), (StatusState::Idle, None));
    assert_eq!(s.get_thread(&t.id).unwrap().unwrap().relationship, None);
    assert_eq!(
        s.get_session(&sess.id).unwrap().unwrap().runtime_mode,
        RuntimeMode::Autopilot
    );
}

// --- managed runtime: conversation items (local-only) -------------------------

#[test]
fn items_seq_per_session_patch_pending_and_thread_view() {
    let s = store();
    let owner = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let other_thread = s.create_thread(&owner, "o", "i", None).unwrap();
    let s1 = s
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    let s2 = s
        .start_session(&owner, &new_session(&t.id, &owner, "studio"))
        .unwrap();
    let s3 = s
        .start_session(&owner, &new_session(&other_thread.id, &owner, "mac"))
        .unwrap();

    let a1 = s
        .push_item(&item(
            &s1,
            1,
            ItemKind::UserMessage,
            None,
            None,
            serde_json::json!({"text": "hi"}),
        ))
        .unwrap();
    let a2 = s
        .push_item(&item(
            &s1,
            1,
            ItemKind::ToolCall,
            Some(ItemStatus::Running),
            Some("call-1"),
            serde_json::json!({"title": "ls", "kind": "read"}),
        ))
        .unwrap();
    let b1 = s
        .push_item(&item(
            &s2,
            1,
            ItemKind::Permission,
            Some(ItemStatus::Pending),
            Some("perm-1"),
            serde_json::json!({"tool": "rm"}),
        ))
        .unwrap();
    let a3 = s
        .push_item(&item(
            &s1,
            1,
            ItemKind::Question,
            Some(ItemStatus::Pending),
            Some("q-1"),
            serde_json::json!({"q": "which?"}),
        ))
        .unwrap();
    let c1 = s
        .push_item(&item(
            &s3,
            1,
            ItemKind::Permission,
            Some(ItemStatus::Pending),
            Some("perm-9"),
            serde_json::json!({}),
        ))
        .unwrap();
    assert_eq!(
        (a1.seq, a2.seq, a3.seq),
        (1, 2, 3),
        "seq counts per session"
    );
    assert_eq!((b1.seq, c1.seq), (1, 1));
    assert_eq!(a1.id.len(), 26);
    assert!(a1.at > 1_600_000_000_000, "unix milliseconds");
    assert_eq!(a2.thread_id, t.id);

    // Paging within a session.
    let page: Vec<i64> = s
        .items(&s1.id, 0, 2)
        .unwrap()
        .into_iter()
        .map(|i| i.seq)
        .collect();
    assert_eq!(page, vec![1, 2]);
    let rest: Vec<i64> = s
        .items(&s1.id, 2, 10)
        .unwrap()
        .into_iter()
        .map(|i| i.seq)
        .collect();
    assert_eq!(rest, vec![3]);
    assert!(s.items(&s1.id, 3, 10).unwrap().is_empty());

    // A turn's tool calls still open: only that session's, that turn's, still running.
    let open: Vec<String> = s.open_tool_calls(&s1.id, 1).unwrap().into_iter().map(|i| i.id).collect();
    assert_eq!(open, vec![a2.id.clone()]);
    assert!(s.open_tool_calls(&s1.id, 2).unwrap().is_empty(), "another turn");
    assert!(s.open_tool_calls(&s2.id, 1).unwrap().is_empty(), "a permission isn't a tool call");

    // Lookup by ACP reference and shallow merge patch.
    assert_eq!(s.item_by_ref(&s1.id, "call-1").unwrap().unwrap().id, a2.id);
    assert!(
        s.item_by_ref(&s2.id, "call-1").unwrap().is_none(),
        "refs are per session"
    );
    s.update_item(
        &a2.id,
        Some(ItemStatus::Completed),
        Some(serde_json::json!({"output": "a b", "kind": null, "title": "ls -la"})),
    )
    .unwrap();
    let got = s.item(&a2.id).unwrap().unwrap();
    assert_eq!(got.status, Some(ItemStatus::Completed));
    assert_eq!(
        got.content,
        serde_json::json!({"title": "ls -la", "output": "a b"}),
        "replace, add, null removes"
    );
    s.update_item(&a2.id, None, None).unwrap();
    assert_eq!(s.item(&a2.id).unwrap().unwrap(), got, "no-op update");
    s.update_item(&a1.id, None, Some(serde_json::json!({"text": "hi!"})))
        .unwrap();
    assert_eq!(
        s.item(&a1.id).unwrap().unwrap().status,
        None,
        "status untouched when None"
    );
    assert!(matches!(
        s.update_item("nope", Some(ItemStatus::Failed), None),
        Err(Error::NotFound(_))
    ));

    // Pending filter: Permission|Question with Pending only.
    let pending: Vec<String> = s
        .pending_items(Some(&t.id))
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(pending, vec![b1.id.clone(), a3.id.clone()]);
    assert_eq!(s.pending_items(None).unwrap().len(), 3);
    s.update_item(&b1.id, Some(ItemStatus::Allowed), None)
        .unwrap();
    assert_eq!(s.pending_items(Some(&t.id)).unwrap().len(), 1);
    s.update_item(
        &a3.id,
        Some(ItemStatus::Answered),
        Some(serde_json::json!({"answer": "main"})),
    )
    .unwrap();
    assert!(s.pending_items(Some(&t.id)).unwrap().is_empty());
    assert_eq!(
        s.pending_items(None).unwrap().len(),
        1,
        "other thread still pending"
    );

    // Thread view across sessions: (at, seq) ascending, last `limit`.
    let all = s.thread_items(&t.id, 100).unwrap();
    assert_eq!(all.len(), 4);
    assert!(
        all.windows(2)
            .all(|w| (w[0].at, w[0].seq) <= (w[1].at, w[1].seq))
    );
    assert!(all.iter().all(|i| i.thread_id == t.id));
    let tail = s.thread_items(&t.id, 2).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(
        tail.last().unwrap().id,
        all.last().unwrap().id,
        "newest last"
    );
    assert_eq!(tail[0].id, all[all.len() - 2].id);

    // Items are local: a rebuild leaves them alone and they never reach the log.
    s.rebuild_projections().unwrap();
    let after: Vec<ConversationItem> = all
        .iter()
        .map(|i| s.item(&i.id).unwrap().unwrap())
        .collect();
    assert_eq!(s.thread_items(&t.id, 100).unwrap(), after);
    assert_eq!(s.items(&s1.id, 0, 10).unwrap().len(), 3);
    assert!(
        s.events_since(0, 100)
            .unwrap()
            .iter()
            .all(|(_, e)| !e.content.contains("which?"))
    );
}

// --- managed runtime: sessions, sockets, turns --------------------------------

#[test]
fn managed_session_runtime_socket_and_lenient_wire() {
    let s = store();
    let owner = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let pane = s
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    assert_eq!(pane.runtime, SessionRuntime::Pane);
    let sess = s
        .start_session(
            &owner,
            &NewSession {
                runtime: SessionRuntime::Managed,
                ..new_session(&t.id, &owner, "mac")
            },
        )
        .unwrap();
    assert_eq!(sess.runtime, SessionRuntime::Managed);
    assert_eq!(sess.socket_path, None);
    let wire: serde_json::Value = serde_json::from_str(
        &s.get_event(&s.thread_events(&t.id).unwrap().last().unwrap().id)
            .unwrap()
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(wire["runtime"], serde_json::json!("managed"));

    s.set_session_socket(&sess.id, Some(Path::new("/run/s.sock")))
        .unwrap();
    s.set_transcript_offset(&sess.id, 77).unwrap();
    assert_eq!(
        s.get_session(&sess.id)
            .unwrap()
            .unwrap()
            .socket_path
            .as_deref(),
        Some(Path::new("/run/s.sock"))
    );
    // Replays (a new event on the thread) and rebuilds keep the local columns.
    s.update_thread(&owner, &t.id, Some("renamed"), None)
        .unwrap();
    s.rebuild_projections().unwrap();
    let after = s.get_session(&sess.id).unwrap().unwrap();
    assert_eq!(after.socket_path.as_deref(), Some(Path::new("/run/s.sock")));
    assert_eq!(after.transcript_offset, 77);
    assert_eq!(after.runtime, SessionRuntime::Managed);
    assert!(
        s.thread_events(&t.id)
            .unwrap()
            .iter()
            .all(|e| !e.content.contains("s.sock")),
        "socket is not in the log"
    );
    s.set_session_socket(&sess.id, None).unwrap();
    assert!(
        s.get_session(&sess.id)
            .unwrap()
            .unwrap()
            .socket_path
            .is_none()
    );
    assert!(matches!(
        s.set_session_socket("nope", None),
        Err(Error::NotFound(_))
    ));

    // A session.start from an older replica without "runtime", or with an unknown one, reads as pane.
    for content in [
        r#"{"environment_id":"e","environment_label":"old","harness":"claude","runtime_mode":"autopilot"}"#,
        r#"{"environment_id":"e","environment_label":"old","harness":"claude","runtime_mode":"autopilot","runtime":"hologram"}"#,
    ] {
        let sid = new_id();
        let e = EnvelopeBuilder::new(kind::SESSION_START, content)
            .thread(&t.id)
            .tag(vec!["s".into(), sid.clone()])
            .mention(&pk(&owner))
            .sign(&owner)
            .unwrap();
        assert!(s.append(&e).unwrap().projected);
        assert_eq!(
            s.get_session(&sid).unwrap().unwrap().runtime,
            SessionRuntime::Pane
        );
    }
    assert_eq!(SessionRuntime::default(), SessionRuntime::Pane);
    for k in ItemKind::ALL {
        assert_eq!(k.as_str().parse::<ItemKind>().unwrap(), *k);
    }
    for st in ItemStatus::ALL {
        assert_eq!(st.as_str().parse::<ItemStatus>().unwrap(), *st);
    }
    assert!(kind::is_thread_kind(kind::TURN) && kind::is_own_agent_kind(kind::TURN));
    assert!(!kind::is_message_kind(kind::HANDOFF) && kind::is_addressed_kind(kind::HANDOFF));
    assert!(!kind::is_message_kind(kind::STATUS) && kind::is_message_kind(kind::ANSWER));
}

#[test]
fn turn_roundtrip_projection_and_convergence() {
    let a = store();
    let owner = Keypair::generate();
    let bob = Keypair::generate();
    let atlas = Keypair::generate();
    a.insert_agent(&agent_spec("atlas", &atlas, Some(&owner)))
        .unwrap();
    let t = a.create_thread(&owner, "t", "i", None).unwrap();
    a.add_participant(&owner, &t.id, &pk(&bob), ParticipantRole::Member)
        .unwrap();
    let s1 = a
        .start_session(
            &atlas,
            &NewSession {
                runtime: SessionRuntime::Managed,
                ..new_session(&t.id, &atlas, "mac")
            },
        )
        .unwrap();
    let s2 = a
        .start_session(&owner, &new_session(&t.id, &owner, "studio"))
        .unwrap();

    let summary = TurnSummary {
        ordinal: 1,
        user_text: "fix the test".into(),
        assistant_text: "Done, 2 files.".into(),
        tool_calls: 5,
        files_edited: 2,
        duration_ms: 42_000,
        stop_reason: "end_turn".into(),
        changes: Some(TurnChanges {
            before: "a".repeat(40),
            after: "b".repeat(40),
            files: vec![
                FileChange { path: "src/lib.rs".into(), old_path: None, status: ChangeStatus::Modified, added: Some(4), removed: Some(1), authored: true },
                FileChange { path: "img.png".into(), old_path: None, status: ChangeStatus::Added, added: None, removed: None, authored: false },
            ],
            truncated: false,
            between_turns: true,
        }),
    };
    a.record_turn(&atlas, &s1.id, &summary).unwrap();
    a.record_turn(
        &atlas,
        &s1.id,
        &TurnSummary {
            ordinal: 2,
            assistant_text: "x".repeat(5000),
            ..Default::default()
        },
    )
    .unwrap();
    a.record_turn(
        &owner,
        &s2.id,
        &TurnSummary {
            ordinal: 1,
            user_text: "review".into(),
            ..Default::default()
        },
    )
    .unwrap();
    a.post(&note(&bob, "looks good", &[&pk(&owner)], Some(&t.id)))
        .unwrap();

    let turns = a.turns(&t.id).unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].summary, summary);
    assert_eq!(
        (turns[0].session_id.as_str(), turns[0].from_pubkey.as_str()),
        (s1.id.as_str(), pk(&atlas).as_str())
    );
    assert_eq!(
        turns[1].summary.assistant_text.chars().count(),
        2000,
        "assistant head is capped"
    );
    assert_eq!(turns[2].session_id, s2.id);
    assert_eq!(turns[0].summary.changes.as_ref().unwrap().totals(), (4, 1), "binary files count for no lines");
    assert_eq!(turns[0].summary.changes.as_ref().unwrap().authored(), 1);
    assert!(turns[1].summary.changes.is_none(), "a turn without checkpoints carries none");
    assert!(
        !serde_json::to_string(&turns[1].summary).unwrap().contains("changes"),
        "and its envelope doesn't mention them, so old readers see the same JSON"
    );
    let env = a.get_event(&turns[0].id).unwrap().unwrap();
    assert_eq!(env.kind, kind::TURN);
    assert_eq!(env.first_tag("s"), Some(s1.id.as_str()));
    assert_eq!(env.thread_id(), Some(t.id.as_str()));
    assert_eq!(
        serde_json::from_str::<TurnSummary>(&env.content).unwrap(),
        summary
    );
    assert_eq!(
        a.thread_messages(&t.id).unwrap().len(),
        1,
        "turns are not conversation messages"
    );
    assert!(matches!(
        a.record_turn(&owner, "nope", &summary),
        Err(Error::NotFound(_))
    ));
    // A stranger's turn is stored but not projected.
    let stranger = Keypair::generate();
    let fake = EnvelopeBuilder::new(kind::TURN, serde_json::to_string(&summary).unwrap())
        .thread(&t.id)
        .tag(vec!["s".into(), s1.id.clone()])
        .sign(&stranger)
        .unwrap();
    assert!(!a.append(&fake).unwrap().projected);
    assert_eq!(a.turns(&t.id).unwrap().len(), 3);

    // Shuffled replication converges, turns included; a rebuild changes nothing.
    let expected = a.thread_summary(&t.id).unwrap().unwrap();
    let events: Vec<Envelope> = a
        .events_since(0, 1000)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    for seed in [1u64, 0xDEADBEEF, 99_991] {
        let mut shuffled = events.clone();
        shuffle(&mut shuffled, seed);
        let b = store();
        for e in &shuffled {
            b.append(e).unwrap();
        }
        assert_eq!(b.turns(&t.id).unwrap(), turns, "seed {seed}");
        assert_eq!(
            b.thread_summary(&t.id).unwrap().unwrap(),
            expected,
            "seed {seed}"
        );
        assert_eq!(
            ids(&b.thread_messages(&t.id).unwrap()),
            ids(&a.thread_messages(&t.id).unwrap())
        );
        b.rebuild_projections().unwrap();
        assert_eq!(b.turns(&t.id).unwrap(), turns, "seed {seed} after rebuild");
        assert!(
            b.thread_items(&t.id, 10).unwrap().is_empty(),
            "items never replicate"
        );
    }
}

// --- migrations: v1 database upgrades to v2 ------------------------------------

#[test]
fn v1_database_upgrades_to_v2_with_defaults_and_turn_projection() {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut conn, 1).unwrap();
    assert_eq!(schema::current_version(&conn).unwrap(), 1);
    let has = |conn: &Connection, table: &str| -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    };
    assert!(!has(&conn, "turns") && !has(&conn, "conversation_items"));
    assert!(
        conn.execute("SELECT socket_path FROM sessions", [])
            .is_err(),
        "v1 has no socket_path"
    );

    // A v1 session row, plus a thread whose log already holds a kind-40 turn
    // (written by a newer replica and synced in, or by a build that stored
    // it before projecting).  Build the envelopes with a real store so they
    // verify, then copy them into the v1 database as raw event rows.
    let src = store();
    let owner = Keypair::generate();
    let t = src.create_thread(&owner, "legacy", "i", None).unwrap();
    let sess = src
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    src.record_turn(
        &owner,
        &sess.id,
        &TurnSummary {
            ordinal: 1,
            user_text: "u".into(),
            ..Default::default()
        },
    )
    .unwrap();
    for (_, e) in src.events_since(0, 100).unwrap() {
        conn.execute(
            "INSERT INTO events (id, from_pubkey, created_at, kind, tags_json, content, sig, thread_id, session_id, received_at, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            rusqlite::params![
                e.id,
                e.from,
                e.created_at as i64,
                e.kind as i64,
                serde_json::to_string(&e.tags).unwrap(),
                e.content,
                e.sig,
                t.id,
                e.first_tag("s"),
                1_700_000_000i64
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO sessions (id, thread_id, agent_pubkey, environment_id, environment_label, harness, runtime_mode,
             transcript_offset, state, started_at)
         VALUES (?1, ?2, ?3, 'env', 'mac', 'claude', 'autopilot', 512, 'running', 1700000000)",
        rusqlite::params![sess.id, t.id, pk(&owner)],
    )
    .unwrap();

    // Same path Store::open takes.
    schema::migrate(&mut conn).unwrap();
    assert_eq!(schema::current_version(&conn).unwrap(), SCHEMA_VERSION);
    assert!(has(&conn, "turns") && has(&conn, "conversation_items"));
    let applied: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(applied, SCHEMA_VERSION);

    let s = Store::from_connection(conn);
    let got = s.get_session(&sess.id).unwrap().unwrap();
    assert_eq!(got.runtime, SessionRuntime::Pane);
    assert_eq!(got.socket_path, None);
    assert_eq!(
        got.transcript_offset, 512,
        "local offset survives the v2 re-projection"
    );
    assert_eq!(got.environment_label, "mac");
    let turns = s.turns(&t.id).unwrap();
    assert_eq!(
        turns.len(),
        1,
        "pre-existing kind-40 events are projected by v2"
    );
    assert_eq!(turns[0].summary.user_text, "u");
    // The upgraded database is fully usable.
    s.push_item(&item(
        &got,
        1,
        ItemKind::UserMessage,
        None,
        None,
        serde_json::json!({"text": "hi"}),
    ))
    .unwrap();
    assert_eq!(s.items(&sess.id, 0, 10).unwrap().len(), 1);
    s.set_session_socket(&sess.id, Some(Path::new("/run/x.sock")))
        .unwrap();
    assert!(
        s.get_session(&sess.id)
            .unwrap()
            .unwrap()
            .socket_path
            .is_some()
    );
    // Migrating again is a no-op.
    s.rebuild_projections().unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn interim_v1_database_with_v2_objects_migrates_cleanly() {
    // The interim build created the v2 objects under user_version 1.
    let mut conn = Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut conn, 1).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions ADD COLUMN runtime TEXT NOT NULL DEFAULT 'pane';
         ALTER TABLE sessions ADD COLUMN socket_path TEXT;",
    )
    .unwrap();
    conn.execute_batch(schema::migration_sql(2)).unwrap();
    assert_eq!(schema::current_version(&conn).unwrap(), 1);
    conn.execute(
        "INSERT INTO sessions (id, thread_id, agent_pubkey, environment_id, environment_label, harness, runtime_mode,
             runtime, socket_path, state, started_at)
         VALUES ('s1', 't1', 'pk', 'env', 'mac', 'claude', 'autopilot', 'managed', '/run/s1.sock', 'running', 1700000000)",
        [],
    )
    .unwrap();

    schema::migrate(&mut conn).unwrap();
    assert_eq!(schema::current_version(&conn).unwrap(), SCHEMA_VERSION);
    let s = Store::from_connection(conn);
    let got = s.get_session("s1").unwrap().unwrap();
    assert_eq!(
        got.runtime,
        SessionRuntime::Managed,
        "existing column values are kept"
    );
    assert_eq!(got.socket_path.as_deref(), Some(Path::new("/run/s1.sock")));
    assert!(s.turns("t1").unwrap().is_empty());
    assert!(s.pending_items(None).unwrap().is_empty());
    // And a third open is still a no-op.
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn agent_policy_defaults_round_trip_and_reach_edits() {
    let s = store();
    let a = Keypair::generate();
    let b = Keypair::generate();
    let p = s.agent_policy(&pk(&a)).unwrap();
    assert_eq!(p, AgentPolicy::default());
    assert_eq!(p.reach, Reach::Nobody, "agents reach nobody by default");
    assert_eq!(p.budget, Budget { spend: SpendLimit::Unlimited, max_depth: DEFAULT_MAX_DEPTH });

    let mut p = p;
    p.reach.allow(&pk(&b));
    p.reach.allow(&pk(&b));
    assert_eq!(p.reach, Reach::Only(vec![pk(&b)]));
    p.budget.spend = SpendLimit::DailyUsd(2.5);
    p.budget.max_depth = 1;
    s.set_agent_policy(&pk(&a), &p).unwrap();
    assert_eq!(s.agent_policy(&pk(&a)).unwrap(), p);
    assert!(p.reach.disallow(&pk(&b)));
    assert_eq!(p.reach, Reach::Nobody, "emptied list is nobody");
    let mut any = Reach::Anyone;
    assert!(!any.disallow(&pk(&b)), "anyone can't exclude one");
    assert!(any.allows("whoever"));
    // Stored JSON is tagged, so it stays readable and stable.
    assert_eq!(serde_json::to_string(&Reach::Only(vec!["k".into()])).unwrap(), r#"{"kind":"only","agents":["k"]}"#);
    assert_eq!(serde_json::to_string(&SpendLimit::DailyUsd(1.0)).unwrap(), r#"{"kind":"daily_usd","usd":1.0}"#);
    assert_eq!(narrower(RuntimeMode::Autopilot, RuntimeMode::Edits), RuntimeMode::Edits);
    assert_eq!(narrower(RuntimeMode::Plan, RuntimeMode::Autopilot), RuntimeMode::Plan);
}

#[test]
fn summons_check_reach_cycles_depth_mode_and_budget() {
    let s = store();
    let owner = Keypair::generate();
    let (alice, bob, carol) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    for (n, k) in [("alice", &alice), ("bob", &bob), ("carol", &carol)] {
        s.insert_agent(&agent_spec(n, k, Some(&owner))).unwrap();
    }
    let t = s.create_thread(&owner, "T", "t", None).unwrap();
    let mut alice_s = new_session(&t.id, &alice, "local");
    alice_s.runtime_mode = RuntimeMode::Edits;
    let sa = s.start_session(&alice, &alice_s).unwrap();

    // The owner may always summon (within budget).
    assert_eq!(s.check_summon(None, &pk(&bob), 0).unwrap(), SummonCheck::Allowed { parent: None, mode_cap: None });
    // alice reaches nobody by default.
    assert_eq!(
        s.check_summon(Some(&sa.id), &pk(&bob), 0).unwrap(),
        SummonCheck::Refused(Refusal::NotReachable { from: pk(&alice), to: pk(&bob) })
    );
    let mut ap = s.agent_policy(&pk(&alice)).unwrap();
    ap.reach.allow(&pk(&bob));
    s.set_agent_policy(&pk(&alice), &ap).unwrap();
    // Now allowed, capped at alice's session mode.
    assert_eq!(
        s.check_summon(Some(&sa.id), &pk(&bob), 0).unwrap(),
        SummonCheck::Allowed { parent: Some(sa.id.clone()), mode_cap: Some(RuntimeMode::Edits) }
    );
    let sb = s.start_session(&bob, &new_session(&t.id, &bob, "local")).unwrap();
    assert_eq!(s.link_session(&sb.id, &sa.id).unwrap(), 1);
    assert_eq!(s.summon_chain(&sb.id).unwrap(), vec![pk(&alice), pk(&bob)]);

    // bob → alice is a loop even when bob may reach alice; so is bob → bob.
    s.set_agent_policy(&pk(&bob), &AgentPolicy { reach: Reach::Anyone, budget: Budget { max_depth: 1, ..Default::default() } }).unwrap();
    assert!(matches!(s.check_summon(Some(&sb.id), &pk(&alice), 0).unwrap(), SummonCheck::Refused(Refusal::Cycle { .. })));
    assert!(matches!(s.check_summon(Some(&sb.id), &pk(&bob), 0).unwrap(), SummonCheck::Refused(Refusal::Cycle { .. })));
    // bob (depth 1, max 1) can't go deeper.
    assert_eq!(
        s.check_summon(Some(&sb.id), &pk(&carol), 0).unwrap(),
        SummonCheck::Refused(Refusal::TooDeep { agent: pk(&bob), depth: 2, max: 1 })
    );

    // Spend limit: carol at $1/day with $1.20 recorded today is refused, even for the owner.
    let sc = s.start_session(&carol, &new_session(&t.id, &carol, "local")).unwrap();
    s.add_usage(&carol, &sc.id, &UsageDelta { input_tokens: 10, cost_usd_est: 1.2, ..Default::default() }, None).unwrap();
    s.set_agent_policy(&pk(&carol), &AgentPolicy { budget: Budget { spend: SpendLimit::DailyUsd(1.0), ..Default::default() }, ..Default::default() }).unwrap();
    assert!((s.agent_spend_since(&pk(&carol), 0).unwrap() - 1.2).abs() < 1e-9);
    assert!(matches!(s.check_summon(None, &pk(&carol), 0).unwrap(), SummonCheck::Refused(Refusal::OverBudget { .. })));
    // Usage from before the day started doesn't count.
    assert_eq!(s.agent_spend_since(&pk(&carol), i64::MAX).unwrap(), 0.0);
    assert!(matches!(s.check_summon(None, &pk(&carol), i64::MAX).unwrap(), SummonCheck::Allowed { .. }));
}

// ---------------------------------------------------------------- remote hosts

fn sync_all(from: &Store, to: &Store) {
    for (_, e) in from.events_since(0, 10_000).unwrap() {
        to.append(&e).unwrap();
    }
}

/// The laptop (owner + attested atlas) and the mini (its own host key, no
/// agent keys).  atlas delegates to the mini's key for one thread.
#[test]
fn a_host_works_for_a_delegating_agent_and_the_laptop_sees_the_agent() {
    let laptop = store();
    let mini = store();
    let owner = Keypair::generate();
    let atlas = Keypair::generate();
    let host = Keypair::generate();
    laptop.insert_agent(&agent_spec("atlas", &atlas, Some(&owner))).unwrap();
    let t = laptop.create_thread(&owner, "t", "fix the flaky test", None).unwrap();
    let other = laptop.create_thread(&owner, "other", "x", None).unwrap();
    sync_all(&laptop, &mini);

    let now = chrono::Utc::now().timestamp() as u64;
    let grant = axel_identity::delegate(&atlas, &host.public(), &t.id, now - 60, now + 3600).unwrap();
    let auth = attest(&owner, &atlas.public(), "").unwrap().to_tag();
    mini.put_delegation(&pk(&host), &grant.to_tag(), Some(&auth)).unwrap();
    assert_eq!(mini.delegator(&pk(&host), &t.id).unwrap(), Some(pk(&atlas)));
    assert_eq!(mini.delegator(&pk(&host), &other.id).unwrap(), None);

    // The mini runs atlas's session, signing with its own key.
    let mut ns = new_session(&t.id, &atlas, "mini");
    ns.runtime = SessionRuntime::Managed;
    let s = mini.start_session(&host, &ns).unwrap();
    assert_eq!(s.agent_pubkey, pk(&atlas));
    mini.record_status(&host, &s.id, &StatusReport { state: StatusState::Done, ..Default::default() }).unwrap();
    let msg = mini
        .sign_with_attestation(&host, EnvelopeBuilder::new(kind::NOTE, "done: see branch").mention(&pk(&owner)).thread(&t.id), Some(&t.id))
        .unwrap();
    mini.post(&msg).unwrap();
    assert_eq!(author(&msg), pk(&atlas));
    assert_eq!(msg.from, pk(&host));

    // Back on the laptop it is atlas's work.
    sync_all(&mini, &laptop);
    let back = laptop.get_session(&s.id).unwrap().expect("session replicated");
    assert_eq!(back.agent_pubkey, pk(&atlas));
    assert_eq!(laptop.status(&s.id).unwrap().unwrap().state, StatusState::Done);
    let inbox = laptop.inbox(&pk(&owner), 10).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(author(&inbox[0]), pk(&atlas));

    // The grant doesn't reach another thread.
    let stray = mini
        .sign_with_attestation(
            &host,
            EnvelopeBuilder::new(kind::NOTE, "stray").thread(&other.id).tag(grant.to_tag()).tag(auth.clone()),
            None,
        )
        .unwrap();
    let r = laptop.append(&stray).unwrap();
    assert!(r.rejected_reason.unwrap().contains("delegation"), "wrong thread");

    // A delegate can't add participants, even on its thread.
    let sneaky = EnvelopeBuilder::new(kind::THREAD_PARTICIPANT, r#"{"role":"owner","op":"add"}"#)
        .thread(&t.id)
        .mention(&pk(&host))
        .tag(grant.to_tag())
        .sign(&host)
        .unwrap();
    laptop.append(&sneaky).unwrap();
    assert_eq!(laptop.participants(&t.id).unwrap().iter().filter(|p| p.pubkey == pk(&host)).count(), 0);

    // A forged grant (another key "delegating" for atlas) is rejected.
    let mallory = Keypair::generate();
    let mut forged = axel_identity::delegate(&mallory, &host.public(), &t.id, now - 60, now + 3600).unwrap();
    forged.delegator = atlas.public();
    let fake = EnvelopeBuilder::new(kind::NOTE, "fake").thread(&t.id).tag(forged.to_tag()).sign(&host).unwrap();
    assert!(laptop.append(&fake).unwrap().rejected_reason.unwrap().contains("delegation"));
}

#[test]
fn items_of_a_hosts_sessions_replicate_with_updates_and_deletes() {
    let mini = store();
    let laptop = store();
    let owner = Keypair::generate();
    let t = mini.create_thread(&owner, "t", "i", None).unwrap();
    let env = mini.local_environment().unwrap();
    let mut ns = new_session(&t.id, &owner, "mini");
    ns.environment_id = env.id.clone();
    let s = mini.start_session(&owner, &ns).unwrap();
    sync_all(&mini, &laptop);

    let a = mini.push_item(&item(&s, 1, ItemKind::AssistantMessage, None, None, serde_json::json!({"text": "Hel"}))).unwrap();
    let b = mini.push_item(&item(&s, 1, ItemKind::AssistantMessage, None, None, serde_json::json!({"text": "tmp"}))).unwrap();
    let ship = |cursor: i64| -> i64 {
        let changes = mini.item_changes_since(&env.id, cursor, 100).unwrap();
        for c in &changes {
            laptop.import_item_change(c).unwrap();
        }
        changes.last().map_or(cursor, ItemChange::rev)
    };
    let c1 = ship(0);
    assert_eq!(laptop.items(&s.id, 0, 100).unwrap().len(), 2);
    mini.update_item(&a.id, None, Some(serde_json::json!({"text": "Hello"}))).unwrap();
    mini.delete_item(&b.id).unwrap();
    let c2 = ship(c1);
    assert!(c2 > c1);
    let got = laptop.items(&s.id, 0, 100).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].content["text"], "Hello");
    assert_eq!(ship(c2), c2, "nothing new");
    // Another environment's sessions aren't exported.
    assert!(mini.item_changes_since("elsewhere", 0, 100).unwrap().is_empty());
}

#[test]
fn an_agent_can_have_a_default_host_until_the_host_goes() {
    let s = store();
    let atlas = Keypair::generate();
    assert!(s.agent_host(&pk(&atlas)).unwrap().is_none());
    s.put_host("env-mini", "mini", "mini", "hostpk", Some("mini")).unwrap();
    s.set_agent_host(&pk(&atlas), Some("env-mini")).unwrap();
    assert_eq!(s.agent_host(&pk(&atlas)).unwrap().map(|e| e.label), Some("mini".to_string()));
    s.set_agent_host(&pk(&atlas), None).unwrap();
    assert!(s.agent_host(&pk(&atlas)).unwrap().is_none());
    s.set_agent_host(&pk(&atlas), Some("env-mini")).unwrap();
    assert!(s.remove_host("mini").unwrap());
    assert!(s.agent_host(&pk(&atlas)).unwrap().is_none(), "a removed host is nobody's default");
}

/// Two agents working on one thread on the same host: each job has its own
/// key, so each signs as its own agent.
#[test]
fn two_agents_on_one_host_and_thread_sign_as_themselves() {
    let laptop = store();
    let mini = store();
    let owner = Keypair::generate();
    let (orion, lyra) = (Keypair::generate(), Keypair::generate());
    let (k_orion, k_lyra) = (Keypair::generate(), Keypair::generate());
    laptop.insert_agent(&agent_spec("orion", &orion, Some(&owner))).unwrap();
    laptop.insert_agent(&agent_spec("lyra", &lyra, Some(&owner))).unwrap();
    let t = laptop.create_thread(&owner, "t", "i", None).unwrap();
    sync_all(&laptop, &mini);
    let now = chrono::Utc::now().timestamp() as u64;
    for (agent, key) in [(&orion, &k_orion), (&lyra, &k_lyra)] {
        let grant = axel_identity::delegate(agent, &key.public(), &t.id, now - 60, now + 3600).unwrap();
        let auth = attest(&owner, &agent.public(), "").unwrap().to_tag();
        mini.put_delegation(&pk(key), &grant.to_tag(), Some(&auth)).unwrap();
    }
    assert_eq!(mini.delegate_signer(&pk(&orion), &t.id).unwrap(), Some(pk(&k_orion)));
    assert_eq!(mini.delegate_signer(&pk(&lyra), &t.id).unwrap(), Some(pk(&k_lyra)));
    let sr = mini.start_session(&k_orion, &new_session(&t.id, &orion, "mini")).unwrap();
    let sl = mini.start_session(&k_lyra, &new_session(&t.id, &lyra, "mini")).unwrap();
    sync_all(&mini, &laptop);
    assert_eq!(laptop.get_session(&sr.id).unwrap().unwrap().agent_pubkey, pk(&orion));
    assert_eq!(laptop.get_session(&sl.id).unwrap().unwrap().agent_pubkey, pk(&lyra));
}

#[test]
fn usage_is_kept_per_agent_over_the_last_hour_day_and_ever() {
    let s = store();
    let owner = Keypair::generate();
    let (terra, opus) = (Keypair::generate(), Keypair::generate());
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let other = s.create_thread(&owner, "other", "i", None).unwrap();
    for (th, a) in [(&t.id, &terra), (&t.id, &opus), (&other.id, &terra)] {
        s.add_participant(&owner, th, &pk(a), ParticipantRole::Agent)
            .unwrap();
    }
    let st = s
        .start_session(&terra, &new_session(&t.id, &terra, "mac"))
        .unwrap();
    let so = s
        .start_session(&opus, &new_session(&t.id, &opus, "mac"))
        .unwrap();
    let st2 = s
        .start_session(&terra, &new_session(&other.id, &terra, "mac"))
        .unwrap();

    let now = crate::now();
    // terra: 30 min ago, 5 hours ago, 3 days ago (the 24h samples drop that
    // one; the usage log keeps it).
    let spend = |signer: &Keypair, thread: &str, session: &str, ago: i64, input: i64, cost: f64| {
        let env = EnvelopeBuilder::new(
            kind::USAGE,
            format!(r#"{{"input": {input}, "output": 1, "cache_read": 10, "reasoning": 1, "turns": 1, "cost_usd_est": {cost}, "model": "claude-opus-5-5"}}"#),
        )
        .thread(thread)
        .tag(vec!["s".into(), session.into()])
        .created_at((now - ago) as u64)
        .sign(signer)
        .unwrap();
        assert!(s.append(&env).unwrap().projected);
    };
    spend(&terra, &t.id, &st.id, 1800, 100, 1.0);
    spend(&terra, &t.id, &st.id, 5 * 3600, 200, 2.0);
    spend(&terra, &other.id, &st2.id, 3 * 86_400, 400, 4.0);
    spend(&opus, &t.id, &so.id, 60, 7, 0.5);

    let by_agent = s.usage_by_agent(now).unwrap();
    assert_eq!(by_agent.len(), 2);
    assert_eq!(by_agent[0].key, pk(&opus), "the most recent spender first");
    let terra_u = by_agent.iter().find(|u| u.key == pk(&terra)).unwrap();
    assert_eq!(terra_u.last_hour.input_tokens, 100);
    assert_eq!(terra_u.last_day.input_tokens, 300);
    assert_eq!(terra_u.total.input_tokens, 700);
    assert_eq!(terra_u.total.turns, 3);
    assert_eq!(terra_u.total.reasoning_tokens, 3);
    assert!((terra_u.total.cost_usd - 7.0).abs() < 1e-9);
    assert_eq!(
        terra_u.last_hour.tokens(),
        100 + 1 + 10,
        "input, output and cache reads"
    );

    let by_thread = s.usage_by_thread(now, 10).unwrap();
    let t_u = by_thread.iter().find(|u| u.key == t.id).unwrap();
    assert_eq!(
        (t_u.last_hour.input_tokens, t_u.total.input_tokens),
        (107, 307)
    );
    assert_eq!(s.usage_by_thread(now, 1).unwrap().len(), 1);

    // An hour in four 15-minute slices: terra's 30-minute-old turn and opus's.
    assert_eq!(
        s.usage_series(Some(&pk(&terra)), now, 900, 4).unwrap(),
        vec![0, 0, 111, 0]
    );
    assert_eq!(
        s.usage_series(None, now, 900, 4).unwrap(),
        vec![0, 0, 111, 18]
    );

    // The log is a projection: rebuilt, it holds the same.
    s.rebuild_projections().unwrap();
    assert_eq!(s.usage_by_agent(now).unwrap(), by_agent);
}

#[test]
fn a_sessions_context_fill_is_a_snapshot() {
    let s = store();
    let owner = Keypair::generate();
    let t = s.create_thread(&owner, "t", "i", None).unwrap();
    let sess = s
        .start_session(&owner, &new_session(&t.id, &owner, "mac"))
        .unwrap();
    s.set_session_context(&sess.id, 10_000, 200_000).unwrap();
    s.set_session_context(&sess.id, 42_000, 200_000).unwrap();
    let c = s.session_contexts().unwrap();
    assert_eq!(
        (c.len(), c[0].used, c[0].size),
        (1, 42_000, 200_000),
        "replaced, not summed"
    );
    s.end_session(&owner, &sess.id, None).unwrap();
    assert!(
        s.session_contexts().unwrap().is_empty(),
        "only live sessions"
    );
}

#[test]
fn a_plans_windows_are_kept_per_harness_and_replaced() {
    let s = store();
    s.set_plan_limit(&Harness::Codex, LimitWindow::Week, 40.0, Some(2_000)).unwrap();
    s.set_plan_limit(&Harness::Claude, LimitWindow::Week, 9.0, Some(3_000)).unwrap();
    s.set_plan_limit(&Harness::Claude, LimitWindow::FiveHour, 13.0, Some(1_000)).unwrap();
    s.set_plan_limit(&Harness::Claude, LimitWindow::FiveHour, 140.0, None).unwrap();
    let l = s.plan_limits().unwrap();
    let got: Vec<(Harness, LimitWindow, f64)> = l.iter().map(|l| (l.harness.clone(), l.window, l.used_pct)).collect();
    assert_eq!(
        got,
        vec![(Harness::Claude, LimitWindow::FiveHour, 100.0), (Harness::Claude, LimitWindow::Week, 9.0), (Harness::Codex, LimitWindow::Week, 40.0)],
        "replaced, clamped, five hours before the week"
    );
    assert_eq!(l[0].resets_at, None);
    assert_eq!(LimitWindow::from_minutes(300), LimitWindow::FiveHour);
    assert_eq!(LimitWindow::from_minutes(10_080), LimitWindow::Week);
    assert_eq!(LimitWindow::from_minutes(43_200), LimitWindow::Month);
}
