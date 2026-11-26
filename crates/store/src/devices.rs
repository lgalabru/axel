//! Device links (kind 120, RFD 0004): which machine's key speaks for which
//! owner, and over what span of time.
//!
//! A link needs both sides: the owner signs the envelope, and the device's
//! own signature in it (`consent`) agrees to being linked.  Without the
//! second, any key could claim any machine.  A key may be linked, revoked,
//! and linked again later; each link is its own row, so a revocation is
//! never undone by a later link: the span between them stays unlinked.

use std::collections::HashMap;

use axel_identity::{PublicKey, verify_link_consent};
use axel_inbox::Envelope;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Device, DeviceOp, Error, PairingRoute, Store, now};

/// The most links between a key and the owner it speaks for.  A device of a
/// merged home is two (device → old owner → owner); four leaves room for a
/// merge of merges and bounds the walk whatever the table holds.
pub const PRINCIPAL_DEPTH_MAX: usize = 4;

/// The most links one key may have had over its life.  A machine linked,
/// revoked and linked again a few times a year stays far below it; past it
/// is a replica being flooded, and further links are refused.
const LINKS_PER_DEVICE_MAX: usize = 64;

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
    /// The device's signature agreeing to the link
    /// (`axel_identity::consent_to_link`): required to link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<String>,
}

/// One span during which a device spoke for an owner.
#[derive(Clone, Debug)]
struct Link {
    owner: String,
    /// `None` for a key's first link: it vouches for what the key signed
    /// before, which is how a home merged into another brings its history.
    from: Option<i64>,
    until: Option<i64>,
}

impl Link {
    fn covers(&self, at: u64) -> bool {
        let Ok(at) = i64::try_from(at) else { return false };
        self.from.is_none_or(|from| from <= at) && self.until.is_none_or(|until| at < until)
    }
}

/// Every device link on this replica, for resolving signers to owners.
#[derive(Clone, Debug, Default)]
pub(crate) struct Principals {
    links: HashMap<String, Vec<Link>>,
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
        let mut statement = conn.prepare_cached(
            "SELECT pubkey, owner, linked_at, revoked_at FROM devices WHERE owner IS NOT NULL ORDER BY pubkey, linked_at, link_event_id",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?))
        })?;
        let mut links: HashMap<String, Vec<Link>> = HashMap::new();
        for row in rows {
            let (device, owner, linked_at, revoked_at) = row?;
            debug_assert_ne!(device, owner, "no device is its own owner");
            let spans = links.entry(device).or_default();
            let from = (!spans.is_empty()).then_some(linked_at);
            spans.push(Link { owner, from, until: revoked_at });
        }
        Ok(Principals { links })
    }

    /// The owner `key` speaks for at `at`: itself when no link covers `at`.
    pub(crate) fn resolve<'a>(&'a self, key: &'a str, at: u64) -> &'a str {
        let mut current = key;
        for _ in 0..PRINCIPAL_DEPTH_MAX {
            match self.links.get(current).and_then(|spans| spans.iter().find(|l| l.covers(at))) {
                Some(link) => current = &link.owner,
                None => return current,
            }
        }
        current
    }

    /// The live link of `device`, if it has one.
    fn live(&self, device: &str) -> Option<&Link> {
        self.links.get(device).and_then(|spans| spans.iter().find(|l| l.until.is_none()))
    }

    /// How many live links lead up from `key` to its root.
    fn depth_above(&self, key: &str) -> usize {
        let mut current = key;
        for depth in 0..=PRINCIPAL_DEPTH_MAX {
            match self.live(current) {
                Some(link) => current = &link.owner,
                None => return depth,
            }
        }
        PRINCIPAL_DEPTH_MAX + 1
    }

    /// How many live links lead down from `key` to its deepest device.
    fn depth_below(&self, key: &str) -> usize {
        let mut level = vec![key.to_string()];
        for depth in 0..=PRINCIPAL_DEPTH_MAX {
            let next: Vec<String> = self
                .links
                .iter()
                .filter(|(_, spans)| spans.iter().any(|l| l.until.is_none() && level.contains(&l.owner)))
                .map(|(device, _)| device.clone())
                .collect();
            if next.is_empty() {
                return depth;
            }
            level = next;
        }
        PRINCIPAL_DEPTH_MAX + 1
    }

    /// Whether linking `device` under `owner` would make a key its own owner
    /// or a chain longer than authorization walks.
    fn refuses_link(&self, owner: &str, device: &str) -> Option<String> {
        let mut current = owner;
        for _ in 0..=PRINCIPAL_DEPTH_MAX {
            if current == device {
                return Some(format!("linking {device} under {owner} would make a key its own owner"));
            }
            match self.live(current) {
                Some(link) => current = &link.owner,
                None => break,
            }
        }
        let length = self.depth_above(owner) + 1 + self.depth_below(device);
        (length > PRINCIPAL_DEPTH_MAX).then(|| format!("that link would chain {length} keys, more than {PRINCIPAL_DEPTH_MAX}"))
    }
}

pub(crate) fn parse_record(env: &Envelope) -> Result<DeviceRecord, String> {
    let record: DeviceRecord = serde_json::from_str(&env.content).map_err(|e| format!("malformed content: {e}"))?;
    if PublicKey::from_base58(&record.device).is_err() {
        return Err(format!("device {:?} is not a public key", record.device));
    }
    Ok(record)
}

/// Why a kind-120 envelope can't take effect yet, or `Ok` once it has.
pub(crate) fn apply(conn: &Connection, env: &Envelope) -> Result<Result<(), String>, Error> {
    let record = match parse_record(env) {
        Ok(r) => r,
        Err(reason) => return Ok(Err(reason)),
    };
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
    // One label per machine: the newest.
    let newer: bool = conn
        .query_row(
            "SELECT 1 FROM devices WHERE pubkey = ?1 AND owner IS NULL AND linked_at > ?2",
            params![record.device, at],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !newer {
        conn.execute("DELETE FROM devices WHERE pubkey = ?1 AND owner IS NULL", [&record.device])?;
        conn.execute(
            "INSERT INTO devices (link_event_id, pubkey, owner, label, linked_at) VALUES (?1, ?2, NULL, ?3, ?4)",
            params![env.id, record.device, record.label, at],
        )?;
    }
    Ok(Ok(()))
}

fn apply_link(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    if record.device == env.from {
        return Ok(Err("an owner can't link itself".into()));
    }
    let Some(route) = record.route else {
        return Ok(Err("a link must say how the device was proved".into()));
    };
    let (Ok(owner_key), Ok(device_key)) = (PublicKey::from_base58(&env.from), PublicKey::from_base58(&record.device)) else {
        return Ok(Err("a link names two public keys".into()));
    };
    let consents = record.consent.as_deref().is_some_and(|c| verify_link_consent(&owner_key, &device_key, c).is_ok());
    if !consents {
        return Ok(Err(format!("device {} didn't consent to being linked", record.device)));
    }
    let principals = Principals::load(conn)?;
    if let Some(live) = principals.live(&record.device) {
        // The same link arriving twice, or a refresh from its owner, changes nothing.
        return Ok(if live.owner == env.from { Ok(()) } else { Err(format!("device {} is already linked to {}", record.device, live.owner)) });
    }
    if principals.links.get(&record.device).is_some_and(|spans| spans.len() >= LINKS_PER_DEVICE_MAX) {
        return Ok(Err(format!("device {} was linked {LINKS_PER_DEVICE_MAX} times already", record.device)));
    }
    let revoked_after: bool = conn
        .query_row("SELECT 1 FROM devices WHERE pubkey = ?1 AND owner IS NOT NULL AND revoked_at > ?2", params![record.device, at], |_| Ok(()))
        .optional()?
        .is_some();
    if revoked_after {
        return Ok(Err("a link can't predate the device's last revocation".into()));
    }
    if let Some(reason) = principals.refuses_link(&env.from, &record.device) {
        return Ok(Err(reason));
    }
    conn.execute(
        "INSERT INTO devices (link_event_id, pubkey, owner, label, route, linked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![env.id, record.device, env.from, record.label, route.as_str(), at],
    )?;
    Ok(Ok(()))
}

fn apply_revoke(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    let live: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT link_event_id, owner, linked_at FROM devices WHERE pubkey = ?1 AND owner IS NOT NULL AND revoked_at IS NULL",
            [&record.device],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((link_id, owner, linked_at)) = live else {
        return revoke_again(conn, env, record, at);
    };
    // Any key that speaks for the link's owner may withdraw it: a lost
    // laptop that linked a machine can't be the only way to unlink it.
    let principals = Principals::load(conn)?;
    if principals.resolve(&env.from, env.created_at) != principals.resolve(&owner, env.created_at) {
        return Ok(Err(format!("{} doesn't speak for the owner that linked {}", env.from, record.device)));
    }
    if at < linked_at {
        return Ok(Err("a revocation can't predate the link it withdraws".into()));
    }
    conn.execute(
        "UPDATE devices SET revoked_at = ?2, revoke_event_id = ?3 WHERE link_event_id = ?1",
        params![link_id, at, env.id],
    )?;
    Ok(Ok(()))
}

/// A revocation of a link already revoked: the earliest revocation stands,
/// so a later one is accepted and changes nothing, and an earlier one by
/// the same owner moves it back.
fn revoke_again(conn: &Connection, env: &Envelope, record: &DeviceRecord, at: i64) -> Result<Result<(), String>, Error> {
    let latest: Option<(String, String, i64, i64)> = conn
        .query_row(
            "SELECT link_event_id, owner, linked_at, revoked_at FROM devices WHERE pubkey = ?1 AND owner IS NOT NULL
             ORDER BY linked_at DESC, link_event_id DESC LIMIT 1",
            [&record.device],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((link_id, owner, linked_at, revoked_at)) = latest else {
        return Ok(Err(format!("device {} is not linked yet", record.device)));
    };
    let principals = Principals::load(conn)?;
    if principals.resolve(&env.from, env.created_at) != principals.resolve(&owner, env.created_at) {
        return Ok(Err(format!("{} doesn't speak for the owner that linked {}", env.from, record.device)));
    }
    if at >= linked_at && at < revoked_at {
        conn.execute("UPDATE devices SET revoked_at = ?2, revoke_event_id = ?3 WHERE link_event_id = ?1", params![link_id, at, env.id])?;
    }
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

/// A machine's current row: its newest link, else its own label.
const CURRENT_DEVICE: &str = "ORDER BY owner IS NULL, linked_at DESC, link_event_id DESC LIMIT 1";

impl Store {
    /// Every machine this replica knows, one row each: its newest link, or
    /// an owner's own label.
    pub fn devices(&self) -> Result<Vec<Device>, Error> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {DEVICE_COLS} FROM devices d
             WHERE link_event_id = (SELECT link_event_id FROM devices WHERE pubkey = d.pubkey {CURRENT_DEVICE})
             ORDER BY label COLLATE NOCASE"
        ))?;
        let rows = statement.query_map([], row_device)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One machine by its key: its newest link, else its own label.
    pub fn device(&self, pubkey: &str) -> Result<Option<Device>, Error> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {DEVICE_COLS} FROM devices WHERE pubkey = ?1 {CURRENT_DEVICE}"), [pubkey], row_device)
            .optional()?)
    }

    /// The machine labelled `label` whose live link speaks for `owner` (a
    /// principal).  Another owner's machine of the same name never matches.
    pub fn device_by_label(&self, label: &str, owner: &str) -> Result<Option<Device>, Error> {
        let principals = Principals::load(&self.conn)?;
        let now = u64::try_from(now()).unwrap_or(0);
        Ok(self
            .devices()?
            .into_iter()
            .find(|d| d.label == label && d.revoked_at.is_none() && d.owner.is_some() && principals.resolve(&d.pubkey, now) == owner))
    }

    /// The owner `key` speaks for now: itself unless a live link covers it.
    pub fn principal(&self, key: &str) -> Result<String, Error> {
        let principals = Principals::load(&self.conn)?;
        let now = u64::try_from(now()).unwrap_or(0);
        Ok(principals.resolve(key, now).to_string())
    }
}
