//! `axel device`: one owner, many machines (RFD 0004).
//!
//! Each machine signs with its own key.  `axel device link` adopts another
//! machine's key as one of the owner's devices with a signed kind-120
//! record, after checking that the far end is the machine we meant.  From
//! then on the two sync every thread and every agent profile, and the other
//! machine's key speaks for the owner.  A machine that was a home of its own
//! keeps its threads and agents: they become the owner's (phase 7).

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_identity::{PublicKey, attest, code_matches, confirm_code};
use axel_inbox::EnvelopeBuilder;
use axel_store::{
    Agent, AgentProfile, AnnounceWire, DeviceOp, DeviceRecord, Environment, NewAgent, PairingRoute, ProfileWire, TeamEdit,
    TeamMember, kind,
};

use super::Ctx;

/// The most owner records one publish signs.  Every profile and instance an
/// owner has, many times over: a publish that has more is a bug, not a load.
const PUBLISH_RECORDS_MAX: usize = 10_000;

/// `axel device link LABEL [--ssh TARGET] [--route ROUTE] [--code CODE]`.
pub fn link(label: &str, ssh_target: Option<&str>, route: PairingRoute, code: Option<&str>, install: bool) -> Result<()> {
    let ctx = Ctx::open()?;
    // Reaching it only asks its key.  Our skills go to it, and it becomes a
    // host here, only once the route has proved it is the machine we meant
    // and it consented: a wrong code must leave nothing of ours behind.
    let reached = super::host::reach(label, ssh_target)?;
    let device_key = reached.host.host_pubkey.clone().expect("a reached host said its key");
    let owner = ctx.owner()?;
    let device: PublicKey = device_key.parse().context(t!("the host's key isn't a public key"))?;
    if device == owner.public() {
        bail!("{}", t!("{label} has this machine's own key: it is this machine", label = label));
    }
    check_route(route, &owner.public(), &device, code, label)?;
    let consent = super::host::ask_consent(&reached.host, &owner.public())?;
    super::host::adopt(&ctx, &reached, install)?;
    let host = ctx.store.host(label)?.with_context(|| t!("no host named {name}", name = label))?;
    assert_eq!(host.host_pubkey.as_deref(), Some(device_key.as_str()), "the host recorded is the one that consented");
    let record =
        DeviceRecord { op: DeviceOp::Link, device: device_key.clone(), label: label.into(), route: Some(route), consent: Some(consent) };
    ctx.store.post(&sign_record(&owner, &record)?).context(t!("recording the link"))?;
    publish(&ctx)?;
    let report = super::host::sync_host(&ctx, &host, false)?;
    println!(
        "{}",
        t!("{label} is linked ({route}): its key {key} now speaks for you. Synced {pushed} out, {pulled} in.",
            label = label, route = route, key = device_key, pushed = report.pushed, pulled = report.pulled)
    );
    // A home that had agents of its own may share names with ours (#40).
    let me = ctx.store.principal(&owner.public().to_base58())?;
    if !merge_identical(&ctx, &me, None)?.is_empty() {
        super::host::sync_host(&ctx, &host, false)?;
    }
    print_clashes(&ctx, &me, None)
}

/// Check that the route to `device` proves it is the machine we meant.
fn check_route(route: PairingRoute, owner: &PublicKey, device: &PublicKey, code: Option<&str>, label: &str) -> Result<()> {
    match route {
        // `axel host add` reached it with `BatchMode=yes`, so ssh refused an
        // unknown or changed host key: the key belongs to the host in `known_hosts`.
        PairingRoute::Ssh => Ok(()),
        PairingRoute::RexTailscale => {
            bail!("{}", t!("the rex_tailscale route needs Rex's caller identity, which `axel` can't read yet; use --route ssh or --route confirm"))
        }
        // An anonymous listener proves nothing about the far end on its own.
        PairingRoute::Wireguard | PairingRoute::Confirm => {
            let expected = confirm_code(owner, device);
            let typed = match code {
                Some(c) => c.to_string(),
                None => ask_code(label, owner)?,
            };
            if !code_matches(&expected, &typed) {
                bail!("{}", t!("the code doesn't match: {label} may not be the machine you meant. Nothing was linked.", label = label));
            }
            Ok(())
        }
    }
}

fn ask_code(label: &str, owner: &PublicKey) -> Result<String> {
    if !super::setup::interactive() {
        bail!("{}", t!("run `axel device code {owner}` on {label}, then pass what it prints with --code", owner = owner, label = label));
    }
    eprintln!("{}", t!("On {label}, run: axel device code {owner}", label = label, owner = owner));
    Ok(dialoguer::Input::<String>::new().with_prompt(t!("The code it prints")).interact_text()?)
}

/// `axel device code OWNER`: the code to compare when linking this machine.
pub fn code(owner: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let owner: PublicKey = owner.parse().context(t!("that isn't a public key"))?;
    println!("{}", confirm_code(&owner, &ctx.owner()?.public()));
    Ok(())
}

/// `axel device ls`.
pub fn list() -> Result<()> {
    let ctx = Ctx::open()?;
    let me = ctx.owner()?.public().to_base58();
    let principal = ctx.store.principal(&me)?;
    println!("{}", t!("this machine: {key}, speaking for {owner}", key = me, owner = principal));
    for d in ctx.store.devices()? {
        let state = match (&d.owner, d.revoked_at) {
            (None, _) => t!("label"),
            (Some(_), Some(_)) => t!("revoked"),
            (Some(_), None) => t!("linked"),
        };
        let route = d.route.map(|r| r.as_str()).unwrap_or("-");
        println!("{:<16} {:<8} {:<14} {}", d.label, state, route, d.pubkey);
    }
    Ok(())
}

/// `axel device revoke LABEL`.
pub fn revoke(label: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let owner = ctx.owner()?;
    let me = ctx.store.principal(&owner.public().to_base58())?;
    let device = ctx.store.device_by_label(label, &me)?.with_context(|| t!("no device named {name}", name = label))?;
    let record = DeviceRecord { op: DeviceOp::Revoke, device: device.pubkey.clone(), label: label.into(), route: None, consent: None };
    ctx.store.post(&sign_record(&owner, &record)?)?;
    // The other devices learn it on their next sync; this pushes it now.
    for host in ctx.store.hosts()? {
        if host.host_pubkey.as_deref() != Some(device.pubkey.as_str())
            && is_linked(&ctx, &host)?
            && let Err(e) = super::host::sync_host(&ctx, &host, false)
        {
            eprintln!("{}", t!("{host}: {error}", host = host.label, error = format!("{e:#}")));
        }
    }
    println!("{}", t!("{label} is revoked: nothing it signs from now on speaks for you", label = label));
    Ok(())
}

/// `axel device merge [NAME] [--keep PROFILE | --rename PROFILE NEW_NAME]`.
pub fn merge(name: Option<&str>, keep: Option<&str>, rename: Option<&[String]>) -> Result<()> {
    let ctx = Ctx::open()?;
    let me = ctx.store.principal(&ctx.owner()?.public().to_base58())?;
    match (name, keep, rename) {
        (Some(name), Some(keep), _) => {
            let clash = clash_named(&ctx, &me, name)?;
            let kept = pick(&clash, keep)?;
            for other in clash.iter().filter(|p| p.profile_id != kept.profile_id) {
                merge_into(&ctx, &me, other, kept)?;
            }
        }
        (Some(name), None, Some([profile, new_name])) => {
            let clash = clash_named(&ctx, &me, name)?;
            let renamed = pick(&clash, profile)?;
            let new_name = super::agent::valid_name(new_name)?;
            ctx.store.rename_profile(&renamed.profile_id, new_name)?;
            println!("{}", t!("{id} is now {name}", id = renamed.profile_id, name = new_name));
        }
        (only, None, None) => {
            merge_identical(&ctx, &me, only)?;
        }
        _ => unreachable!("clap requires NAME with --keep or --rename, and --rename takes two values"),
    }
    spread(&ctx)?;
    if ctx.store.name_clashes(&me)?.is_empty() {
        println!("{}", t!("no two agents of yours share a name"));
        return Ok(());
    }
    print_clashes(&ctx, &me, name)
}

/// Merge every agent profile that shares its name (`only` that name, when
/// given) and its definition (harness, model, prompt...) with an older one
/// of ours into the oldest.  Every device picks the same one, so two
/// merging at once agree.
fn merge_identical(ctx: &Ctx, me: &str, only: Option<&str>) -> Result<Vec<String>> {
    let only = only.map(|n| n.trim().trim_start_matches('@'));
    let mut merged = Vec::new();
    for clash in ctx.store.name_clashes(me)? {
        if only.is_some_and(|n| n != clash[0].name) {
            continue;
        }
        let (oldest, rest) = clash.split_first().expect("a clash has two or more profiles");
        for other in rest.iter().filter(|p| p.same_definition(oldest)) {
            merge_into(ctx, me, other, oldest)?;
            merged.push(other.profile_id.clone());
        }
    }
    Ok(merged)
}

/// Merge `from` into `into`: its instances and its team seats go to `into`.
fn merge_into(ctx: &Ctx, me: &str, from: &AgentProfile, into: &AgentProfile) -> Result<()> {
    assert_eq!(from.name, into.name, "only profiles that share a name merge");
    let teams = ctx.store.teams_of_profile(&from.profile_id)?;
    ctx.store.merge_profile(&from.profile_id, &into.profile_id, me)?;
    for team in teams {
        let role = team.members.iter().find(|m| m.profile_id == from.profile_id).map(|m| m.role.clone()).unwrap_or_default();
        let has_into = team.members.iter().any(|m| m.profile_id == into.profile_id);
        let edit = TeamEdit {
            add: if has_into { Vec::new() } else { vec![TeamMember { profile_id: into.profile_id.clone(), role }] },
            remove: vec![from.profile_id.clone()],
            lead: (team.lead.as_deref() == Some(from.profile_id.as_str())).then(|| into.profile_id.clone()),
            skills: None,
            reach: None,
        };
        let edited = ctx.store.edit_team(&team.team_id, &edit)?;
        super::team::publish(ctx, &edited)?;
    }
    println!("{}", t!("merged {from} into {into} ({name}): one agent now", from = from.profile_id, into = into.profile_id, name = into.name));
    Ok(())
}

/// The profiles of ours named `name`, two or more of them.
fn clash_named(ctx: &Ctx, me: &str, name: &str) -> Result<Vec<AgentProfile>> {
    let name = name.trim().trim_start_matches('@');
    ctx.store
        .name_clashes(me)?
        .into_iter()
        .find(|clash| clash[0].name == name)
        .with_context(|| t!("only one agent of yours is named {name}: nothing to settle", name = name))
}

/// The profile in `clash` whose id is or starts with `id`.
fn pick<'a>(clash: &'a [AgentProfile], id: &str) -> Result<&'a AgentProfile> {
    let id = id.trim();
    let mut found = clash.iter().filter(|p| !id.is_empty() && p.profile_id.starts_with(id));
    match (found.next(), found.next()) {
        (Some(profile), None) => Ok(profile),
        (Some(_), Some(_)) => bail!("{}", t!("{id} starts more than one of these profile ids; give more of it", id = id)),
        (None, _) => bail!("{}", t!("no agent named {name} has profile id {id}", name = clash[0].name, id = id)),
    }
}

/// Sign what changed and send it to every linked device now, as `revoke`
/// does; a device we can't reach gets it on its next sync.
fn spread(ctx: &Ctx) -> Result<()> {
    publish(ctx)?;
    for host in ctx.store.hosts()? {
        if is_linked(ctx, &host)?
            && let Err(e) = super::host::sync_host(ctx, &host, false)
        {
            eprintln!("{}", t!("{host}: {error}", host = host.label, error = format!("{e:#}")));
        }
    }
    Ok(())
}

/// The names still shared (just `name`, when given), each with its
/// profiles and the two ways to settle it.
fn print_clashes(ctx: &Ctx, me: &str, name: Option<&str>) -> Result<()> {
    for clash in ctx.store.name_clashes(me)? {
        let shared = &clash[0].name;
        if name.is_some_and(|n| n.trim().trim_start_matches('@') != shared) {
            continue;
        }
        println!("{}", t!("{n} agents of yours are named {name}:", n = clash.len(), name = shared));
        for profile in &clash {
            let runs = match &profile.model {
                Some(model) => format!("{}/{model}", profile.harness.as_str()),
                None => profile.harness.as_str().to_string(),
            };
            println!("  {:<30} {:<24} {}", profile.profile_id, runs, instance_places(ctx, &profile.profile_id)?.join(", "));
        }
        println!("{}", t!("  keep one:   axel device merge {name} --keep PROFILE", name = shared));
        println!("{}", t!("  or rename:  axel device merge {name} --rename PROFILE NEW_NAME", name = shared));
    }
    Ok(())
}

/// Where a profile's instances run, for people.
fn instance_places(ctx: &Ctx, profile_id: &str) -> Result<Vec<String>> {
    let mut places = Vec::new();
    for instance in ctx.store.profile_instances(profile_id)? {
        places.push(match &instance.device {
            None => t!("this machine").to_string(),
            Some(device) => ctx.store.device(device)?.map_or_else(|| device.clone(), |d| d.label),
        });
    }
    Ok(places)
}

/// Whether `host` is a live device of the owner this machine speaks for.
pub fn is_linked(ctx: &Ctx, host: &Environment) -> Result<bool> {
    let Some(key) = host.host_pubkey.as_deref() else { return Ok(false) };
    let me = ctx.store.principal(&ctx.owner()?.public().to_base58())?;
    Ok(key != me && ctx.store.principal(key)? == me)
}

fn sign_record(owner: &axel_identity::Keypair, record: &DeviceRecord) -> Result<axel_inbox::Envelope> {
    Ok(EnvelopeBuilder::new(kind::OWNER_DEVICE, serde_json::to_string(record)?)
        .tag(vec!["d".into(), record.device.clone()])
        .sign(owner)?)
}

/// Sign what this machine changed and its devices haven't seen: profiles
/// edited here, instances made here, and this machine's own label.  Sync
/// calls it first, so anything the CLI, the TUI or the MCP server changed
/// travels on the next sync.  Returns how many records it signed.
pub fn publish(ctx: &Ctx) -> Result<usize> {
    let key = ctx.owner()?;
    let me = key.public().to_base58();
    let principal = ctx.store.principal(&me)?;
    let mut signed = 0_usize;
    if ctx.store.device(&me)?.is_none() {
        let label = ctx.store.local_environment()?.label;
        let record = DeviceRecord { op: DeviceOp::Label, device: me.clone(), label, route: None, consent: None };
        ctx.store.post(&sign_record(&key, &record)?)?;
        signed += 1;
    }
    for profile in ctx.store.unsigned_profiles(&principal)?.into_iter().take(PUBLISH_RECORDS_MAX) {
        let at = u64::try_from(profile.updated_at).context("a profile edited before 1970")?;
        let wire = ProfileWire::of(&profile, &principal);
        let env = EnvelopeBuilder::new(kind::AGENT_PROFILE, serde_json::to_string(&wire)?)
            .tag(vec!["d".into(), profile.profile_id.clone()])
            .created_at(at)
            .sign(&key)?;
        ctx.store.post(&env)?;
        signed += 1;
    }
    for (instance, profile_id, auth) in ctx.store.unannounced_instances()?.into_iter().take(PUBLISH_RECORDS_MAX) {
        // Only instances that speak for this owner: a host also holds rows for
        // other owners' agents it ran jobs for, which aren't ours to announce.
        let Ok(tag) = axel_identity::AuthTag::from_tag(&auth) else { continue };
        if ctx.store.principal(&tag.owner.to_base58())? != principal {
            continue;
        }
        let wire = AnnounceWire { instance: instance.clone(), profile_id, auth };
        let env = EnvelopeBuilder::new(kind::AGENT_ANNOUNCE, serde_json::to_string(&wire)?)
            .tag(vec!["d".into(), instance])
            .sign(&key)?;
        ctx.store.post(&env)?;
        signed += 1;
    }
    Ok(signed)
}

/// This machine's instance of `profile_id`, made (key, attestation,
/// announcement) the first time the profile runs here.
pub fn local_instance(ctx: &Ctx, profile_id: &str) -> Result<Agent> {
    let existing = ctx.store.profile_instances(profile_id)?;
    for agent in existing.iter().filter(|a| a.device.is_none()) {
        if ctx.keystore.agent(&agent.pubkey.parse()?)?.is_some() {
            return Ok(agent.clone());
        }
    }
    let profile = ctx.store.profile(profile_id)?.with_context(|| t!("no agent profile {id}", id = profile_id))?;
    let me = ctx.store.principal(&ctx.owner()?.public().to_base58())?;
    let owner = profile.owner.as_deref().map(|o| ctx.store.principal(o)).transpose()?;
    if owner.as_deref() != Some(me.as_str()) {
        bail!("{}", t!("{name} is another owner's agent: it doesn't run as one of yours", name = profile.name));
    }
    if profile.archived_at.is_some() {
        bail!("{}", t!("{name} was removed", name = profile.name));
    }
    let key = axel_identity::Keypair::generate();
    ctx.keystore.put_agent(&key).context(t!("storing the agent key"))?;
    let tag = attest(&ctx.owner()?, &key.public(), "").context(t!("attesting the agent"))?;
    let agent = ctx.store.insert_agent(&NewAgent {
        pubkey: key.public().to_base58(),
        name: profile.name.clone(),
        harness: profile.harness.clone(),
        auth_tag: Some(tag.to_tag()),
        profile_id: Some(profile_id.to_string()),
        ..Default::default()
    })?;
    assert_eq!(agent.profile_id, profile_id, "an instance of the profile asked for");
    publish(ctx)?;
    Ok(agent)
}
