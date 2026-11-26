//! Teams (RFD 0005): the kind-123 projection, local edits, the limits, and
//! how a team's reach rule combines with each member's own policy.

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

/// An agent of `owner` named `name`; returns its instance key and profile id.
fn agent(s: &Store, owner: &Keypair, name: &str) -> (Keypair, String) {
    let key = Keypair::generate();
    let agent = s
        .insert_agent(&NewAgent {
            pubkey: pk(&key),
            name: name.into(),
            auth_tag: Some(attest(owner, &key.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    (key, agent.profile_id)
}

fn member(profile_id: &str, role: &str) -> TeamMember {
    TeamMember { profile_id: profile_id.into(), role: role.into() }
}

fn wire(team_id: &str, owner: &Keypair, name: &str, members: Vec<TeamMember>, reach: TeamReach, lead: Option<&str>) -> TeamWire {
    TeamWire {
        team_id: team_id.into(),
        owner: pk(owner),
        name: name.into(),
        members,
        skills: vec![],
        policy: TeamPolicyWire { reach },
        lead: lead.map(str::to_string),
        archived: false,
    }
}

fn team_record(signer: &Keypair, wire: &TeamWire, at: u64) -> Envelope {
    EnvelopeBuilder::new(kind::TEAM, serde_json::to_string(wire).unwrap())
        .tag(vec!["d".into(), wire.team_id.clone()])
        .created_at(at)
        .sign(signer)
        .unwrap()
}

fn profile_record(owner: &Keypair, profile_id: &str, name: &str, at: u64) -> Envelope {
    let wire = ProfileWire {
        profile_id: profile_id.into(),
        owner: pk(owner),
        name: name.into(),
        harness: Harness::Claude,
        model: None,
        effort: None,
        skills: vec![],
        prompt: None,
        color: None,
        runtime_mode: RuntimeMode::Autopilot,
        archived: false,
    };
    EnvelopeBuilder::new(kind::AGENT_PROFILE, serde_json::to_string(&wire).unwrap())
        .tag(vec!["d".into(), profile_id.into()])
        .created_at(at)
        .sign(owner)
        .unwrap()
}

/// Publish a team as the CLI does: sign what is stored, append it.
fn publish(s: &Store, owner: &Keypair, team: &Team) -> Envelope {
    let env = team_record(owner, &TeamWire::of(team), u64::try_from(team.updated_at).unwrap());
    let appended = s.append(&env).unwrap();
    assert!(appended.projected, "{:?}", appended.rejected_reason);
    env
}

fn new_team(owner: &Keypair, name: &str, members: Vec<TeamMember>, reach: TeamReach, lead: Option<&str>) -> NewTeam {
    NewTeam { owner: pk(owner), name: name.into(), members, skills: vec![], reach, lead: lead.map(str::to_string) }
}

#[test]
fn a_team_is_unsigned_until_published_and_an_older_record_never_undoes_it() {
    let s = store();
    let owner = Keypair::generate();
    let (_, scout) = agent(&s, &owner, "scout");
    let (_, coordinator) = agent(&s, &owner, "coordinator");
    let team = s
        .create_team(&NewTeam { skills: vec!["./skills/core".into()], ..new_team(&owner, "core", vec![member(&scout, "code reconnaissance")], TeamReach::Lead, Some(&coordinator)) })
        .unwrap();
    assert_eq!(team.members.len(), 2, "the lead joins as a member");
    assert_eq!(s.unsigned_teams(&pk(&owner)).unwrap().len(), 1);

    let published = publish(&s, &owner, &team);
    let stored = s.team_by_name("core").unwrap().unwrap();
    assert_eq!(stored.updated_id.as_deref(), Some(published.id.as_str()));
    assert_eq!(stored.skills, vec!["./skills/core".to_string()]);
    assert!(s.unsigned_teams(&pk(&owner)).unwrap().is_empty());

    // An older record from another device arrives after: it doesn't win.
    let stale = team_record(&owner, &wire(&team.team_id, &owner, "stale", vec![member(&scout, "")], TeamReach::Team, None), T0);
    assert!(s.append(&stale).unwrap().projected, "an older record is valid, just not the latest");
    assert_eq!(s.team(&team.team_id).unwrap().unwrap().name, "core");

    // And a rebuild from the log agrees.
    s.rebuild_projections().unwrap();
    assert_eq!(s.team(&team.team_id).unwrap().unwrap(), stored);
}

#[test]
fn the_newest_record_wins_whatever_the_arrival_order() {
    let s = store();
    let owner = Keypair::generate();
    let (_, scout) = agent(&s, &owner, "scout");
    let old = team_record(&owner, &wire("t-1", &owner, "old", vec![member(&scout, "")], TeamReach::None, None), T0 + 10);
    let new = team_record(&owner, &wire("t-1", &owner, "new", vec![member(&scout, "scouts")], TeamReach::Team, None), T0 + 20);
    s.append(&new).unwrap();
    s.append(&old).unwrap();
    let team = s.team("t-1").unwrap().unwrap();
    assert_eq!(team.name, "new");
    assert_eq!(team.members, vec![member(&scout, "scouts")]);
    assert_eq!(team.updated_id.as_deref(), Some(new.id.as_str()));
}

#[test]
fn a_forged_team_record_is_refused() {
    let s = store();
    let (owner, stranger) = (Keypair::generate(), Keypair::generate());
    let (_, scout) = agent(&s, &owner, "scout");
    let real = team_record(&owner, &wire("t-1", &owner, "core", vec![member(&scout, "")], TeamReach::None, None), T0);
    s.append(&real).unwrap();

    // A stranger signs a newer record in the owner's name.
    let forged = team_record(&stranger, &wire("t-1", &owner, "core", vec![member(&scout, "")], TeamReach::Team, None), T0 + 10);
    let reason = s.append(&forged).unwrap().rejected_reason.expect("a forged team is refused");
    assert!(reason.contains("doesn't speak for"), "{reason}");
    assert_eq!(s.team("t-1").unwrap().unwrap().reach, TeamReach::None, "the real record stands");

    // Signed by the stranger as its own team, but naming the owner's profile.
    let poached = team_record(&stranger, &wire("t-2", &stranger, "mine", vec![member(&scout, "")], TeamReach::Team, None), T0);
    let reason = s.append(&poached).unwrap().rejected_reason.expect("another owner's profile is refused");
    assert!(reason.contains("is not"), "{reason}");
    assert!(s.team("t-2").unwrap().is_none());
}

#[test]
fn a_team_waits_for_a_member_it_names_and_is_revived_when_it_arrives() {
    let s = store();
    let owner = Keypair::generate();
    let early = team_record(&owner, &wire("t-1", &owner, "core", vec![member("p-later", "")], TeamReach::Team, None), T0 + 10);
    let reason = s.append(&early).unwrap().rejected_reason.expect("an unknown member is waited for");
    assert!(reason.contains("not received yet"), "{reason}");
    assert!(s.team("t-1").unwrap().is_none());

    s.append(&profile_record(&owner, "p-later", "scout", T0)).unwrap();
    assert!(s.rejected_reason(&early.id).unwrap().is_none(), "revived");
    assert!(s.team("t-1").unwrap().unwrap().has_member("p-later"));
}

#[test]
fn a_member_that_doesnt_exist_is_refused_here() {
    let s = store();
    let owner = Keypair::generate();
    let (_, scout) = agent(&s, &owner, "scout");
    let err = s.create_team(&new_team(&owner, "core", vec![member(&scout, ""), member("p-nobody", "")], TeamReach::Team, None));
    assert!(matches!(err, Err(Error::Invalid(ref m)) if m.contains("p-nobody")), "{err:?}");
    let team = s.create_team(&new_team(&owner, "core", vec![member(&scout, "")], TeamReach::Team, None)).unwrap();
    let err = s.edit_team(&team.team_id, &TeamEdit { add: vec![member("p-nobody", "")], ..Default::default() });
    assert!(matches!(err, Err(Error::Invalid(_))), "{err:?}");
    assert_eq!(s.team(&team.team_id).unwrap().unwrap().members.len(), 1, "a refused edit changes nothing");
}

#[test]
fn a_team_holds_at_most_the_member_limit() {
    let s = store();
    let owner = Keypair::generate();
    let profiles: Vec<String> = (0..=TEAM_MEMBERS_MAX).map(|i| agent(&s, &owner, &format!("a{i}")).1).collect();
    let members = |n: usize| profiles[..n].iter().map(|p| member(p, "")).collect::<Vec<_>>();

    let full = s.create_team(&new_team(&owner, "full", members(TEAM_MEMBERS_MAX), TeamReach::Team, None)).unwrap();
    assert_eq!(full.members.len(), TEAM_MEMBERS_MAX, "exactly the limit is fine");
    let over = s.create_team(&new_team(&owner, "over", members(TEAM_MEMBERS_MAX + 1), TeamReach::Team, None));
    assert!(matches!(over, Err(Error::Invalid(_))), "{over:?}");
    let grow = s.edit_team(&full.team_id, &TeamEdit { add: vec![member(&profiles[TEAM_MEMBERS_MAX], "")], ..Default::default() });
    assert!(matches!(grow, Err(Error::Invalid(_))), "{grow:?}");
    // A lead from outside would push it over too.
    let lead_over = s.create_team(&new_team(&owner, "lead-over", members(TEAM_MEMBERS_MAX), TeamReach::Lead, Some(&profiles[TEAM_MEMBERS_MAX])));
    assert!(matches!(lead_over, Err(Error::Invalid(_))), "{lead_over:?}");

    let from_afar = team_record(&owner, &wire("t-big", &owner, "big", members(TEAM_MEMBERS_MAX + 1), TeamReach::Team, None), T0);
    let reason = s.append(&from_afar).unwrap().rejected_reason.expect("an oversized record is refused");
    assert!(reason.contains("at most"), "{reason}");
}

#[test]
fn a_team_refuses_bad_names_twins_and_a_lead_rule_without_a_lead() {
    let s = store();
    let owner = Keypair::generate();
    let (_, scout) = agent(&s, &owner, "scout");
    let one = || vec![member(&scout, "")];
    for bad in ["", "two words", "@core", &"x".repeat(TEAM_NAME_BYTES_MAX + 1)] {
        assert!(matches!(s.create_team(&new_team(&owner, bad, one(), TeamReach::None, None)), Err(Error::Invalid(_))), "{bad:?}");
    }
    let twice = vec![member(&scout, "a"), member(&scout, "b")];
    assert!(matches!(s.create_team(&new_team(&owner, "core", twice, TeamReach::None, None)), Err(Error::Invalid(_))));
    assert!(matches!(s.create_team(&new_team(&owner, "core", one(), TeamReach::Lead, None)), Err(Error::Invalid(_))));
    let long_role = vec![member(&scout, &"r".repeat(TEAM_ROLE_BYTES_MAX + 1))];
    assert!(matches!(s.create_team(&new_team(&owner, "core", long_role, TeamReach::None, None)), Err(Error::Invalid(_))));

    s.create_team(&new_team(&owner, "core", one(), TeamReach::None, None)).unwrap();
    assert!(matches!(s.create_team(&new_team(&owner, "core", one(), TeamReach::None, None)), Err(Error::Conflict(_))));
}

#[test]
fn an_edit_adds_removes_and_keeps_a_lead() {
    let s = store();
    let owner = Keypair::generate();
    let (_, lead) = agent(&s, &owner, "coordinator");
    let (_, scout) = agent(&s, &owner, "scout");
    let (_, reviewer) = agent(&s, &owner, "reviewer");
    let team = s.create_team(&new_team(&owner, "core", vec![member(&scout, "")], TeamReach::Lead, Some(&lead))).unwrap();
    publish(&s, &owner, &team);

    let edited = s
        .edit_team(&team.team_id, &TeamEdit { add: vec![member(&reviewer, "reviews"), member(&scout, "scouts")], ..Default::default() })
        .unwrap();
    assert_eq!(edited.members, vec![member(&scout, "scouts"), member(&lead, ""), member(&reviewer, "reviews")]);
    assert!(edited.updated_at > team.updated_at && edited.updated_id.is_none(), "unsigned and newer");

    let no_lead = s.edit_team(&team.team_id, &TeamEdit { remove: vec![lead.clone()], ..Default::default() });
    assert!(matches!(no_lead, Err(Error::Invalid(_))), "the lead goes only with a successor");
    let handed_over = s.edit_team(&team.team_id, &TeamEdit { remove: vec![lead.clone()], lead: Some(reviewer.clone()), ..Default::default() }).unwrap();
    assert_eq!(handed_over.lead.as_deref(), Some(reviewer.as_str()));
    assert!(!handed_over.has_member(&lead));
    publish(&s, &owner, &handed_over);
    assert_eq!(s.teams_of_profile(&reviewer).unwrap().len(), 1);
    assert!(s.teams_of_profile(&lead).unwrap().is_empty());
}

#[test]
fn under_team_a_member_reaches_the_others_and_no_one_outside() {
    let s = store();
    let owner = Keypair::generate();
    let (alice, alice_p) = agent(&s, &owner, "alice");
    let (bob, bob_p) = agent(&s, &owner, "bob");
    let (carol, _) = agent(&s, &owner, "carol");
    assert!(!s.may_reach(&pk(&alice), &pk(&bob)).unwrap(), "nobody, before the team");
    s.create_team(&new_team(&owner, "pair", vec![member(&alice_p, ""), member(&bob_p, "")], TeamReach::Team, None)).unwrap();

    assert!(s.may_reach(&pk(&alice), &pk(&bob)).unwrap());
    assert!(s.may_reach(&pk(&bob), &pk(&alice)).unwrap());
    assert!(!s.may_reach(&pk(&alice), &pk(&carol)).unwrap(), "carol is no member");
    assert!(!s.may_reach(&pk(&carol), &pk(&alice)).unwrap(), "nor reaches in");

    // The summon check honours it too.
    let thread = s.create_thread(&owner, "t", "intent", None).unwrap();
    let session = s.start_session(&alice, &session_of(&thread.id, &alice)).unwrap();
    assert!(matches!(s.check_summon(Some(&session.id), &pk(&bob), 0).unwrap(), SummonCheck::Allowed { .. }));
    assert!(matches!(
        s.check_summon(Some(&session.id), &pk(&carol), 0).unwrap(),
        SummonCheck::Refused(Refusal::NotReachable { .. })
    ));
}

#[test]
fn under_lead_members_reach_the_lead_alone_and_the_lead_reaches_every_member() {
    let s = store();
    let owner = Keypair::generate();
    let (coordinator, coordinator_p) = agent(&s, &owner, "coordinator");
    let (scout, scout_p) = agent(&s, &owner, "scout");
    let (reviewer, reviewer_p) = agent(&s, &owner, "reviewer");
    let (outsider, _) = agent(&s, &owner, "outsider");
    let members = vec![member(&scout_p, "scouts"), member(&reviewer_p, "reviews")];
    s.create_team(&new_team(&owner, "core", members, TeamReach::Lead, Some(&coordinator_p))).unwrap();

    assert!(s.may_reach(&pk(&scout), &pk(&coordinator)).unwrap(), "a member reaches the lead");
    assert!(!s.may_reach(&pk(&scout), &pk(&reviewer)).unwrap(), "but not another member");
    assert!(s.may_reach(&pk(&coordinator), &pk(&scout)).unwrap(), "the lead reaches every member");
    assert!(s.may_reach(&pk(&coordinator), &pk(&reviewer)).unwrap());
    assert!(!s.may_reach(&pk(&coordinator), &pk(&outsider)).unwrap(), "and no one outside");

    let reach = s.effective_reach(&pk(&scout)).unwrap();
    assert_eq!(reach.teams.len(), 1);
    assert_eq!(reach.teams[0].team_name, "core");
    assert_eq!(reach.teams[0].profiles, vec![coordinator_p]);
}

#[test]
fn the_stricter_of_own_and_team_reach_wins_and_named_agents_come_on_top() {
    let s = store();
    let owner = Keypair::generate();
    let (alice, alice_p) = agent(&s, &owner, "alice");
    let (bob, bob_p) = agent(&s, &owner, "bob");
    let (carol, _) = agent(&s, &owner, "carol");
    let (dave, _) = agent(&s, &owner, "dave");
    s.create_team(&new_team(&owner, "pair", vec![member(&alice_p, ""), member(&bob_p, "")], TeamReach::Team, None)).unwrap();

    // Anyone, alone, would reach carol; the team's rule is stricter.
    s.set_agent_policy(&pk(&alice), &AgentPolicy { reach: Reach::Anyone, ..Default::default() }).unwrap();
    assert!(!s.may_reach(&pk(&alice), &pk(&carol)).unwrap());
    assert!(s.may_reach(&pk(&alice), &pk(&bob)).unwrap());

    // An agent the owner named comes on top of either rule.
    let mut policy = AgentPolicy { reach: Reach::Team(vec![]), ..Default::default() };
    policy.reach.allow(&pk(&dave));
    s.set_agent_policy(&pk(&alice), &policy).unwrap();
    assert!(s.may_reach(&pk(&alice), &pk(&dave)).unwrap());
    assert!(s.may_reach(&pk(&alice), &pk(&bob)).unwrap());
    assert!(!s.may_reach(&pk(&alice), &pk(&carol)).unwrap());
}

#[test]
fn an_archived_team_no_longer_applies_its_policy_or_its_skills() {
    let s = store();
    let owner = Keypair::generate();
    let (alice, alice_p) = agent(&s, &owner, "alice");
    let (bob, bob_p) = agent(&s, &owner, "bob");
    let team = s
        .create_team(&NewTeam { skills: vec!["github:acme/skills".into()], ..new_team(&owner, "pair", vec![member(&alice_p, ""), member(&bob_p, "")], TeamReach::Team, None) })
        .unwrap();
    publish(&s, &owner, &team);
    assert!(s.may_reach(&pk(&alice), &pk(&bob)).unwrap());
    assert_eq!(s.teams_of_profile(&alice_p).unwrap().len(), 1, "its skills come from here at launch");

    let archived = s.archive_team(&team.team_id).unwrap();
    assert!(!s.may_reach(&pk(&alice), &pk(&bob)).unwrap(), "no rule any more");
    assert!(s.teams_of_profile(&alice_p).unwrap().is_empty(), "so no skills either");
    assert!(s.team_by_name("pair").unwrap().is_none());
    assert!(s.teams().unwrap().is_empty());
    assert_eq!(s.unsigned_teams(&pk(&owner)).unwrap().len(), 1, "the archive is signed on the next publish");
    assert!(matches!(s.edit_team(&team.team_id, &TeamEdit::default()), Err(Error::Invalid(_))));

    // Published, it archives the team on every device; replay agrees.
    publish(&s, &owner, &archived);
    s.rebuild_projections().unwrap();
    assert!(s.team(&team.team_id).unwrap().unwrap().archived_at.is_some());
    assert!(!s.may_reach(&pk(&alice), &pk(&bob)).unwrap());
}

#[test]
fn a_rule_from_a_newer_build_reads_as_no_rule() {
    let s = store();
    let owner = Keypair::generate();
    let (alice, alice_p) = agent(&s, &owner, "alice");
    let (bob, bob_p) = agent(&s, &owner, "bob");
    let mut value = serde_json::to_value(wire("t-1", &owner, "pair", vec![member(&alice_p, ""), member(&bob_p, "")], TeamReach::Team, None)).unwrap();
    value["policy"]["reach"] = "everyone-and-their-dog".into();
    let env = EnvelopeBuilder::new(kind::TEAM, value.to_string()).created_at(T0).sign(&owner).unwrap();
    assert!(s.append(&env).unwrap().projected);
    assert_eq!(s.team("t-1").unwrap().unwrap().reach, TeamReach::None);
    assert!(!s.may_reach(&pk(&alice), &pk(&bob)).unwrap(), "an unknown rule widens no one's reach");
}

fn session_of(thread_id: &str, agent: &Keypair) -> NewSession {
    NewSession {
        thread_id: thread_id.into(),
        agent_pubkey: pk(agent),
        environment_id: "env-local".into(),
        environment_label: "local".into(),
        project_repo_key: None,
        harness: Harness::Claude,
        runtime_mode: RuntimeMode::Autopilot,
        resumed_from: None,
        runtime: SessionRuntime::Pane,
    }
}
