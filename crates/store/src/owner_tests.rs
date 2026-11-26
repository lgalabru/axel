//! Owner records (RFD 0004): device links, profiles, instance announcements,
//! and the authorization that resolves a device to its owner.

use axel_identity::{Keypair, attest};
use axel_inbox::{Envelope, EnvelopeBuilder};

use super::*;

const T0: u64 = 1_700_000_000;

fn store() -> Store {
    Store::open_memory().expect("open_memory")
}

fn pk(kp: &Keypair) -> String {
    kp.public().to_base58()
}

fn device_record(owner: &Keypair, op: DeviceOp, device: &Keypair, label: &str, at: u64) -> Envelope {
    let record = DeviceRecord { op, device: pk(device), label: label.into(), route: Some(PairingRoute::Ssh) };
    EnvelopeBuilder::new(kind::OWNER_DEVICE, serde_json::to_string(&record).unwrap())
        .tag(vec!["d".into(), pk(device)])
        .created_at(at)
        .sign(owner)
        .unwrap()
}

fn note_at(signer: &Keypair, text: &str, thread: &str, at: u64) -> Envelope {
    EnvelopeBuilder::new(kind::NOTE, text).thread(thread).created_at(at).sign(signer).unwrap()
}

fn profile_wire(id: &str, owner: &Keypair, name: &str, model: &str) -> ProfileWire {
    ProfileWire {
        profile_id: id.into(),
        owner: pk(owner),
        name: name.into(),
        harness: Harness::Claude,
        model: Some(model.into()),
        effort: None,
        skills: vec![],
        prompt: None,
        color: None,
        runtime_mode: RuntimeMode::Autopilot,
        archived: false,
    }
}

fn profile_record(signer: &Keypair, wire: &ProfileWire, at: u64) -> Envelope {
    EnvelopeBuilder::new(kind::AGENT_PROFILE, serde_json::to_string(wire).unwrap())
        .tag(vec!["d".into(), wire.profile_id.clone()])
        .created_at(at)
        .sign(signer)
        .unwrap()
}

fn announce(device: &Keypair, instance: &Keypair, profile_id: &str, at: u64) -> Envelope {
    let wire = AnnounceWire {
        instance: pk(instance),
        profile_id: profile_id.into(),
        auth: attest(device, &instance.public(), "").unwrap().to_tag(),
    };
    EnvelopeBuilder::new(kind::AGENT_ANNOUNCE, serde_json::to_string(&wire).unwrap())
        .tag(vec!["d".into(), pk(instance)])
        .created_at(at)
        .sign(device)
        .unwrap()
}

fn is_projected(s: &Store, env: &Envelope) -> bool {
    s.rejected_reason(&env.id).unwrap().is_none() && s.get_event(&env.id).unwrap().is_some()
}

#[test]
fn a_linked_device_acts_for_its_owner_and_is_revived_when_the_link_arrives_late() {
    let s = store();
    let (owner, laptop) = (Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    let from_laptop = note_at(&laptop, "hello from the laptop", &thread.id, T0 + 10);
    let appended = s.append(&from_laptop).unwrap();
    assert!(appended.rejected_reason.is_some(), "an unlinked machine is nobody");

    let link = device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0);
    assert!(s.append(&link).unwrap().projected);
    assert!(is_projected(&s, &from_laptop), "the link revives what the device signed");
    assert_eq!(s.principal(&pk(&laptop)).unwrap(), pk(&owner));
    assert_eq!(s.device_by_label("laptop").unwrap().unwrap().owner, Some(pk(&owner)));

    // Replay from scratch agrees.
    s.rebuild_projections().unwrap();
    assert!(is_projected(&s, &from_laptop));
}

#[test]
fn a_thread_a_device_started_is_its_owners() {
    let s = store();
    let (owner, laptop) = (Keypair::generate(), Keypair::generate());
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    let thread = s.create_thread(&laptop, "t", "intent", None).unwrap();
    // The owner, on another machine, may change the roster and post.
    s.add_participant(&owner, &thread.id, "someone", ParticipantRole::Member).unwrap();
    let from_owner = note_at(&owner, "mine too", &thread.id, T0 + 20);
    assert!(s.append(&from_owner).unwrap().projected);
}

#[test]
fn a_revoked_device_authorizes_nothing_signed_after_its_revocation() {
    let s = store();
    let (owner, laptop) = (Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    let before = note_at(&laptop, "before", &thread.id, T0 + 100);
    let after = note_at(&laptop, "after", &thread.id, T0 + 300);
    s.append(&before).unwrap();
    s.append(&after).unwrap();
    assert!(is_projected(&s, &after));

    s.append(&device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 200)).unwrap();
    assert!(is_projected(&s, &before), "what it signed while linked stands");
    assert!(!is_projected(&s, &after), "a revoked device speaks for no one");
    assert_eq!(s.principal(&pk(&laptop)).unwrap(), pk(&laptop));

    // A later revocation can't extend its life.
    s.append(&device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 400)).unwrap();
    assert_eq!(s.device(&pk(&laptop)).unwrap().unwrap().revoked_at, Some((T0 + 200) as i64));
}

#[test]
fn links_that_would_confuse_who_owns_what_are_refused() {
    let s = store();
    let (owner, other, laptop) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    let own = device_record(&owner, DeviceOp::Link, &owner, "me", T0);
    assert!(s.append(&own).unwrap().rejected_reason.is_some(), "an owner can't link itself");

    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    let stolen = device_record(&other, DeviceOp::Link, &laptop, "mine now", T0 + 1);
    assert!(s.append(&stolen).unwrap().rejected_reason.is_some(), "a live link isn't taken over");
    let foreign_revoke = device_record(&other, DeviceOp::Revoke, &laptop, "laptop", T0 + 2);
    assert!(s.append(&foreign_revoke).unwrap().rejected_reason.is_some(), "only its owner revokes it");

    let cycle = device_record(&laptop, DeviceOp::Link, &owner, "loop", T0 + 3);
    assert!(s.append(&cycle).unwrap().rejected_reason.is_some(), "no key ends up its own owner");
    assert_eq!(s.principal(&pk(&laptop)).unwrap(), pk(&owner));
}

#[test]
fn a_merged_home_speaks_for_the_owner_that_adopted_it() {
    // Issue #40: two homes, each with an owner key.  O1 adopts O2 as a
    // device; O2's machines and threads become O1's with no re-signing.
    let s = store();
    let (o1, o2, linux_laptop) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    s.append(&device_record(&o2, DeviceOp::Link, &linux_laptop, "linux-laptop", T0)).unwrap();
    let thread = s.create_thread(&o2, "o2's thread", "intent", None).unwrap();
    let from_o1 = note_at(&o1, "I own this too", &thread.id, T0 + 50);
    assert!(s.append(&from_o1).unwrap().rejected_reason.is_some());

    s.append(&device_record(&o1, DeviceOp::Link, &o2, "linux", T0 + 10)).unwrap();
    assert!(is_projected(&s, &from_o1));
    assert_eq!(s.principal(&pk(&linux_laptop)).unwrap(), pk(&o1), "two links up");
}

#[test]
fn the_newest_profile_wins_and_only_its_owner_may_sign_it() {
    let s = store();
    let (owner, laptop, stranger) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    let old = profile_record(&owner, &profile_wire("p-1", &owner, "reviewer", "old"), T0 + 10);
    let new = profile_record(&owner, &profile_wire("p-1", &owner, "reviewer", "new"), T0 + 20);
    s.append(&new).unwrap();
    s.append(&old).unwrap();
    assert_eq!(s.profile("p-1").unwrap().unwrap().model.as_deref(), Some("new"), "arrival order doesn't matter");

    let forged = profile_record(&stranger, &profile_wire("p-1", &owner, "reviewer", "forged"), T0 + 30);
    assert!(s.append(&forged).unwrap().rejected_reason.is_some());
    assert_eq!(s.profile("p-1").unwrap().unwrap().model.as_deref(), Some("new"));

    // A device may sign its owner's profile once the link is known.
    let from_laptop = profile_record(&laptop, &profile_wire("p-1", &owner, "reviewer", "laptop's"), T0 + 40);
    assert!(s.append(&from_laptop).unwrap().rejected_reason.is_some());
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    assert!(is_projected(&s, &from_laptop));
    let profile = s.profile("p-1").unwrap().unwrap();
    assert_eq!(profile.model.as_deref(), Some("laptop's"));
    assert_eq!(profile.updated_id.as_deref(), Some(from_laptop.id.as_str()));
}

#[test]
fn a_device_announces_its_instance_of_a_profile() {
    let s = store();
    let (owner, linux, instance) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    let link = device_record(&owner, DeviceOp::Link, &linux, "linux", T0);
    let profile = profile_record(&owner, &profile_wire("p-r", &owner, "reviewer", "m"), T0 + 1);
    let hello = announce(&linux, &instance, "p-r", T0 + 2);
    // Worst order: the announcement first, then the profile, then the link.
    assert!(s.append(&hello).unwrap().rejected_reason.is_some());
    s.append(&profile).unwrap();
    assert!(!is_projected(&s, &hello), "still waiting for the link");
    s.append(&link).unwrap();
    assert!(is_projected(&s, &hello));

    let on_linux = s.agent_on_device("reviewer", &pk(&linux)).unwrap().unwrap();
    assert_eq!(on_linux.pubkey, pk(&instance));
    assert_eq!(on_linux.profile_id, "p-r");
    // The instance acts for the owner on the owner's threads.
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    assert!(s.append(&note_at(&instance, "on it", &thread.id, T0 + 60)).unwrap().projected);
}

#[test]
fn an_announcement_never_rehomes_an_instance_this_machine_runs() {
    let s = store();
    let (owner, linux, mine) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    let agent = s
        .insert_agent(&NewAgent {
            pubkey: pk(&mine),
            name: "atlas".into(),
            auth_tag: Some(attest(&owner, &mine.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    s.append(&device_record(&owner, DeviceOp::Link, &linux, "linux", T0)).unwrap();
    s.append(&profile_record(&owner, &profile_wire(&agent.profile_id, &owner, "atlas", "m"), T0 + 1)).unwrap();
    // A forged announcement needs the instance attested by linux, which it isn't.
    let wire = AnnounceWire { instance: pk(&mine), profile_id: agent.profile_id.clone(), auth: agent.auth_tag.clone().unwrap() };
    let env = EnvelopeBuilder::new(kind::AGENT_ANNOUNCE, serde_json::to_string(&wire).unwrap()).created_at(T0 + 2).sign(&linux).unwrap();
    s.append(&env).unwrap();
    assert_eq!(s.agent_by_pubkey(&pk(&mine)).unwrap().unwrap().device, None, "still this machine's");
}

#[test]
fn two_owners_may_each_have_a_reviewer_but_one_owner_only_one() {
    let s = store();
    let (o1, o2) = (Keypair::generate(), Keypair::generate());
    let reviewer = |owner: &Keypair| {
        let key = Keypair::generate();
        s.insert_agent(&NewAgent {
            pubkey: pk(&key),
            name: "reviewer".into(),
            auth_tag: Some(attest(owner, &key.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
    };
    let first = reviewer(&o1).unwrap();
    let second = reviewer(&o2).expect("another owner's reviewer is a different agent (#40)");
    assert_ne!(first.profile_id, second.profile_id);
    assert!(matches!(reviewer(&o1), Err(Error::Conflict(_))), "names are unique per owner");

    // A second instance of an existing profile shares its name.
    let other_machine = Keypair::generate();
    let instance = s
        .insert_agent(&NewAgent {
            pubkey: pk(&other_machine),
            name: "ignored".into(),
            profile_id: Some(first.profile_id.clone()),
            device: Some("linux-device".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(instance.name, "reviewer");
    assert_eq!(s.profile_instances(&first.profile_id).unwrap().len(), 2);
    assert_eq!(s.list_agents().unwrap().iter().filter(|a| a.profile_id == first.profile_id).count(), 1, "one row per profile");
}

#[test]
fn an_edit_is_unsigned_until_published_and_wins_over_older_records() {
    let s = store();
    let owner = Keypair::generate();
    let key = Keypair::generate();
    let agent = s
        .insert_agent(&NewAgent {
            pubkey: pk(&key),
            name: "atlas".into(),
            auth_tag: Some(attest(&owner, &key.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(agent.owner, Some(pk(&owner)));
    assert_eq!(s.unsigned_profiles(&pk(&owner)).unwrap().len(), 1, "a new profile waits to be signed");

    s.set_agent_harness(&agent.pubkey, &Harness::Codex, Some("gpt")).unwrap();
    let edited = s.profile(&agent.profile_id).unwrap().unwrap();
    assert_eq!(edited.updated_id, None);

    // Publishing signs it as it stands.
    let wire = ProfileWire::of(&edited, &pk(&owner));
    let published = profile_record(&owner, &wire, edited.updated_at as u64);
    s.append(&published).unwrap();
    assert_eq!(s.profile(&agent.profile_id).unwrap().unwrap().updated_id.as_deref(), Some(published.id.as_str()));
    assert!(s.unsigned_profiles(&pk(&owner)).unwrap().is_empty());

    // An older record from elsewhere doesn't undo it.
    let stale = profile_record(&owner, &profile_wire(&agent.profile_id, &owner, "atlas", "stale"), T0);
    s.append(&stale).unwrap();
    assert_eq!(s.agent_by_pubkey(&agent.pubkey).unwrap().unwrap().model.as_deref(), Some("gpt"));
}

#[test]
fn migration_13_turns_each_agent_into_a_profile_and_its_instance() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrate_to(&mut conn, 12).unwrap();
    let owner = Keypair::generate();
    let key = Keypair::generate();
    let auth = serde_json::to_string(&attest(&owner, &key.public(), "").unwrap().to_tag()).unwrap();
    conn.execute(
        "INSERT INTO agents (pubkey, name, harness, model, skills_json, runtime_mode, auth_tag_json, created_at)
         VALUES (?1, 'atlas', 'codex', 'gpt', '[]', 'edits', ?2, 1700000000)",
        rusqlite::params![pk(&key), auth],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (pubkey, name, harness, skills_json, runtime_mode, created_at) VALUES ('pk-legacy', 'nova', 'claude', '[]', 'autopilot', 1700000001)",
        [],
    )
    .unwrap();
    schema::migrate(&mut conn).unwrap();
    let s = Store::from_connection(conn);

    let atlas = s.agent_by_name("atlas").unwrap().unwrap();
    assert_eq!(atlas.pubkey, pk(&key));
    assert_eq!(atlas.profile_id, format!("p-{}", pk(&key)));
    assert_eq!(atlas.owner, Some(pk(&owner)));
    assert_eq!(atlas.runtime_mode, RuntimeMode::Edits);
    assert_eq!(atlas.model.as_deref(), Some("gpt"));
    let nova = s.agent_by_name("nova").unwrap().unwrap();
    assert_eq!(nova.owner, None, "no attestation, no known owner");
    assert_eq!(s.profiles().unwrap().len(), 2);
}

#[test]
fn the_humans_inbox_is_the_owners_on_every_machine() {
    let s = store();
    let (owner, laptop, agent) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    s.add_participant(&owner, &thread.id, &pk(&agent), ParticipantRole::Agent).unwrap();
    // An agent on the laptop asks the human there: the message names the laptop's key.
    let question = EnvelopeBuilder::new(kind::QUESTION, "which branch?").thread(&thread.id).mention(&pk(&laptop)).created_at(T0 + 5).sign(&agent).unwrap();
    s.post(&question).unwrap();
    assert_eq!(s.inbox(&pk(&owner), 10).unwrap().len(), 1, "the owner sees it from the home machine");
    assert_eq!(s.inbox(&pk(&laptop), 10).unwrap().len(), 1);
    assert_eq!(s.unread_count(&pk(&owner)).unwrap(), 1);

    s.append(&device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 10)).unwrap();
    assert!(s.inbox(&pk(&owner), 10).unwrap().is_empty(), "a revoked device's mail isn't the owner's");
}

#[test]
fn the_inbox_walks_as_many_links_as_authorization() {
    let sql_depth = format!("s.depth < {PRINCIPAL_DEPTH_MAX}");
    assert!(crate::reads::same_owner_sql().contains(&sql_depth), "keep SAME_OWNER_KEYS and PRINCIPAL_DEPTH_MAX in step");
}
