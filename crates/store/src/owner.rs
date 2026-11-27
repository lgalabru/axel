//! Owner records (RFD 0004): which machines belong to which owner (kind 120),
//! agent profiles (kind 122), the agent instances each device runs
//! (kind 121), teams of profiles (kind 123, RFD 0005) and schedules (kind
//! 124, projected by `crate::schedule`).  They are on no thread and travel to
//! every device of an owner.
//!
//! A machine signs with its own key.  Once its owner links it, that key
//! speaks for the owner: [`Principals::resolve`] walks device → owner, and
//! thread authorization compares the principals it finds, not raw keys.
//! A home merged into another (an owner key linked as a device) is the same
//! walk, one link longer.

use axel_identity::{AuthTag, PublicKey, verify_attestation};
use axel_inbox::Envelope;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) use crate::devices::Principals;
pub use crate::devices::{DeviceRecord, PRINCIPAL_DEPTH_MAX};
use crate::log::{EVENT_COLS, mark_applied, mark_rejected, row_envelope};
use crate::types::lenient;
use crate::{
    AgentColor, AgentProfile, Effort, Error, Harness, RuntimeMode,
    Store, Team, TeamMember, TeamReach, kind, now,
};

/// The most passes that revive owner records rejected for want of another
/// one (a profile before its signer's link).  Each pass that revives
/// anything can unlock at most one more level of the chain.
const REVIVE_PASSES_MAX: usize = PRINCIPAL_DEPTH_MAX + 2;

/// Kind-122 content: a whole profile.  The latest `created_at` per
/// `profile_id` wins, ties broken by envelope id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileWire {
    pub profile_id: String,
    pub owner: String,
    pub name: String,
    pub harness: Harness,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "lenient::or_none")]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default, deserialize_with = "lenient::or_none")]
    pub color: Option<AgentColor>,
    #[serde(default, deserialize_with = "lenient::or_default")]
    pub runtime_mode: RuntimeMode,
    #[serde(default)]
    pub archived: bool,
    /// The profile this one merged into: its instances become that one's.
    /// Left out when absent, so records that never merged keep their id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
}

/// The longest chain of merges followed (a merged into b, b into c, ...).
/// A merge never targets a profile already merged into it, so chains are
/// acyclic; owners merge a handful of profiles, ever.
pub const MERGE_CHAIN_MAX: usize = 8;

impl ProfileWire {
    /// The wire form of a stored profile, for its owner to sign.
    pub fn of(profile: &AgentProfile, owner: &str) -> ProfileWire {
        ProfileWire {
            profile_id: profile.profile_id.clone(),
            owner: owner.to_string(),
            name: profile.name.clone(),
            harness: profile.harness.clone(),
            model: profile.model.clone(),
            effort: profile.effort,
            skills: profile.skills.clone(),
            prompt: profile.prompt.clone(),
            color: profile.color,
            runtime_mode: profile.runtime_mode,
            archived: profile.archived_at.is_some(),
            merged_into: profile.merged_into.clone(),
        }
    }
}

/// The policy part of a kind-123 team.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPolicyWire {
    /// A rule this build doesn't know reads as `none`: members keep their
    /// own policy, and nobody's reach widens.
    #[serde(default, deserialize_with = "lenient::or_default")]
    pub reach: TeamReach,
}

/// Kind-123 content: a whole team.  The latest `created_at` per `team_id`
/// wins, ties broken by envelope id, as for profiles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamWire {
    pub team_id: String,
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub policy: TeamPolicyWire,
    #[serde(default)]
    pub lead: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

impl TeamWire {
    /// The wire form of a stored team, for its owner to sign.
    pub fn of(team: &Team) -> TeamWire {
        TeamWire {
            team_id: team.team_id.clone(),
            owner: team.owner.clone(),
            name: team.name.clone(),
            members: team.members.clone(),
            skills: team.skills.clone(),
            policy: TeamPolicyWire { reach: team.reach },
            lead: team.lead.clone(),
            archived: team.archived_at.is_some(),
        }
    }
}

/// Kind-121 content: an instance the signing device runs.  `auth` is the
/// instance's attestation, by that device (or by an owner key on it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnounceWire {
    pub instance: String,
    pub profile_id: String,
    pub auth: Vec<String>,
}

/// Project one owner record and settle it as applied or rejected, reviving
/// records that waited for it.  Returns whether it changed who may sign on
/// threads, in which case the caller owes every thread a replay: it does
/// that once per batch of appends, not once per record.
pub(crate) fn project(conn: &Connection, env: &Envelope) -> Result<bool, Error> {
    assert!(kind::is_owner_record(env.kind), "kind {} is no owner record", env.kind);
    match apply(conn, env)? {
        Err(reason) => {
            mark_rejected(conn, &env.id, &reason)?;
            Ok(false)
        }
        Ok(()) => {
            mark_applied(conn, &env.id)?;
            let mut authorization_changed = changes_authorization(env.kind);
            authorization_changed |= revive(conn)?;
            if env.kind == kind::OWNER_DEVICE {
                // A revocation can withdraw what a device signed after it, even
                // records that arrived before the revocation did.
                revalidate(conn)?;
            }
            Ok(authorization_changed)
        }
    }
}

/// Whether a record of `kind` changes who may sign on threads: a device link
/// or revocation, or an instance attestation authorization reads.
fn changes_authorization(kind: u32) -> bool {
    kind == kind::OWNER_DEVICE || kind == kind::AGENT_ANNOUNCE
}

/// Re-apply owner records rejected so far, until a pass revives nothing.
/// Returns whether a revived record changed who may sign on threads.
pub(crate) fn revive(conn: &Connection) -> Result<bool, Error> {
    let mut links_changed = false;
    for _ in 0..REVIVE_PASSES_MAX {
        let rejected: Vec<Envelope> = {
            let mut statement = conn.prepare_cached(&format!(
                "SELECT {EVENT_COLS} FROM events
                 WHERE kind BETWEEN 120 AND 129 AND thread_id IS NULL AND rejected_reason IS NOT NULL
                 ORDER BY seq ASC"
            ))?;
            let rows = statement.query_map([], row_envelope)?;
            rows.collect::<Result<_, _>>()?
        };
        let mut revived = 0_usize;
        for env in &rejected {
            if apply(conn, env)?.is_ok() {
                mark_applied(conn, &env.id)?;
                revived += 1;
                links_changed |= changes_authorization(env.kind);
            }
        }
        if revived == 0 {
            break;
        }
    }
    Ok(links_changed)
}

/// The most applied owner records one revalidation re-checks.  An owner's
/// every profile, team and instance many times over; past it, the rest wait
/// for the next link change or rebuild.
const REVALIDATE_RECORDS_MAX: i64 = 100_000;

/// Re-check every applied profile, team and announcement against the device
/// links as they stand now, and undo the ones that no longer hold.
pub(crate) fn revalidate(conn: &Connection) -> Result<(), Error> {
    let applied: Vec<Envelope> = {
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {EVENT_COLS} FROM events
             WHERE kind IN (121, 122, 123) AND thread_id IS NULL AND applied_at IS NOT NULL
             ORDER BY kind ASC, seq ASC LIMIT ?1"
        ))?;
        let rows = statement.query_map([REVALIDATE_RECORDS_MAX], row_envelope)?;
        rows.collect::<Result<_, _>>()?
    };
    for env in &applied {
        if let Some(reason) = authority(conn, env)? {
            mark_rejected(conn, &env.id, &reason)?;
            undo(conn, env)?;
        }
    }
    Ok(())
}

/// [`profile_authority`], [`team_authority`] or [`announce_authority`] for
/// a stored record.
fn authority(conn: &Connection, env: &Envelope) -> Result<Option<String>, Error> {
    match env.kind {
        kind::AGENT_ANNOUNCE => match parse::<AnnounceWire>(env) {
            Ok(wire) => announce_authority(conn, env, &wire),
            Err(reason) => Ok(Some(reason)),
        },
        kind::AGENT_PROFILE => match parse::<ProfileWire>(env) {
            Ok(wire) => profile_authority(conn, env, &wire),
            Err(reason) => Ok(Some(reason)),
        },
        kind::TEAM => match parse::<TeamWire>(env) {
            Ok(wire) => team_authority(conn, env, &wire),
            Err(reason) => Ok(Some(reason)),
        },
        other => unreachable!("kind {other} is not revalidated"),
    }
}

/// Take back what a record that no longer holds had set: the instance it
/// announced, or the profile or team it last wrote, which falls back to the
/// newest record that still holds.
fn undo(conn: &Connection, env: &Envelope) -> Result<(), Error> {
    match env.kind {
        kind::AGENT_ANNOUNCE => {
            if let Ok(wire) = parse::<AnnounceWire>(env) {
                conn.execute("DELETE FROM agents WHERE pubkey = ?1 AND device = ?2", params![wire.instance, env.from])?;
            }
        }
        kind::AGENT_PROFILE => {
            let set_by_it: Option<String> = conn
                .query_row("SELECT profile_id FROM agent_profiles WHERE updated_id = ?1", [&env.id], |r| r.get(0))
                .optional()?;
            if let Some(profile_id) = set_by_it {
                restore_profile(conn, &profile_id)?;
            }
        }
        kind::TEAM => {
            if let Ok(wire) = parse::<TeamWire>(env)
                && crate::team::read_team(conn, &wire.team_id)?.is_some_and(|t| t.updated_id.as_deref() == Some(env.id.as_str()))
            {
                restore_team(conn, &wire.team_id)?;
            }
        }
        other => unreachable!("kind {other} is not revalidated"),
    }
    Ok(())
}

/// The newest applied record of `kind` whose content `matches`.
fn newest_applied<T: serde::de::DeserializeOwned>(conn: &Connection, kind: u32, matches: impl Fn(&T) -> bool) -> Result<Option<(Envelope, T)>, Error> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE kind = ?1 AND applied_at IS NOT NULL ORDER BY created_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = statement.query_map(params![kind, REVALIDATE_RECORDS_MAX], row_envelope)?;
    for row in rows {
        let env = row?;
        if let Ok(wire) = parse::<T>(&env)
            && matches(&wire)
        {
            return Ok(Some((env, wire)));
        }
    }
    Ok(None)
}

fn restore_profile(conn: &Connection, profile_id: &str) -> Result<(), Error> {
    match newest_applied::<ProfileWire>(conn, kind::AGENT_PROFILE, |w| w.profile_id == profile_id)? {
        Some((env, wire)) => write_profile(conn, &wire, crate::sql_integer(env.created_at, "envelope created_at")?, &env.id),
        None => {
            // Nothing that holds ever set it: it goes, unless instances here
            // still use it, which keep it as it is.
            conn.execute(
                "DELETE FROM agent_profiles WHERE profile_id = ?1 AND NOT EXISTS (SELECT 1 FROM agents WHERE profile_id = ?1)",
                [profile_id],
            )?;
            Ok(())
        }
    }
}

fn restore_team(conn: &Connection, team_id: &str) -> Result<(), Error> {
    match newest_applied::<TeamWire>(conn, kind::TEAM, |w| w.team_id == team_id)? {
        Some((env, _)) => {
            // Re-applying the record that holds writes the team as it set it.
            conn.execute("UPDATE teams SET updated_at = 0, updated_id = NULL WHERE team_id = ?1", [team_id])?;
            apply_team(conn, &env).map(|_| ())
        }
        None => {
            conn.execute("UPDATE teams SET archived_at = COALESCE(archived_at, ?2) WHERE team_id = ?1", params![team_id, now()])?;
            Ok(())
        }
    }
}

/// [`apply`] for a full rebuild, which replays every thread afterwards.
pub(crate) fn apply_only(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    apply(conn, env)
}

/// Why an owner record can't take effect yet, or `Ok` once it has.
fn apply(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    match env.kind {
        kind::OWNER_DEVICE => crate::devices::apply(conn, env),
        kind::AGENT_ANNOUNCE => apply_announce(conn, env),
        kind::AGENT_PROFILE => apply_profile(conn, env),
        kind::TEAM => apply_team(conn, env),
        kind::SCHEDULE => Ok(match crate::schedule::project_schedule(conn, env)? {
            Some(reason) => Err(reason),
            None => Ok(()),
        }),
        other => Ok(Err(format!("owner record kind {other} is not known to this build"))),
    }
}

fn parse<T: serde::de::DeserializeOwned>(env: &Envelope) -> Result<T, String> {
    serde_json::from_str(&env.content).map_err(|e| format!("malformed content: {e}"))
}

fn apply_profile(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: ProfileWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(reason) = profile_authority(conn, env, &wire)? {
        return Ok(Err(reason));
    }
    let at = crate::sql_integer(env.created_at, "envelope created_at")?;
    let current: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT updated_at, updated_id FROM agent_profiles WHERE profile_id = ?1",
            [&wire.profile_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let is_newer = current
        .as_ref()
        .is_none_or(|(updated_at, updated_id)| (at, env.id.as_str()) > (*updated_at, updated_id.as_deref().unwrap_or("")));
    if is_newer {
        write_profile(conn, &wire, at, &env.id)?;
    }
    Ok(Ok(()))
}

/// Why `env` may not set `wire`: its signer must speak for the profile's
/// owner, and a profile's owner never changes, so no one can take over an
/// agent of someone else's by naming its profile id.
fn profile_authority(conn: &Connection, env: &Envelope, wire: &ProfileWire) -> Result<Option<String>, Error> {
    if wire.name.trim().is_empty() || wire.profile_id.is_empty() {
        return Ok(Some("a profile needs an id and a name".into()));
    }
    let principals = Principals::load(conn)?;
    let owner = principals.resolve(&wire.owner, env.created_at);
    if principals.resolve(&env.from, env.created_at) != owner {
        return Ok(Some(format!("signer {} doesn't speak for owner {}", env.from, wire.owner)));
    }
    let stored: Option<Option<String>> = conn
        .query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [&wire.profile_id], |r| r.get(0))
        .optional()?;
    let ownership = match stored {
        None => None,
        Some(None) => Some(format!("profile {} has no known owner, so no one can claim it", wire.profile_id)),
        Some(Some(stored)) if principals.resolve(&stored, env.created_at) == owner => None,
        Some(Some(stored)) => Some(format!("profile {} belongs to {stored}", wire.profile_id)),
    };
    if ownership.is_some() {
        return Ok(ownership);
    }
    match &wire.merged_into {
        None => Ok(None),
        Some(target) => merge_authority(conn, &principals, owner, &wire.profile_id, target, env.created_at),
    }
}

/// Why `profile_id` may not merge into `target`: the target must be another
/// profile of the same owner, and must not lead back to `profile_id`.
fn merge_authority(
    conn: &Connection,
    principals: &Principals,
    owner: &str,
    profile_id: &str,
    target: &str,
    at: u64,
) -> Result<Option<String>, Error> {
    if target == profile_id {
        return Ok(Some(format!("profile {profile_id} can't merge into itself")));
    }
    let target_owner: Option<Option<String>> = conn
        .query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [target], |r| r.get(0))
        .optional()?;
    let Some(target_owner) = target_owner else {
        return Ok(Some(format!("profile {target} not received yet")));
    };
    if !target_owner.is_some_and(|o| principals.resolve(&o, at) == owner) {
        return Ok(Some(format!("profile {target} belongs to another owner")));
    }
    let mut next = Some(target.to_string());
    for _ in 0..MERGE_CHAIN_MAX {
        let Some(id) = next else { return Ok(None) };
        if id == profile_id {
            return Ok(Some(format!("profile {target} is merged into {profile_id}: the merge would loop")));
        }
        next = merged_into(conn, &id)?;
    }
    Ok(Some(format!("profile {target} is at the end of more than {MERGE_CHAIN_MAX} merges")))
}

/// The profile `profile_id` was merged into, if any.
fn merged_into(conn: &Connection, profile_id: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row("SELECT merged_into FROM agent_profiles WHERE profile_id = ?1", [profile_id], |r| r.get(0))
        .optional()?
        .flatten())
}

/// The profile instances of `profile_id` belong to: itself, or the end of
/// the merges it went through.
pub(crate) fn surviving_profile(conn: &Connection, profile_id: &str) -> Result<String, Error> {
    let mut current = profile_id.to_string();
    for _ in 0..MERGE_CHAIN_MAX {
        match merged_into(conn, &current)? {
            Some(next) => current = next,
            None => return Ok(current),
        }
    }
    // Merge authority refuses longer chains; one here came from elsewhere.
    Err(Error::Corrupt(format!("profile {profile_id} is at the end of more than {MERGE_CHAIN_MAX} merges")))
}

/// Move what belongs to `from` over to `into`, which it merged into: every
/// instance, and this machine's own ones are announced again, under `into`.
pub(crate) fn move_instances(conn: &Connection, from: &str, into: &str) -> Result<(), Error> {
    assert_ne!(from, into, "a profile never merges into itself");
    conn.execute(
        "UPDATE agents SET profile_id = ?2, announced_id = CASE WHEN device IS NULL THEN NULL ELSE announced_id END
         WHERE profile_id = ?1",
        params![from, into],
    )?;
    Ok(())
}

/// Store `wire` as the profile.  A merge also moves the instances over; a
/// record that later stops holding (a revoked device's) restores the
/// profile but leaves the instances where the merge put them, under a live
/// profile of the same owner.
fn write_profile(conn: &Connection, wire: &ProfileWire, at: i64, event_id: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO agent_profiles (profile_id, owner, name, harness, model, effort, skills_json, prompt, color, runtime_mode,
                                     created_at, updated_at, updated_id, archived_at, merged_into)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13, ?14)
         ON CONFLICT(profile_id) DO UPDATE SET owner = excluded.owner, name = excluded.name, harness = excluded.harness,
             model = excluded.model, effort = excluded.effort, skills_json = excluded.skills_json, prompt = excluded.prompt,
             color = excluded.color, runtime_mode = excluded.runtime_mode, updated_at = excluded.updated_at,
             updated_id = excluded.updated_id, archived_at = excluded.archived_at, merged_into = excluded.merged_into",
        params![
            wire.profile_id,
            wire.owner,
            wire.name,
            wire.harness.as_str(),
            wire.model,
            wire.effort.map(|e| e.as_str()),
            serde_json::to_string(&wire.skills)?,
            wire.prompt,
            wire.color.map(|c| c.as_str()),
            wire.runtime_mode.as_str(),
            at,
            event_id,
            wire.archived.then_some(at),
            wire.merged_into,
        ],
    )?;
    if let Some(into) = &wire.merged_into {
        move_instances(conn, &wire.profile_id, into)?;
    }
    Ok(())
}

fn apply_team(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: TeamWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(reason) = team_authority(conn, env, &wire)? {
        return Ok(Err(reason));
    }
    let at = crate::sql_integer(env.created_at, "envelope created_at")?;
    let current = crate::team::read_team(conn, &wire.team_id)?;
    let is_newer = current.as_ref().is_none_or(|team| {
        (at, env.id.as_str()) > (team.updated_at, team.updated_id.as_deref().unwrap_or(""))
    });
    if is_newer {
        let team = Team {
            team_id: wire.team_id,
            owner: wire.owner,
            name: wire.name,
            members: wire.members,
            skills: wire.skills,
            reach: wire.policy.reach,
            lead: wire.lead,
            created_at: current.as_ref().map_or(at, |t| t.created_at.min(at)),
            updated_at: at,
            updated_id: Some(env.id.clone()),
            archived_at: wire.archived.then(|| current.as_ref().and_then(|t| t.archived_at).unwrap_or(at)),
        };
        crate::team::write_team(conn, &team)?;
    }
    Ok(Ok(()))
}

/// Why `env` may not set `wire`: the same rules as a profile's, and every
/// member must be one of the owner's profiles.
fn team_authority(conn: &Connection, env: &Envelope, wire: &TeamWire) -> Result<Option<String>, Error> {
    if wire.team_id.is_empty() {
        return Ok(Some("a team needs an id".into()));
    }
    if let Err(reason) = crate::team::check_shape(&wire.name, &wire.members, &wire.skills, wire.lead.as_deref()) {
        return Ok(Some(reason));
    }
    let principals = Principals::load(conn)?;
    let owner = principals.resolve(&wire.owner, env.created_at);
    if principals.resolve(&env.from, env.created_at) != owner {
        return Ok(Some(format!("signer {} doesn't speak for owner {}", env.from, wire.owner)));
    }
    if let Some(stored) = crate::team::read_team(conn, &wire.team_id)?
        && principals.resolve(&stored.owner, env.created_at) != owner
    {
        return Ok(Some(format!("team {} belongs to {}", wire.team_id, stored.owner)));
    }
    Ok(crate::team::check_members(conn, &principals, owner, &wire.members, env.created_at)?.err())
}

fn apply_announce(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: AnnounceWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Some(reason) = announce_authority(conn, env, &wire)? {
        return Ok(Err(reason));
    }
    // An announcement made before its profile merged (or carried late by a
    // third device) lands on the profile it merged into.
    let profile_id = surviving_profile(conn, &wire.profile_id)?;
    // An instance this machine runs is never re-homed by someone's
    // announcement; this machine's own announcement marks it announced.
    conn.execute(
        "UPDATE agents SET announced_id = COALESCE(announced_id, ?2) WHERE pubkey = ?1 AND device IS NULL",
        params![wire.instance, env.id],
    )?;
    conn.execute(
        "INSERT INTO agents (pubkey, profile_id, device, auth_tag_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(pubkey) DO UPDATE SET device = excluded.device, profile_id = excluded.profile_id,
             auth_tag_json = excluded.auth_tag_json
             WHERE agents.device IS NOT NULL",
        params![
            wire.instance,
            profile_id,
            env.from,
            serde_json::to_string(&wire.auth)?,
            crate::sql_integer(env.created_at, "envelope created_at")?
        ],
    )?;
    Ok(Ok(()))
}

/// Why `env` may not announce `wire`'s instance: the attestation must
/// verify, and the announcing device must speak for both the attester and
/// the profile's owner, so a machine nobody linked can't add instances to
/// someone's agents.
fn announce_authority(conn: &Connection, env: &Envelope, wire: &AnnounceWire) -> Result<Option<String>, Error> {
    let Ok(instance) = PublicKey::from_base58(&wire.instance) else {
        return Ok(Some(format!("instance {:?} is not a public key", wire.instance)));
    };
    let Ok(auth) = AuthTag::from_tag(&wire.auth) else {
        return Ok(Some("malformed instance attestation".into()));
    };
    if verify_attestation(&auth, &instance).is_err() {
        return Ok(Some("the instance attestation doesn't verify".into()));
    }
    let principals = Principals::load(conn)?;
    let device = principals.resolve(&env.from, env.created_at);
    let attester = auth.owner.to_base58();
    if principals.resolve(&attester, env.created_at) != device {
        return Ok(Some(format!("device {} doesn't speak for the instance's attester {attester}", env.from)));
    }
    let profile_owner: Option<Option<String>> = conn
        .query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [&wire.profile_id], |r| r.get(0))
        .optional()?;
    let Some(profile_owner) = profile_owner else {
        return Ok(Some(format!("profile {} not received yet", wire.profile_id)));
    };
    if !profile_owner.is_some_and(|o| principals.resolve(&o, env.created_at) == device) {
        return Ok(Some(format!("device {} doesn't speak for the owner of profile {}", env.from, wire.profile_id)));
    }
    Ok(None)
}

/// Migration 13: each profile's owner is whoever attested its instance.
pub(crate) fn fill_profile_owners(tx: &Connection) -> Result<(), Error> {
    let rows: Vec<(String, String)> = {
        let mut statement =
            tx.prepare("SELECT profile_id, auth_tag_json FROM agents WHERE auth_tag_json IS NOT NULL")?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (profile_id, auth_json) in rows {
        // An attestation that doesn't parse leaves the owner unknown, as it
        // was: authorization never read it from here.
        let Some(tag) = serde_json::from_str::<Vec<String>>(&auth_json).ok().and_then(|t| AuthTag::from_tag(&t).ok())
        else {
            continue;
        };
        tx.execute(
            "UPDATE agent_profiles SET owner = ?2 WHERE profile_id = ?1",
            params![profile_id, tag.owner.to_base58()],
        )?;
    }
    Ok(())
}

impl Store {
    /// This machine's instances that no kind-121 envelope announced yet,
    /// with the attestation an announcement carries.
    pub fn unannounced_instances(&self) -> Result<Vec<(String, String, Vec<String>)>, Error> {
        let mut statement = self.conn.prepare(
            "SELECT pubkey, profile_id, auth_tag_json FROM agents
             WHERE device IS NULL AND announced_id IS NULL AND auth_tag_json IS NOT NULL AND archived_at IS NULL",
        )?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (pubkey, profile_id, auth_json) = row?;
            out.push((pubkey, profile_id, serde_json::from_str(&auth_json)?));
        }
        Ok(out)
    }
}
