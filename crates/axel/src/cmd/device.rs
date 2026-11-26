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
    Agent, AnnounceWire, DeviceOp, DeviceRecord, Environment, NewAgent, PairingRoute, ProfileWire, kind,
};

use super::Ctx;

/// The most owner records one publish signs.  Every profile and instance an
/// owner has, many times over: a publish that has more is a bug, not a load.
const PUBLISH_RECORDS_MAX: usize = 10_000;

/// `axel device link LABEL [--ssh TARGET] [--route ROUTE] [--code CODE]`.
pub fn link(label: &str, ssh_target: Option<&str>, route: PairingRoute, code: Option<&str>, install: bool) -> Result<()> {
    super::host::add(label, ssh_target, install)?;
    let ctx = Ctx::open()?;
    let host = ctx.store.host(label)?.with_context(|| t!("no host named {name}", name = label))?;
    let device_key = host.host_pubkey.clone().context(t!("the host didn't say its key"))?;
    let owner = ctx.owner()?;
    let device: PublicKey = device_key.parse().context(t!("the host's key isn't a public key"))?;
    if device == owner.public() {
        bail!("{}", t!("{label} has this machine's own key: it is this machine", label = label));
    }
    check_route(route, &owner.public(), &device, code, label)?;
    let record = DeviceRecord { op: DeviceOp::Link, device: device_key.clone(), label: label.into(), route: Some(route) };
    ctx.store.post(&sign_record(&owner, &record)?).context(t!("recording the link"))?;
    publish(&ctx)?;
    let report = super::host::sync_host(&ctx, &host, false)?;
    println!(
        "{}",
        t!("{label} is linked ({route}): its key {key} now speaks for you. Synced {pushed} out, {pulled} in.",
            label = label, route = route, key = device_key, pushed = report.pushed, pulled = report.pulled)
    );
    Ok(())
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
    let device = ctx.store.device_by_label(label)?.with_context(|| t!("no device named {name}", name = label))?;
    let owner = ctx.owner()?;
    if device.owner.as_deref() != Some(owner.public().to_base58().as_str()) {
        bail!("{}", t!("{label} wasn't linked from this machine's key; revoke it from the machine that linked it", label = label));
    }
    let record = DeviceRecord { op: DeviceOp::Revoke, device: device.pubkey.clone(), label: label.into(), route: None };
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
        let record = DeviceRecord { op: DeviceOp::Label, device: me.clone(), label, route: None };
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
