//! `axel team …`: teams of agents (RFD 0005).
//!
//! A team is the owner's: every change is signed with the owner key and
//! appended as a kind-123 record, so it reaches the owner's other devices.
//! Members are named by agent and stored as profile ids.  Axel stores,
//! shows and installs what a team says; it never routes by role.

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_store::{EnvelopeBuilder, NewTeam, Store, Team, TeamEdit, TeamMember, TeamReach, TeamWire, kind};

use super::Ctx;
use super::text::pad;
use crate::cli::{TeamCommand, TeamReachArg};

/// The most roster text one system prompt carries.  A full team of 32 with
/// one-line roles is about 8 KB; 16 KiB covers a member of two full teams
/// and still caps a runaway.
const ROSTER_BYTES_MAX: usize = 16 * 1024;

pub fn run(cmd: TeamCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    match cmd {
        TeamCommand::New { name, lead, members, skills, reach } => new_team(&ctx, &name, lead.as_deref(), &members, skills, reach),
        TeamCommand::Edit { name, add, remove, lead, skills, reach } => {
            edit_team(&ctx, &name, EditArgs { add, remove, lead, skills, reach })
        }
        TeamCommand::Show { name } => show(&ctx, &name),
        TeamCommand::Ls => list(&ctx),
        TeamCommand::Rm { name } => remove(&ctx, &name),
    }
}

/// `axel team edit`'s flags, by agent name.
struct EditArgs {
    add: Vec<String>,
    remove: Vec<String>,
    lead: Option<String>,
    skills: Vec<String>,
    reach: Option<TeamReachArg>,
}

/// Teams are the owner's to change, like agents.
fn refuse_agents() -> Result<()> {
    if super::policy::caller_agent().is_some() {
        bail!("{}", t!("agents can't change teams; ask the owner to run this"));
    }
    Ok(())
}

fn new_team(ctx: &Ctx, name: &str, lead: Option<&str>, members: &[String], skills: Vec<String>, reach: TeamReachArg) -> Result<()> {
    refuse_agents()?;
    check_sources(&skills)?;
    let members = members.iter().map(|m| parse_member(ctx, m)).collect::<Result<Vec<_>>>()?;
    let lead = lead.map(|l| profile_id(ctx, l)).transpose()?;
    let owner = ctx.owner()?.public().to_base58();
    let team = ctx.store.create_team(&NewTeam { owner, name: name.to_string(), members, skills, reach: reach.into(), lead })?;
    publish(ctx, &team)?;
    print_team(ctx, &team);
    Ok(())
}

fn edit_team(ctx: &Ctx, name: &str, args: EditArgs) -> Result<()> {
    refuse_agents()?;
    let team = team_named(ctx, name)?;
    check_sources(&args.skills)?;
    let edit = TeamEdit {
        add: args.add.iter().map(|m| parse_member(ctx, m)).collect::<Result<_>>()?,
        remove: args.remove.iter().map(|m| profile_id(ctx, m)).collect::<Result<_>>()?,
        lead: args.lead.as_deref().map(|l| profile_id(ctx, l)).transpose()?,
        skills: (!args.skills.is_empty()).then_some(args.skills),
        reach: args.reach.map(Into::into),
    };
    let team = ctx.store.edit_team(&team.team_id, &edit)?;
    publish(ctx, &team)?;
    print_team(ctx, &team);
    Ok(())
}

fn remove(ctx: &Ctx, name: &str) -> Result<()> {
    refuse_agents()?;
    let team = team_named(ctx, name)?;
    let team = ctx.store.archive_team(&team.team_id)?;
    publish(ctx, &team)?;
    println!("{}", t!("archived {name}", name = team.name));
    Ok(())
}

fn show(ctx: &Ctx, name: &str) -> Result<()> {
    let team = team_named(ctx, name)?;
    print_team(ctx, &team);
    Ok(())
}

fn list(ctx: &Ctx) -> Result<()> {
    let teams = ctx.store.teams()?;
    if teams.is_empty() {
        println!("{}", t!("no teams yet; create one with `axel team new NAME --member AGENT`"));
        return Ok(());
    }
    for team in &teams {
        let lead = team.lead.as_deref().map(|l| profile_name(&ctx.store, l)).unwrap_or_else(|| "-".into());
        let count = t!("{n} members", n = team.members.len());
        println!("{} {} {} {}", pad(&team.name, 16), pad(&count, 12), pad(team.reach.as_str(), 6), lead);
    }
    Ok(())
}

/// Sign the team as stored with the owner key and append it, so it syncs.
pub(super) fn publish(ctx: &Ctx, team: &Team) -> Result<()> {
    assert!(team.updated_id.is_none(), "only an unsigned edit is published");
    let owner = ctx.owner()?;
    let content = serde_json::to_string(&TeamWire::of(team))?;
    let at = u64::try_from(team.updated_at).context(t!("the clock is before 1970"))?;
    let envelope = EnvelopeBuilder::new(kind::TEAM, content)
        .tag(vec!["d".into(), team.team_id.clone()])
        .created_at(at)
        .sign(&owner)?;
    let appended = ctx.store.append(&envelope)?;
    if let Some(reason) = appended.rejected_reason {
        bail!("{}", t!("the team record was refused: {reason}", reason = reason));
    }
    debug_assert!(ctx.store.team(&team.team_id)?.is_some_and(|t| t.updated_id.as_deref() == Some(envelope.id.as_str())));
    Ok(())
}

fn team_named(ctx: &Ctx, name: &str) -> Result<Team> {
    ctx.store.team_by_name(name)?.with_context(|| t!("no team named {name}", name = format!("{name:?}")))
}

/// The profile of the agent people call `name`.
fn profile_id(ctx: &Ctx, name: &str) -> Result<String> {
    let name = name.trim().trim_start_matches('@');
    let profile = ctx.store.profile_by_name(name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
    Ok(profile.profile_id)
}

/// `AGENT[:ROLE]`.
fn parse_member(ctx: &Ctx, spec: &str) -> Result<TeamMember> {
    let (name, role) = spec.split_once(':').unwrap_or((spec, ""));
    Ok(TeamMember { profile_id: profile_id(ctx, name)?, role: role.trim().to_string() })
}

/// Refuse a source `AXEL.md` couldn't read either, before anything is saved.
fn check_sources(skills: &[String]) -> Result<()> {
    for source in skills {
        if let Err(e) = axel_config::SkillSource::parse(source, None) {
            bail!("{}", t!("skill source {source}: {error}", source = source, error = e));
        }
    }
    Ok(())
}

/// A profile's name, or its id when this replica doesn't have it.
fn profile_name(store: &Store, profile_id: &str) -> String {
    // A failed read shows the id: the line still says who it is.
    store.profile(profile_id).ok().flatten().map_or_else(|| profile_id.to_string(), |p| p.name)
}

/// What a team's rule means, in a few words.
pub fn reach_rule_label(reach: TeamReach) -> String {
    match reach {
        TeamReach::Team => t!("members reach each other").into(),
        TeamReach::Lead => t!("members reach the lead; the lead reaches every member").into(),
        TeamReach::None => t!("no team rule").into(),
    }
}

fn print_team(ctx: &Ctx, team: &Team) {
    let lead = team.lead.as_deref().map(|l| profile_name(&ctx.store, l)).unwrap_or_else(|| "-".into());
    println!("{}  {}", team.name, team.team_id);
    println!("  {}", t!("reach   {who}", who = format!("{} ({})", team.reach, reach_rule_label(team.reach))));
    println!("  {}", t!("lead    {name}", name = lead));
    println!("  {}", t!("members"));
    for member in &team.members {
        println!("    {} {}", pad(&profile_name(&ctx.store, &member.profile_id), 20), member.role);
    }
    let skills = if team.skills.is_empty() { "-".to_string() } else { team.skills.join(", ") };
    println!("  {}", t!("skills  {list}", list = skills));
}

/// Skill sources of every live team `profile_id` belongs to, in team
/// order, each once.
pub fn skill_sources(store: &Store, profile_id: &str) -> Result<Vec<String>> {
    let mut sources: Vec<String> = Vec::new();
    for team in store.teams_of_profile(profile_id)? {
        debug_assert!(team.archived_at.is_none(), "an archived team's skills don't apply");
        for source in team.skills {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    Ok(sources)
}

/// The roster written into a member's appended system prompt: each of its
/// teams, every member's name and role, and the team's rule.  English: a
/// model reads it.  Empty for an agent in no team.
pub fn roster(store: &Store, profile_id: &str) -> Result<String> {
    let teams = store.teams_of_profile(profile_id)?;
    if teams.is_empty() {
        return Ok(String::new());
    }
    let mut s = String::from("\n\n# Your teams\n\nAxel shows these roles; it never acts on them. Delegate by messaging members.");
    for team in &teams {
        let section = roster_section(store, team, profile_id);
        if s.len() + section.len() > ROSTER_BYTES_MAX {
            eprintln!("axel: {}", t!("team {name} left out of the roster: it passes {max} bytes", name = team.name, max = ROSTER_BYTES_MAX));
            continue;
        }
        s.push_str(&section);
    }
    Ok(s)
}

fn roster_section(store: &Store, team: &Team, me: &str) -> String {
    assert!(team.has_member(me), "a roster is only written for a member");
    let mut s = format!("\n\n## Team {}\n", team.name);
    for member in &team.members {
        let name = profile_name(store, &member.profile_id);
        let mut tags: Vec<&str> = Vec::new();
        if team.lead.as_deref() == Some(member.profile_id.as_str()) {
            tags.push("lead");
        }
        if member.profile_id == me {
            tags.push("you");
        }
        let tags = if tags.is_empty() { String::new() } else { format!(" ({})", tags.join(", ")) };
        let role = if member.role.is_empty() { String::new() } else { format!(": {}", member.role) };
        s.push_str(&format!("\n- {name}{tags}{role}"));
    }
    let rule = match team.reach {
        TeamReach::Team => "\n\nMembers may reach each other.",
        TeamReach::Lead => "\n\nMembers may reach the lead alone; the lead may reach every member.",
        TeamReach::None => "",
    };
    s.push_str(rule);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axel_identity::{Keypair, attest};
    use axel_store::NewAgent;

    fn agent(store: &Store, owner: &Keypair, name: &str) -> String {
        let key = Keypair::generate();
        let spec = NewAgent {
            pubkey: key.public().to_base58(),
            name: name.into(),
            auth_tag: Some(attest(owner, &key.public(), "").unwrap().to_tag()),
            ..Default::default()
        };
        store.insert_agent(&spec).unwrap().profile_id
    }

    #[test]
    fn the_roster_names_every_member_its_role_and_the_rule() {
        let store = Store::open_memory().unwrap();
        let owner = Keypair::generate();
        let (lead, scout, loner) = (agent(&store, &owner, "coordinator"), agent(&store, &owner, "scout"), agent(&store, &owner, "loner"));
        let new = NewTeam {
            owner: owner.public().to_base58(),
            name: "core".into(),
            members: vec![TeamMember { profile_id: scout.clone(), role: "code reconnaissance".into() }],
            skills: vec!["./skills/core".into(), "github:acme/skills".into()],
            reach: TeamReach::Lead,
            lead: Some(lead.clone()),
        };
        let team = store.create_team(&new).unwrap();

        let text = roster(&store, &scout).unwrap();
        assert!(text.contains("## Team core"), "{text}");
        assert!(text.contains("- scout (you): code reconnaissance"), "{text}");
        assert!(text.contains("- coordinator (lead)"), "{text}");
        assert!(text.contains("the lead may reach every member"), "{text}");
        assert_eq!(roster(&store, &loner).unwrap(), "", "no team, no roster");
        assert_eq!(skill_sources(&store, &lead).unwrap(), new.skills);

        store.archive_team(&team.team_id).unwrap();
        assert_eq!(roster(&store, &scout).unwrap(), "", "an archived team writes no roster");
        assert!(skill_sources(&store, &scout).unwrap().is_empty(), "nor installs skills");
    }
}
