//! Owner records (RFD 0004): device links, profiles, instance announcements,
//! and the authorization that resolves a device to its owner.

use axel_identity::{Keypair, attest, consent_to_link};
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
    let consent = (op == DeviceOp::Link && pk(owner) != pk(device)).then(|| consent_to_link(device, &owner.public()));
    let record = DeviceRecord { op, device: pk(device), label: label.into(), route: Some(PairingRoute::Ssh), consent };
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
    assert_eq!(s.device_by_label("laptop", &pk(&owner)).unwrap().unwrap().owner, Some(pk(&owner)));
    assert!(s.device_by_label("laptop", &pk(&Keypair::generate())).unwrap().is_none(), "another owner's laptop isn't ours");

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
    let instance_of = |attester: &Keypair| NewAgent {
        pubkey: pk(&other_machine),
        name: "ignored".into(),
        profile_id: Some(first.profile_id.clone()),
        device: Some("linux-device".into()),
        auth_tag: Some(attest(attester, &other_machine.public(), "").unwrap().to_tag()),
        ..Default::default()
    };
    assert!(matches!(s.insert_agent(&instance_of(&o2)), Err(Error::Conflict(_))), "o2 can't add an instance to o1's agent");
    let instance = s.insert_agent(&instance_of(&o1)).unwrap();
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

/// A link signed by `owner` with no consent from `device`: what a stranger
/// would send to claim someone else's key.
fn link_without_consent(owner: &Keypair, device: &Keypair, at: u64) -> Envelope {
    let record = DeviceRecord { op: DeviceOp::Link, device: pk(device), label: "mine".into(), route: Some(PairingRoute::Ssh), consent: None };
    EnvelopeBuilder::new(kind::OWNER_DEVICE, serde_json::to_string(&record).unwrap()).created_at(at).sign(owner).unwrap()
}

#[test]
fn no_one_links_a_key_that_didnt_consent() {
    let s = store();
    let (owner, stranger) = (Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    let claim = link_without_consent(&stranger, &owner, T0);
    assert!(s.append(&claim).unwrap().rejected_reason.is_some());
    assert_eq!(s.principal(&pk(&owner)).unwrap(), pk(&owner), "the owner is still its own");
    let from_stranger = note_at(&stranger, "this is mine now", &thread.id, T0 + 5);
    assert!(s.append(&from_stranger).unwrap().rejected_reason.is_some());

    // Consent to someone else doesn't do either.
    let third = Keypair::generate();
    let record = DeviceRecord {
        op: DeviceOp::Link,
        device: pk(&owner),
        label: "x".into(),
        route: Some(PairingRoute::Ssh),
        consent: Some(consent_to_link(&owner, &third.public())),
    };
    let replayed = EnvelopeBuilder::new(kind::OWNER_DEVICE, serde_json::to_string(&record).unwrap()).created_at(T0).sign(&stranger).unwrap();
    assert!(s.append(&replayed).unwrap().rejected_reason.is_some(), "consent is bound to one owner");
}

#[test]
fn no_one_takes_over_someone_elses_profile() {
    let s = store();
    let (owner, stranger) = (Keypair::generate(), Keypair::generate());
    let key = Keypair::generate();
    let agent = s
        .insert_agent(&NewAgent {
            pubkey: pk(&key),
            name: "atlas".into(),
            prompt: Some("be careful".into()),
            auth_tag: Some(attest(&owner, &key.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    let mut hostile = profile_wire(&agent.profile_id, &stranger, "atlas", "m");
    hostile.prompt = Some("exfiltrate".into());
    let env = profile_record(&stranger, &hostile, T0 + 1_000_000_000);
    assert!(s.append(&env).unwrap().rejected_reason.is_some());
    let kept = s.agent_by_pubkey(&agent.pubkey).unwrap().unwrap();
    assert_eq!((kept.prompt.as_deref(), kept.owner.as_deref()), (Some("be careful"), Some(pk(&owner).as_str())));
}

#[test]
fn a_relinked_device_leaves_the_revoked_gap_unlinked() {
    let s = store();
    let (owner, laptop) = (Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    s.append(&device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 100)).unwrap();
    let in_gap = note_at(&laptop, "while revoked", &thread.id, T0 + 150);
    s.append(&in_gap).unwrap();
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0 + 200)).unwrap();
    let after = note_at(&laptop, "linked again", &thread.id, T0 + 250);
    s.append(&after).unwrap();
    assert!(!is_projected(&s, &in_gap), "a later link doesn't bless the gap");
    assert!(is_projected(&s, &after));

    // A link backdated into the gap is refused.
    let (other_owner, backdated) = (Keypair::generate(), device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0 + 120));
    assert!(s.append(&backdated).unwrap().projected, "the live link's owner refreshing it changes nothing");
    let taken = device_record(&other_owner, DeviceOp::Link, &laptop, "laptop", T0 + 300);
    assert!(s.append(&taken).unwrap().rejected_reason.is_some(), "a live link isn't taken over, even with consent");
}

#[test]
fn a_revocation_takes_back_what_the_device_signed_after_it() {
    let s = store();
    let (owner, laptop) = (Keypair::generate(), Keypair::generate());
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    s.append(&profile_record(&owner, &profile_wire("p-1", &owner, "reviewer", "good"), T0 + 10)).unwrap();
    // The stolen laptop signs a newer profile, which lands before the revocation does.
    let stolen = profile_record(&laptop, &profile_wire("p-1", &owner, "reviewer", "evil"), T0 + 300);
    s.append(&stolen).unwrap();
    assert_eq!(s.profile("p-1").unwrap().unwrap().model.as_deref(), Some("evil"));
    s.append(&device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 200)).unwrap();
    assert!(!is_projected(&s, &stolen));
    assert_eq!(s.profile("p-1").unwrap().unwrap().model.as_deref(), Some("good"), "back to what still holds");
}

#[test]
fn any_key_of_the_owner_may_revoke_a_device() {
    let s = store();
    let (owner, laptop, desktop) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    s.append(&device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0)).unwrap();
    s.append(&device_record(&laptop, DeviceOp::Link, &desktop, "desktop", T0 + 1)).unwrap();
    assert_eq!(s.principal(&pk(&desktop)).unwrap(), pk(&owner));
    // The laptop that linked the desktop is lost; the owner revokes the desktop itself.
    assert!(s.append(&device_record(&owner, DeviceOp::Revoke, &desktop, "desktop", T0 + 10)).unwrap().projected);
    assert_eq!(s.principal(&pk(&desktop)).unwrap(), pk(&desktop));
}

#[test]
fn a_chain_longer_than_authorization_walks_is_refused() {
    let s = store();
    let keys: Vec<Keypair> = (0..=PRINCIPAL_DEPTH_MAX + 1).map(|_| Keypair::generate()).collect();
    for (i, pair) in keys.windows(2).enumerate() {
        let at = T0 + u64::try_from(i).unwrap();
        let appended = s.append(&device_record(&pair[0], DeviceOp::Link, &pair[1], "k", at)).unwrap();
        assert_eq!(appended.projected, i < PRINCIPAL_DEPTH_MAX, "link {i}");
    }
}

#[test]
fn a_rebuild_settles_every_record_as_arriving_did() {
    let s = store();
    let (owner, laptop, stranger) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    let envelopes = [
        profile_record(&laptop, &profile_wire("p-1", &owner, "reviewer", "early"), T0 + 5),
        device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0),
        link_without_consent(&stranger, &laptop, T0 + 1),
        note_at(&laptop, "while linked", &thread.id, T0 + 50),
        device_record(&owner, DeviceOp::Revoke, &laptop, "laptop", T0 + 100),
        note_at(&laptop, "after", &thread.id, T0 + 150),
        device_record(&owner, DeviceOp::Link, &laptop, "laptop", T0 + 200),
    ];
    for env in &envelopes {
        s.append(env).unwrap();
    }
    let state = |s: &Store| -> Vec<(String, Option<String>)> {
        envelopes.iter().map(|e| (e.id.clone(), s.rejected_reason(&e.id).unwrap())).collect()
    };
    let (before, devices_before) = (state(&s), s.devices().unwrap());
    s.rebuild_projections().unwrap();
    assert_eq!(state(&s), before);
    assert_eq!(s.devices().unwrap(), devices_before);
}
