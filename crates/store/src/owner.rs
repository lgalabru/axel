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

use std::collections::HashMap;

use axel_identity::{AuthTag, PublicKey, verify_attestation};
use axel_inbox::Envelope;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::log::{EVENT_COLS, mark_applied, mark_rejected, replay_thread, row_envelope};
use crate::types::lenient;
use crate::{
    AgentColor, AgentProfile, Device, DeviceOp, Effort, Error, Harness, PairingRoute, RuntimeMode,
    Store, Team, TeamMember, TeamReach, kind, now,
};

/// The most links between a key and the owner it speaks for.  A device of a
/// merged home is two (device → old owner → owner); four leaves room for a
/// merge of merges and bounds the walk whatever the table holds.
pub const PRINCIPAL_DEPTH_MAX: usize = 4;

/// The most passes that revive owner records rejected for want of another
/// one (a profile before its signer's link).  Each pass that revives
/// anything can unlock at most one more level of the chain.
const REVIVE_PASSES_MAX: usize = PRINCIPAL_DEPTH_MAX + 2;

/// Kind-120 content.  The signer is the owner, except for `label`, where
/// the signer names its own machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub op: DeviceOp,
    pub device: String,
    pub label: String,
    /// How the far end was proved: required to link, absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<PairingRoute>,
}

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
}

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

/// One device → owner link as authorization sees it.
#[derive(Clone, Debug)]
struct Link {
    owner: String,
    revoked_at: Option<i64>,
}

impl Link {
    /// A revoked device authorizes nothing signed at or after its revocation.
    fn permits(&self, at: u64) -> bool {
        self.revoked_at
            .is_none_or(|revoked| i64::try_from(at).is_ok_and(|at| at < revoked))
    }
}

/// Every device link on this replica, for resolving signers to owners.
#[derive(Clone, Debug, Default)]
pub(crate) struct Principals {
    links: HashMap<String, Link>,
}

impl Principals {
    pub(crate) fn load(conn: &Connection) -> Result<Principals, Error> {
        // Migrations before 13 replay threads too, before this table exists:
        // then nobody is anybody's device yet.
        let has_devices = conn
            .prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'devices'")?
            .exists([])?;
        if !has_devices {
            return Ok(Principals::default());
        }
        let mut statement =
            conn.prepare_cached("SELECT pubkey, owner, revoked_at FROM devices WHERE owner IS NOT NULL")?;
        let rows = statement.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, Link { owner: r.get(1)?, revoked_at: r.get(2)? }))
        })?;
        let links: HashMap<String, Link> = rows.collect::<Result<_, _>>()?;
        debug_assert!(links.iter().all(|(device, link)| *device != link.owner), "no device is its own owner");
        Ok(Principals { links })
    }

    /// The owner `key` speaks for at `at`: itself when it is no live device.
    pub(crate) fn resolve<'a>(&'a self, key: &'a str, at: u64) -> &'a str {
        let mut current = key;
        for _ in 0..PRINCIPAL_DEPTH_MAX {
            match self.links.get(current) {
                Some(link) if link.permits(at) => current = &link.owner,
                _ => return current,
            }
        }
        current
    }

    /// Whether linking `device` under `owner` would make a key its own owner.
    fn would_cycle(&self, owner: &str, device: &str) -> bool {
        let mut current = owner;
        for _ in 0..=PRINCIPAL_DEPTH_MAX {
            if current == device {
                return true;
            }
            match self.links.get(current) {
                Some(link) => current = &link.owner,
                None => return false,
            }
        }
        // Deeper than any chain we accept counts as a cycle: fail closed.
        true
    }
}

/// Project one owner record and settle it as applied or rejected.  When a
/// link changes who speaks for whom, every thread is replayed, and records
/// that waited for it are revived.
pub(crate) fn project(conn: &Connection, env: &Envelope) -> Result<(), Error> {
    assert!(kind::is_owner_record(env.kind), "kind {} is no owner record", env.kind);
    match apply(conn, env)? {
        Err(reason) => mark_rejected(conn, &env.id, &reason),
        Ok(()) => {
            mark_applied(conn, &env.id)?;
            let mut authorization_changed = changes_authorization(env.kind);
            authorization_changed |= revive(conn)?;
            if authorization_changed {
                replay_all_threads(conn)?;
            }
            Ok(())
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

fn replay_all_threads(conn: &Connection) -> Result<(), Error> {
    let threads: Vec<String> = {
        let mut statement =
            conn.prepare_cached("SELECT DISTINCT thread_id FROM events WHERE thread_id IS NOT NULL")?;
        let rows = statement.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for thread_id in &threads {
        replay_thread(conn, thread_id, None)?;
    }
    Ok(())
}

/// [`apply`] for a full rebuild, which replays every thread afterwards.
pub(crate) fn apply_only(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    apply(conn, env)
}

/// Why an owner record can't take effect yet, or `Ok` once it has.
fn apply(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    match env.kind {
        kind::OWNER_DEVICE => apply_device(conn, env),
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

fn is_key(text: &str) -> bool {
    PublicKey::from_base58(text).is_ok()
}

fn apply_device(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let record: DeviceRecord = match parse(env) {
        Ok(r) => r,
        Err(reason) => return Ok(Err(reason)),
    };
    if !is_key(&record.device) {
        return Ok(Err(format!("device {:?} is not a public key", record.device)));
    }
    let at = crate::sql_integer(env.created_at, "envelope created_at")?;
    match record.op {
        DeviceOp::Label => apply_label(conn, env, &record, at),
        DeviceOp::Link => apply_link(conn, env, &record, at),
        DeviceOp::Revoke => apply_revoke(conn, env, &record, at),
    }
}

fn apply_label(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    if record.device != env.from {
        return Ok(Err("a machine can only label itself".into()));
    }
    conn.execute(
        "INSERT INTO devices (pubkey, owner, label, route, link_event_id, linked_at) VALUES (?1, NULL, ?2, ?3, ?4, ?5)
         ON CONFLICT(pubkey) DO UPDATE SET label = excluded.label",
        params![record.device, record.label, record.route.map(PairingRoute::as_str), env.id, at],
    )?;
    Ok(Ok(()))
}

fn apply_link(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    if record.device == env.from {
        return Ok(Err("an owner can't link itself".into()));
    }
    let Some(route) = record.route else {
        return Ok(Err("a link must say how the device was proved".into()));
    };
    let existing = device_row(conn, &record.device)?;
    if let Some(d) = &existing
        && let Some(owner) = &d.owner
        && owner != &env.from
        && d.revoked_at.is_none()
    {
        return Ok(Err(format!("device {} is already linked to {owner}", record.device)));
    }
    if Principals::load(conn)?.would_cycle(&env.from, &record.device) {
        return Ok(Err(format!("linking {} under {} would make a key its own owner", record.device, env.from)));
    }
    conn.execute(
        "INSERT INTO devices (pubkey, owner, label, route, link_event_id, linked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(pubkey) DO UPDATE SET owner = excluded.owner, label = excluded.label, route = excluded.route,
             link_event_id = excluded.link_event_id, linked_at = excluded.linked_at, revoked_at = NULL, revoke_event_id = NULL",
        params![record.device, env.from, record.label, route.as_str(), env.id, at],
    )?;
    Ok(Ok(()))
}

fn apply_revoke(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    let Some(existing) = device_row(conn, &record.device)? else {
        return Ok(Err(format!("device {} is not linked yet", record.device)));
    };
    if existing.owner.as_deref() != Some(env.from.as_str()) {
        return Ok(Err("only the owner that linked a device can revoke it".into()));
    }
    // The earliest revocation stands: a later one can't extend a device's life.
    conn.execute(
        "UPDATE devices SET revoked_at = MIN(COALESCE(revoked_at, ?2), ?2), revoke_event_id = COALESCE(revoke_event_id, ?3)
         WHERE pubkey = ?1",
        params![record.device, at, env.id],
    )?;
    Ok(Ok(()))
}

fn apply_profile(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: ProfileWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    if wire.name.trim().is_empty() || wire.profile_id.is_empty() {
        return Ok(Err("a profile needs an id and a name".into()));
    }
    let principals = Principals::load(conn)?;
    if principals.resolve(&env.from, env.created_at) != principals.resolve(&wire.owner, env.created_at) {
        return Ok(Err(format!("signer {} doesn't speak for owner {}", env.from, wire.owner)));
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

fn write_profile(conn: &Connection, wire: &ProfileWire, at: i64, event_id: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO agent_profiles (profile_id, owner, name, harness, model, effort, skills_json, prompt, color, runtime_mode,
                                     created_at, updated_at, updated_id, archived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13)
         ON CONFLICT(profile_id) DO UPDATE SET owner = excluded.owner, name = excluded.name, harness = excluded.harness,
             model = excluded.model, effort = excluded.effort, skills_json = excluded.skills_json, prompt = excluded.prompt,
             color = excluded.color, runtime_mode = excluded.runtime_mode, updated_at = excluded.updated_at,
             updated_id = excluded.updated_id, archived_at = excluded.archived_at",
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
        ],
    )?;
    Ok(())
}

fn apply_team(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: TeamWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    if wire.team_id.is_empty() {
        return Ok(Err("a team needs an id".into()));
    }
    if let Err(reason) = crate::team::check_shape(&wire.name, &wire.members, &wire.skills, wire.lead.as_deref()) {
        return Ok(Err(reason));
    }
    let principals = Principals::load(conn)?;
    let owner = principals.resolve(&wire.owner, env.created_at);
    if principals.resolve(&env.from, env.created_at) != owner {
        return Ok(Err(format!("signer {} doesn't speak for owner {}", env.from, wire.owner)));
    }
    if let Err(reason) = crate::team::check_members(conn, &principals, owner, &wire.members, env.created_at)? {
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

fn apply_announce(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let wire: AnnounceWire = match parse(env) {
        Ok(w) => w,
        Err(reason) => return Ok(Err(reason)),
    };
    let Ok(instance) = PublicKey::from_base58(&wire.instance) else {
        return Ok(Err(format!("instance {:?} is not a public key", wire.instance)));
    };
    let Ok(auth) = AuthTag::from_tag(&wire.auth) else {
        return Ok(Err("malformed instance attestation".into()));
    };
    if verify_attestation(&auth, &instance).is_err() {
        return Ok(Err("the instance attestation doesn't verify".into()));
    }
    let principals = Principals::load(conn)?;
    let attester = auth.owner.to_base58();
    if principals.resolve(&attester, env.created_at) != principals.resolve(&env.from, env.created_at) {
        return Ok(Err(format!("device {} doesn't speak for the instance's attester {attester}", env.from)));
    }
    let profile_owner: Option<Option<String>> = conn
        .query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [&wire.profile_id], |r| r.get(0))
        .optional()?;
    let Some(profile_owner) = profile_owner else {
        return Ok(Err(format!("profile {} not received yet", wire.profile_id)));
    };
    // The device must speak for the profile's owner: a machine nobody linked
    // can't add instances to someone's agents.
    let speaks_for_owner = profile_owner
        .is_some_and(|o| principals.resolve(&o, env.created_at) == principals.resolve(&env.from, env.created_at));
    if !speaks_for_owner {
        return Ok(Err(format!("device {} doesn't speak for the owner of profile {}", env.from, wire.profile_id)));
    }
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
            wire.profile_id,
            env.from,
            serde_json::to_string(&wire.auth)?,
            crate::sql_integer(env.created_at, "envelope created_at")?
        ],
    )?;
    Ok(Ok(()))
}

const DEVICE_COLS: &str = "pubkey, owner, label, route, linked_at, revoked_at";

fn row_device(r: &Row<'_>) -> rusqlite::Result<Device> {
    let route: Option<String> = r.get("route")?;
    Ok(Device {
        pubkey: r.get("pubkey")?,
        owner: r.get("owner")?,
        label: r.get("label")?,
        // A route this build doesn't know was refused at ingest, so this is
        // only a hand-edited row; it reads as the weakest route.
        route: route.map(|r| r.parse().unwrap_or(PairingRoute::Confirm)),
        linked_at: r.get("linked_at")?,
        revoked_at: r.get("revoked_at")?,
    })
}

fn device_row(conn: &Connection, pubkey: &str) -> Result<Option<Device>, Error> {
    Ok(conn
        .query_row(&format!("SELECT {DEVICE_COLS} FROM devices WHERE pubkey = ?1"), [pubkey], row_device)
        .optional()?)
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
    /// Every machine this replica knows, linked ones and owners' own labels.
    pub fn devices(&self) -> Result<Vec<Device>, Error> {
        let mut statement =
            self.conn.prepare(&format!("SELECT {DEVICE_COLS} FROM devices ORDER BY label COLLATE NOCASE"))?;
        let rows = statement.query_map([], row_device)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One machine by its key.
    pub fn device(&self, pubkey: &str) -> Result<Option<Device>, Error> {
        device_row(&self.conn, pubkey)
    }

    /// One machine by its label, preferring a live link.
    pub fn device_by_label(&self, label: &str) -> Result<Option<Device>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {DEVICE_COLS} FROM devices WHERE label = ?1 ORDER BY revoked_at IS NOT NULL LIMIT 1"),
                [label],
                row_device,
            )
            .optional()?)
    }

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

    /// The owner `key` speaks for now: itself unless it is a live device.
    pub fn principal(&self, key: &str) -> Result<String, Error> {
        let principals = Principals::load(&self.conn)?;
        let now = u64::try_from(now()).unwrap_or(0);
        Ok(principals.resolve(key, now).to_string())
    }
}
