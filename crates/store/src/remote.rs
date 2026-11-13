//! Remote hosts and replication: hosts this replica knows, grants it holds
//! to sign for agents, per-peer sync cursors, the threads shared with each
//! peer, and incremental export/import of conversation items.  All
//! local-only; see `docs/DESIGN.md` "Remote hosts".

use axel_identity::Delegation;
use axel_inbox::Envelope;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::items::{ITEM_COLS, row_item};
use crate::log::{EVENT_COLS, row_envelope};
use crate::{ConversationItem, Environment, Error, Store, now};

/// One change to a conversation item, in revision order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ItemChange {
    Upsert { rev: i64, item: ConversationItem },
    Delete { rev: i64, id: String },
}

impl ItemChange {
    pub fn rev(&self) -> i64 {
        match self {
            ItemChange::Upsert { rev, .. } | ItemChange::Delete { rev, .. } => *rev,
        }
    }
}

/// The sync streams a peer cursor can track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStream {
    /// Envelopes we sent to the peer (our `events.seq`).
    EventsOut,
    /// Envelopes we received from the peer (its `events.seq`).
    EventsIn,
    /// Conversation items we received from the peer (its item `rev`).
    ItemsIn,
}

impl SyncStream {
    fn as_str(self) -> &'static str {
        match self {
            SyncStream::EventsOut => "events_out",
            SyncStream::EventsIn => "events_in",
            SyncStream::ItemsIn => "items_in",
        }
    }
}

const ENV_COLS: &str = "id, label, rex_host_id, is_local, created_at, ssh_target, host_pubkey";

fn row_env(r: &rusqlite::Row<'_>) -> rusqlite::Result<Environment> {
    Ok(Environment {
        id: r.get("id")?,
        label: r.get("label")?,
        rex_host_id: r.get("rex_host_id")?,
        is_local: r.get::<_, i64>("is_local")? != 0,
        created_at: r.get("created_at")?,
        ssh_target: r.get("ssh_target")?,
        host_pubkey: r.get("host_pubkey")?,
    })
}

impl Store {
    // ------------------------------------------------------- agent hosts

    /// Run `agent` on host `host_id` when a summon names no host; `None`
    /// runs it here again.
    pub fn set_agent_host(&self, agent: &str, host_id: Option<&str>) -> Result<(), Error> {
        match host_id {
            Some(h) => self.conn.execute(
                "INSERT INTO agent_hosts (agent_pubkey, host_id) VALUES (?1, ?2)
                 ON CONFLICT(agent_pubkey) DO UPDATE SET host_id = ?2",
                params![agent, h],
            )?,
            None => self.conn.execute("DELETE FROM agent_hosts WHERE agent_pubkey = ?1", params![agent])?,
        };
        Ok(())
    }

    /// The host `agent` runs on by default, if it has one (and the host is
    /// still known).
    pub fn agent_host(&self, agent: &str) -> Result<Option<Environment>, Error> {
        let id: Option<String> = self
            .conn
            .query_row("SELECT host_id FROM agent_hosts WHERE agent_pubkey = ?1", params![agent], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.environment(&id),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------- hosts

    /// Record (or update) a remote host.  `id` is the host's *own* local
    /// environment id, so sessions it starts (whose `session.start` names
    /// that id) are recognised as running there.
    pub fn put_host(&self, id: &str, label: &str, ssh_target: &str, host_pubkey: &str, rex_host: Option<&str>) -> Result<Environment, Error> {
        self.conn.execute(
            "INSERT INTO environments (id, label, rex_host_id, is_local, created_at, ssh_target, host_pubkey)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET label = ?2, rex_host_id = ?3, ssh_target = ?5, host_pubkey = ?6",
            params![id, label, rex_host, now(), ssh_target, host_pubkey],
        )?;
        self.environment(id)?.ok_or_else(|| Error::NotFound(format!("host {label}")))
    }

    /// One environment by id.
    pub fn environment(&self, id: &str) -> Result<Option<Environment>, Error> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE id = ?1"), [id], row_env)
            .optional()?)
    }

    /// A remote host by label.
    pub fn host(&self, label: &str) -> Result<Option<Environment>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ENV_COLS} FROM environments WHERE is_local = 0 AND label = ?1"),
                [label],
                row_env,
            )
            .optional()?)
    }

    /// Every remote host, by label.
    pub fn hosts(&self) -> Result<Vec<Environment>, Error> {
        let mut st = self.conn.prepare(&format!("SELECT {ENV_COLS} FROM environments WHERE is_local = 0 ORDER BY label"))?;
        let rows = st.query_map([], row_env)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forget a host: its row, cursors and shared threads.  Its delegations
    /// expire on their own; replicas keep the envelopes it signed.
    pub fn remove_host(&self, label: &str) -> Result<bool, Error> {
        let Some(h) = self.host(label)? else { return Ok(false) };
        self.conn.execute("DELETE FROM peer_cursors WHERE peer = ?1", [&h.id])?;
        self.conn.execute("DELETE FROM shared_threads WHERE peer = ?1", [&h.id])?;
        self.conn.execute("DELETE FROM agent_hosts WHERE host_id = ?1", [&h.id])?;
        self.conn.execute("DELETE FROM environments WHERE id = ?1 AND is_local = 0", [&h.id])?;
        Ok(true)
    }

    // ------------------------------------------------------- delegations

    /// Hold a grant to sign as its delegator on its thread, with `signer`
    /// (this machine's host key).  `auth_tag` is the delegator's owner
    /// attestation, attached too so peers can trace it to a participant.
    pub fn put_delegation(&self, signer: &str, deleg_tag: &[String], auth_tag: Option<&[String]>) -> Result<(), Error> {
        let d = Delegation::from_tag(deleg_tag).map_err(|e| Error::Invalid(format!("delegation: {e}")))?;
        let not_after = d
            .conditions
            .split('&')
            .find_map(|c| c.strip_prefix("created_at<"))
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or_else(|| Error::Invalid("a delegation must expire".into()))?;
        self.conn.execute(
            "INSERT OR REPLACE INTO delegations (signer_pubkey, thread_id, delegator_pubkey, deleg_tag_json, auth_tag_json, not_after)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                signer,
                d.thread,
                d.delegator.to_base58(),
                serde_json::to_string(deleg_tag)?,
                auth_tag.map(serde_json::to_string).transpose()?,
                not_after
            ],
        )?;
        Ok(())
    }

    /// The tags `signer` attaches on `thread` to speak for its delegator
    /// (the grant, then the delegator's attestation), while it is valid.
    pub(crate) fn delegation_tags(&self, signer: &str, thread: &str) -> Result<Option<Vec<Vec<String>>>, Error> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT deleg_tag_json, auth_tag_json FROM delegations WHERE signer_pubkey = ?1 AND thread_id = ?2 AND not_after > ?3",
                params![signer, thread, now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((deleg, auth)) = row else { return Ok(None) };
        let mut tags = vec![serde_json::from_str::<Vec<String>>(&deleg)?];
        if let Some(a) = auth {
            tags.push(serde_json::from_str(&a)?);
        }
        Ok(Some(tags))
    }

    /// The agent `signer` speaks for on `thread`, if it holds a valid grant.
    pub fn delegator(&self, signer: &str, thread: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT delegator_pubkey FROM delegations WHERE signer_pubkey = ?1 AND thread_id = ?2 AND not_after > ?3",
                params![signer, thread, now()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The key this replica signs with for `agent` on `thread`: the newest
    /// valid grant's delegate (each remote job gets its own key).
    pub fn delegate_signer(&self, agent: &str, thread: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT signer_pubkey FROM delegations WHERE delegator_pubkey = ?1 AND thread_id = ?2 AND not_after > ?3
                 ORDER BY not_after DESC LIMIT 1",
                params![agent, thread, now()],
                |r| r.get(0),
            )
            .optional()?)
    }

    // -------------------------------------------------------------- peers

    pub fn peer_cursor(&self, peer: &str, stream: SyncStream) -> Result<i64, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT cursor FROM peer_cursors WHERE peer = ?1 AND stream = ?2",
                params![peer, stream.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_peer_cursor(&self, peer: &str, stream: SyncStream, cursor: i64) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO peer_cursors (peer, stream, cursor) VALUES (?1, ?2, ?3)
             ON CONFLICT(peer, stream) DO UPDATE SET cursor = MAX(cursor, ?3)",
            params![peer, stream.as_str(), cursor],
        )?;
        Ok(())
    }

    /// Replicate `thread` to `peer` from now on.
    pub fn share_thread(&self, peer: &str, thread: &str) -> Result<(), Error> {
        self.conn.execute("INSERT OR IGNORE INTO shared_threads (peer, thread_id) VALUES (?1, ?2)", params![peer, thread])?;
        Ok(())
    }

    pub fn shared_threads(&self, peer: &str) -> Result<Vec<String>, Error> {
        let mut st = self.conn.prepare("SELECT thread_id FROM shared_threads WHERE peer = ?1 ORDER BY thread_id")?;
        let rows = st.query_map([peer], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Envelopes on `threads` with `seq > cursor`, oldest first.  `None`
    /// means every envelope that is on a thread.
    pub fn thread_events_since(&self, cursor: i64, threads: Option<&[String]>, limit: usize) -> Result<Vec<(i64, Envelope)>, Error> {
        let mut out = Vec::new();
        let mut st = self.conn.prepare(&format!(
            "SELECT seq, thread_id, {EVENT_COLS} FROM events WHERE seq > ?1 AND thread_id IS NOT NULL ORDER BY seq ASC"
        ))?;
        let rows = st.query_map([cursor], |r| Ok((r.get::<_, i64>("seq")?, r.get::<_, String>("thread_id")?, row_envelope(r)?)))?;
        for row in rows {
            let (seq, thread, env) = row?;
            if threads.is_none_or(|ts| ts.contains(&thread)) {
                out.push((seq, env));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The highest `events.seq` here (a cursor that skips everything so far).
    pub fn last_event_seq(&self) -> Result<i64, Error> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    // -------------------------------------------------------------- items

    /// Changes to the conversation items of sessions that run on
    /// `environment_id` (this machine's sessions), with `rev > cursor`.
    pub fn item_changes_since(&self, environment_id: &str, cursor: i64, limit: usize) -> Result<Vec<ItemChange>, Error> {
        let cols: String = ITEM_COLS.split(", ").map(|c| format!("ci.{c}")).collect::<Vec<_>>().join(", ");
        let mut st = self.conn.prepare(&format!(
            "SELECT ci.rev AS rev, {cols} FROM conversation_items ci JOIN sessions s ON s.id = ci.session_id
             WHERE s.environment_id = ?1 AND ci.rev > ?2 ORDER BY ci.rev ASC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![environment_id, cursor, limit as i64], |r| {
            Ok(ItemChange::Upsert { rev: r.get("rev")?, item: row_item(r)? })
        })?;
        let mut out: Vec<ItemChange> = rows.collect::<Result<_, _>>()?;
        let mut st = self.conn.prepare(
            "SELECT t.rev, t.id FROM item_tombstones t JOIN sessions s ON s.id = t.session_id
             WHERE s.environment_id = ?1 AND t.rev > ?2 ORDER BY t.rev ASC LIMIT ?3",
        )?;
        let rows = st.query_map(params![environment_id, cursor, limit as i64], |r| {
            Ok(ItemChange::Delete { rev: r.get(0)?, id: r.get(1)? })
        })?;
        for r in rows {
            out.push(r?);
        }
        out.sort_by_key(ItemChange::rev);
        out.truncate(limit);
        Ok(out)
    }

    /// Apply an item change received from the host that runs its session.
    pub fn import_item_change(&self, change: &ItemChange) -> Result<(), Error> {
        match change {
            ItemChange::Upsert { item, .. } => {
                self.conn.execute(
                    "INSERT INTO conversation_items (id, session_id, thread_id, seq, turn, at, kind, status, ref_id, content_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(id) DO UPDATE SET turn = ?5, at = ?6, kind = ?7, status = ?8, ref_id = ?9, content_json = ?10",
                    params![
                        item.id,
                        item.session_id,
                        item.thread_id,
                        item.seq,
                        item.turn,
                        item.at,
                        item.kind.as_str(),
                        item.status.map(crate::ItemStatus::as_str),
                        item.ref_id,
                        serde_json::to_string(&item.content)?
                    ],
                )?;
            }
            ItemChange::Delete { id, .. } => {
                self.conn.execute("DELETE FROM conversation_items WHERE id = ?1", [id])?;
            }
        }
        Ok(())
    }
}
