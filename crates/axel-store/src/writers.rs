//! Typed writers: build an envelope, sign it, append it, and surface rejection
//! as an error.  Every thread-scoped write takes a `created_at` that is
//! strictly after the thread's latest event on this replica, so local writes
//! in the same second keep their call order under `(created_at, id)` replay.

use std::path::Path;

use axel_identity::Keypair;
use axel_inbox::{Envelope, EnvelopeBuilder};
use rusqlite::{OptionalExtension, params};

use crate::wire::{self, kind, tag};
use crate::{
    Error, Harness, LimitWindow, NewSession, ParticipantRole, Relationship, Session, SessionUpdate, StatusReport, Store,
    Thread, ThreadState, ThreadWorkspace, TurnSummary, Usage, UsageDelta, new_id, now,
};

fn tag2(a: &str, b: &str) -> Vec<String> {
    vec![a.to_string(), b.to_string()]
}

impl Store {
    /// A `created_at` later than every event of `thread_id` already here.
    fn next_created_at(&self, thread_id: &str) -> Result<u64, Error> {
        let last: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(created_at) FROM events WHERE thread_id = ?1",
                [thread_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(now().max(last.map_or(0, |l| l + 1)) as u64)
    }

    /// The owner attestation registered for `signer`, if it is one of our agents.
    fn auth_tag_for(&self, signer: &Keypair) -> Result<Option<Vec<String>>, Error> {
        Ok(self
            .agent_by_pubkey(&signer.public().to_base58())?
            .and_then(|a| a.auth_tag))
    }

    fn build(
        &self,
        signer: &Keypair,
        kind: u32,
        content: String,
        thread_id: Option<&str>,
        extra_tags: Vec<Vec<String>>,
    ) -> Result<Envelope, Error> {
        let mut b = EnvelopeBuilder::new(kind, content);
        if let Some(t) = thread_id {
            b = b.thread(t).created_at(self.next_created_at(t)?);
        }
        for t in extra_tags {
            b = b.tag(t);
        }
        // A host signing for a delegating agent attaches the grant and the
        // agent's attestation; otherwise our own agents carry theirs.
        let delegated = match thread_id {
            Some(t) => self.delegation_tags(&signer.public().to_base58(), t)?,
            None => None,
        };
        if let Some(tags) = delegated {
            for t in tags {
                b = b.tag(t);
            }
        } else if let Some(auth) = self.auth_tag_for(signer)? {
            b = b.tag(auth);
        }
        Ok(b.sign(signer)?)
    }

    /// Sign an envelope built elsewhere for `thread` (if any), adding what
    /// lets other replicas authorize it: the grant and attestation when
    /// `signer` is a host acting for a delegating agent on that thread, else
    /// the owner attestation this replica holds for `signer` (one of our agents).
    pub fn sign_with_attestation(
        &self,
        signer: &Keypair,
        mut builder: EnvelopeBuilder,
        thread: Option<&str>,
    ) -> Result<Envelope, Error> {
        let delegated = match thread {
            Some(t) => self.delegation_tags(&signer.public().to_base58(), t)?,
            None => None,
        };
        if let Some(tags) = delegated {
            for t in tags {
                builder = builder.tag(t);
            }
        } else if let Some(auth) = self.auth_tag_for(signer)? {
            builder = builder.tag(auth);
        }
        Ok(builder.sign(signer)?)
    }

    fn commit(&self, env: &Envelope) -> Result<(), Error> {
        match self.append(env)?.rejected_reason {
            Some(reason) => Err(Error::Unauthorized(reason)),
            None => Ok(()),
        }
    }

    fn require_thread(&self, thread_id: &str) -> Result<Thread, Error> {
        self.get_thread(thread_id)?
            .ok_or_else(|| Error::NotFound(format!("thread {thread_id}")))
    }

    fn require_session(&self, session_id: &str) -> Result<Session, Error> {
        self.get_session(session_id)?
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    /// Start a thread.  The envelope id becomes the thread id and `signer`
    /// its first owner.  `parent` is `(parent_thread_id, relationship)`.
    pub fn create_thread(
        &self,
        signer: &Keypair,
        title: &str,
        intent: &str,
        parent: Option<(&str, Relationship)>,
    ) -> Result<Thread, Error> {
        let mut tags = vec![tag2(tag::TITLE, title.trim())];
        if let Some((p, rel)) = parent {
            tags.push(vec![
                tag::PARENT.to_string(),
                p.to_string(),
                rel.as_str().to_string(),
            ]);
        }
        let env = self.build(signer, kind::THREAD_CREATE, intent.to_string(), None, tags)?;
        self.commit(&env)?;
        self.require_thread(&env.id)
    }

    /// Change a thread's title and/or coarse state.
    pub fn update_thread(
        &self,
        signer: &Keypair,
        thread_id: &str,
        title: Option<&str>,
        state: Option<ThreadState>,
    ) -> Result<(), Error> {
        self.require_thread(thread_id)?;
        if title.is_none() && state.is_none() {
            return Ok(());
        }
        let content = serde_json::to_string(&wire::ThreadUpdate {
            title: title.map(str::to_string),
            state,
        })?;
        let env = self.build(
            signer,
            kind::THREAD_UPDATE,
            content,
            Some(thread_id),
            vec![],
        )?;
        self.commit(&env)
    }

    /// Attach a project (by `repo_key`, else by path) with its workspace.
    pub fn attach_project(
        &self,
        signer: &Keypair,
        thread_id: &str,
        repo_key: Option<&str>,
        path_hint: &Path,
        ws: &ThreadWorkspace,
    ) -> Result<(), Error> {
        self.require_thread(thread_id)?;
        let op = wire::ThreadProjectOp {
            op: "attach".into(),
            repo_key: repo_key.map(str::to_string),
            path_hint: Some(path_hint.to_string_lossy().into_owned()),
            workspace_kind: Some(ws.kind),
            branch: ws.branch.clone(),
            worktree_path: ws
                .worktree_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        };
        let env = self.build(
            signer,
            kind::THREAD_PROJECT,
            serde_json::to_string(&op)?,
            Some(thread_id),
            vec![],
        )?;
        self.commit(&env)
    }

    /// Detach a project by its `repo_key` or its path hint.
    pub fn detach_project(
        &self,
        signer: &Keypair,
        thread_id: &str,
        repo_key_or_path: &str,
    ) -> Result<(), Error> {
        self.require_thread(thread_id)?;
        let op = wire::ThreadProjectOp {
            op: "detach".into(),
            repo_key: Some(repo_key_or_path.to_string()),
            path_hint: Some(repo_key_or_path.to_string()),
            ..Default::default()
        };
        let env = self.build(
            signer,
            kind::THREAD_PROJECT,
            serde_json::to_string(&op)?,
            Some(thread_id),
            vec![],
        )?;
        self.commit(&env)
    }

    /// Let `pubkey` act on the thread.  `signer` must be an owner.
    pub fn add_participant(
        &self,
        signer: &Keypair,
        thread_id: &str,
        pubkey: &str,
        role: ParticipantRole,
    ) -> Result<(), Error> {
        self.require_thread(thread_id)?;
        let content = serde_json::to_string(&wire::ParticipantOp {
            op: "add".into(),
            role: Some(role),
        })?;
        let env = self.build(
            signer,
            kind::THREAD_PARTICIPANT,
            content,
            Some(thread_id),
            vec![tag2(tag::MENTION, pubkey)],
        )?;
        self.commit(&env)
    }

    /// Remove `pubkey` from the thread.  `signer` must be an owner, or `pubkey` itself.
    pub fn remove_participant(
        &self,
        signer: &Keypair,
        thread_id: &str,
        pubkey: &str,
    ) -> Result<(), Error> {
        self.require_thread(thread_id)?;
        let content = serde_json::to_string(&wire::ParticipantOp {
            op: "remove".into(),
            role: None,
        })?;
        let env = self.build(
            signer,
            kind::THREAD_PARTICIPANT,
            content,
            Some(thread_id),
            vec![tag2(tag::MENTION, pubkey)],
        )?;
        self.commit(&env)
    }

    /// Start a session on a thread.  Mints the session id (a ULID); `signer`
    /// is the agent, or its owner acting on its behalf.
    pub fn start_session(&self, signer: &Keypair, s: &NewSession) -> Result<Session, Error> {
        self.require_thread(&s.thread_id)?;
        let session_id = new_id();
        let content = serde_json::to_string(&wire::SessionStart {
            environment_id: s.environment_id.clone(),
            environment_label: s.environment_label.clone(),
            project_repo_key: s.project_repo_key.clone(),
            harness: s.harness.clone(),
            runtime_mode: s.runtime_mode,
            resumed_from: s.resumed_from.clone(),
            runtime: s.runtime,
        })?;
        let env = self.build(
            signer,
            kind::SESSION_START,
            content,
            Some(&s.thread_id),
            vec![
                tag2(tag::SESSION, &session_id),
                tag2(tag::MENTION, &s.agent_pubkey),
            ],
        )?;
        self.commit(&env)?;
        self.require_session(&session_id)
    }

    /// Change session fields; `None`s are left alone.
    pub fn update_session(
        &self,
        signer: &Keypair,
        session_id: &str,
        u: &SessionUpdate,
    ) -> Result<(), Error> {
        let session = self.require_session(session_id)?;
        let content = serde_json::to_string(&wire::SessionUpdateWire {
            state: u.state,
            harness_session_id: u.harness_session_id.clone(),
            transcript_path: u.transcript_path.clone(),
            rex: u.rex.as_ref().map(|(s, w, b)| wire::RexIds {
                session: s.clone(),
                window: w.clone(),
                block: b.clone(),
            }),
            runtime_mode: u.runtime_mode,
        })?;
        let env = self.build(
            signer,
            kind::SESSION_UPDATE,
            content,
            Some(&session.thread_id),
            vec![tag2(tag::SESSION, session_id)],
        )?;
        self.commit(&env)
    }

    /// End a session.
    pub fn end_session(
        &self,
        signer: &Keypair,
        session_id: &str,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let session = self.require_session(session_id)?;
        let content = serde_json::to_string(&wire::SessionEnd {
            reason: reason.map(str::to_string),
        })?;
        let env = self.build(
            signer,
            kind::SESSION_END,
            content,
            Some(&session.thread_id),
            vec![tag2(tag::SESSION, session_id)],
        )?;
        self.commit(&env)
    }

    /// Record a session's live status (what hooks and OSC 7501 report).
    pub fn record_status(
        &self,
        signer: &Keypair,
        session_id: &str,
        s: &StatusReport,
    ) -> Result<(), Error> {
        let session = self.require_session(session_id)?;
        let env = self.build(
            signer,
            kind::STATUS,
            serde_json::to_string(s)?,
            Some(&session.thread_id),
            vec![tag2(tag::SESSION, session_id)],
        )?;
        self.commit(&env)
    }

    /// Add a usage delta to a session and return its new totals.
    /// `transcript_offset` is local bookkeeping (how far the transcript reader
    /// got) and is stored on the session row, never in the envelope.
    pub fn add_usage(
        &self,
        signer: &Keypair,
        session_id: &str,
        delta: &UsageDelta,
        transcript_offset: Option<u64>,
    ) -> Result<Usage, Error> {
        let session = self.require_session(session_id)?;
        let content = serde_json::to_string(&wire::UsageWire::from(delta))?;
        let env = self.build(
            signer,
            kind::USAGE,
            content,
            Some(&session.thread_id),
            vec![tag2(tag::SESSION, session_id), tag2(tag::NONCE, &new_id())],
        )?;
        self.commit(&env)?;
        if let Some(off) = transcript_offset {
            self.set_transcript_offset(session_id, off)?;
        }
        Ok(self.usage(session_id)?.unwrap_or(Usage {
            session_id: session_id.to_string(),
            ..Default::default()
        }))
    }

    /// Note how full a session's context window is (local-only: a snapshot
    /// that each report replaces, never shared).
    pub fn set_session_context(&self, session_id: &str, used: u64, size: u64) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO session_context (session_id, used, size, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id) DO UPDATE SET used = excluded.used, size = excluded.size, updated_at = excluded.updated_at",
            params![session_id, used as i64, size as i64, now()],
        )?;
        Ok(())
    }

    /// Note how much of a plan window `harness`'s account has used: each
    /// report replaces the last for that window (local-only).
    pub fn set_plan_limit(&self, harness: &Harness, window: LimitWindow, used_pct: f64, resets_at: Option<i64>) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO plan_limits (harness, window, used_pct, resets_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (harness, window) DO UPDATE SET used_pct = excluded.used_pct, resets_at = excluded.resets_at, updated_at = excluded.updated_at",
            params![harness.as_str(), window.as_str(), used_pct.clamp(0.0, 100.0), resets_at, now()],
        )?;
        Ok(())
    }

    /// Share a completed turn of a managed session (kind 40).  Only the
    /// summary travels; conversation items stay on this machine.  The
    /// assistant text is cut to 2000 characters.
    pub fn record_turn(
        &self,
        signer: &Keypair,
        session_id: &str,
        t: &TurnSummary,
    ) -> Result<(), Error> {
        let session = self.require_session(session_id)?;
        let mut summary = t.clone();
        if summary.assistant_text.chars().count() > 2000 {
            summary.assistant_text = summary.assistant_text.chars().take(2000).collect();
        }
        let env = self.build(
            signer,
            kind::TURN,
            serde_json::to_string(&summary)?,
            Some(&session.thread_id),
            vec![tag2(tag::SESSION, session_id)],
        )?;
        self.commit(&env)
    }

    /// Set (or clear) the local-only control socket of a managed session.
    pub fn set_session_socket(
        &self,
        session_id: &str,
        socket_path: Option<&Path>,
    ) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE sessions SET socket_path = ?2 WHERE id = ?1",
            params![
                session_id,
                socket_path.map(|p| p.to_string_lossy().into_owned())
            ],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("session {session_id}")));
        }
        Ok(())
    }

    /// Set the local-only transcript read offset of a session.
    pub fn set_transcript_offset(&self, session_id: &str, offset: u64) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE sessions SET transcript_offset = ?2 WHERE id = ?1",
            params![session_id, offset as i64],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("session {session_id}")));
        }
        Ok(())
    }
}
