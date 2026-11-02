//! Agent policies: who an agent may reach, and how much it may spend.
//!
//! Local-only, like `agents`: the owner sets them on this machine and they are
//! enforced here, at the two places one agent acts on another — `axel send`
//! (messaging) and launching a session (summoning).  An agent without a row has
//! [`AgentPolicy::default`]: it reaches nobody, spends without limit, and may
//! start summon chains up to [`DEFAULT_MAX_DEPTH`] deep.
//!
//! Summon chains are recorded in `session_links` (child → parent, depth), so a
//! session started by another agent's session knows its ancestry.  That is
//! what stops loops: an agent already in the chain can't be summoned again,
//! and a chain can't grow past the summoner's `max_depth`.  A summoned session
//! also never runs in a broader mode than the session that summoned it.
//!
//! This guards against runaway agents, not hostile ones: agents run as the
//! owner's user and could bypass Axel entirely.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, RuntimeMode, Store, now};

/// How deep a summon chain may go by default (owner → a → b → c).
pub const DEFAULT_MAX_DEPTH: u32 = 3;

/// Which agents an agent may message or summon.  The owner is always reachable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "agents", rename_all = "snake_case")]
pub enum Reach {
    /// No other agent (the default).
    #[default]
    Nobody,
    /// Every agent.
    Anyone,
    /// These agents, by pubkey.
    Only(Vec<String>),
}

impl Reach {
    pub fn allows(&self, pubkey: &str) -> bool {
        match self {
            Reach::Nobody => false,
            Reach::Anyone => true,
            Reach::Only(pks) => pks.iter().any(|p| p == pubkey),
        }
    }

    /// Add one agent (turns `Nobody` into `Only`; `Anyone` stays).
    pub fn allow(&mut self, pubkey: &str) {
        match self {
            Reach::Anyone => {}
            Reach::Nobody => *self = Reach::Only(vec![pubkey.to_string()]),
            Reach::Only(pks) => {
                if !pks.iter().any(|p| p == pubkey) {
                    pks.push(pubkey.to_string());
                }
            }
        }
    }

    /// Remove one agent (an emptied list becomes `Nobody`; `Anyone` can't
    /// exclude one agent, so it is left alone and `false` is returned).
    pub fn disallow(&mut self, pubkey: &str) -> bool {
        match self {
            Reach::Anyone => false,
            Reach::Nobody => true,
            Reach::Only(pks) => {
                pks.retain(|p| p != pubkey);
                if pks.is_empty() {
                    *self = Reach::Nobody;
                }
                true
            }
        }
    }
}

/// A spending limit, in estimated USD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "usd", rename_all = "snake_case")]
pub enum SpendLimit {
    #[default]
    Unlimited,
    /// Per local calendar day, across all of the agent's sessions.
    DailyUsd(f64),
}

/// What an agent may consume.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub spend: SpendLimit,
    /// The deepest summon chain this agent may extend: a session at depth
    /// `d` (owner-started = 0) can summon only while `d + 1 <= max_depth`.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_DEPTH
}

impl Default for Budget {
    fn default() -> Self {
        Budget { spend: SpendLimit::Unlimited, max_depth: DEFAULT_MAX_DEPTH }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentPolicy {
    pub reach: Reach,
    pub budget: Budget,
}

/// Why a summon was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// The summoning agent may not reach the target.
    NotReachable { from: String, to: String },
    /// The target is already in the summon chain (pubkeys, root first).
    Cycle { chain: Vec<String>, to: String },
    /// The chain would get deeper than the summoner (`agent`) allows.
    TooDeep { agent: String, depth: u32, max: u32 },
    /// The target has spent its budget for today.
    OverBudget { agent: String, spent: f64, limit: f64 },
}

/// The outcome of [`Store::check_summon`].
#[derive(Clone, Debug, PartialEq)]
pub enum SummonCheck {
    Allowed {
        /// The summoning session, recorded as the new session's parent.
        parent: Option<String>,
        /// Mode the new session must not exceed (the parent session's mode).
        mode_cap: Option<RuntimeMode>,
    },
    Refused(Refusal),
}

/// Breadth of a runtime mode: what it lets an agent do without asking.
fn breadth(m: RuntimeMode) -> u8 {
    match m {
        RuntimeMode::Plan => 0,
        RuntimeMode::Supervised => 1,
        RuntimeMode::Edits => 2,
        RuntimeMode::Autopilot => 3,
    }
}

/// The narrower of two modes (a summoned session never escalates).
pub fn narrower(a: RuntimeMode, b: RuntimeMode) -> RuntimeMode {
    if breadth(a) <= breadth(b) { a } else { b }
}

/// Start of the local calendar day, in Unix seconds.
pub fn local_day_start() -> i64 {
    let now = chrono::Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(chrono::Local).earliest())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| now.timestamp() - 86_400)
}

impl Store {
    /// An agent's policy, or the default when none was set.
    pub fn agent_policy(&self, pubkey: &str) -> Result<AgentPolicy, Error> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT reach_json, budget_json FROM agent_policies WHERE pubkey = ?1",
                [pubkey],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            None => AgentPolicy::default(),
            Some((reach, budget)) => AgentPolicy {
                reach: serde_json::from_str(&reach)?,
                budget: serde_json::from_str(&budget)?,
            },
        })
    }

    pub fn set_agent_policy(&self, pubkey: &str, policy: &AgentPolicy) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO agent_policies (pubkey, reach_json, budget_json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(pubkey) DO UPDATE SET reach_json = ?2, budget_json = ?3, updated_at = ?4",
            params![
                pubkey,
                serde_json::to_string(&policy.reach)?,
                serde_json::to_string(&policy.budget)?,
                now()
            ],
        )?;
        Ok(())
    }

    /// Record that `child` was summoned from `parent`; returns the child's depth.
    pub fn link_session(&self, child: &str, parent: &str) -> Result<u32, Error> {
        let depth = self.session_depth(parent)? + 1;
        self.conn.execute(
            "INSERT OR REPLACE INTO session_links (session_id, parent_session_id, depth) VALUES (?1, ?2, ?3)",
            params![child, parent, depth],
        )?;
        Ok(depth)
    }

    /// 0 for a session the owner started.
    pub fn session_depth(&self, session_id: &str) -> Result<u32, Error> {
        Ok(self
            .conn
            .query_row("SELECT depth FROM session_links WHERE session_id = ?1", [session_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// Agent pubkeys of a session and its ancestors, root first.
    pub fn summon_chain(&self, session_id: &str) -> Result<Vec<String>, Error> {
        let mut chain = Vec::new();
        let mut cur = Some(session_id.to_string());
        // Bounded walk: a corrupt link table can't hang us.
        for _ in 0..64 {
            let Some(id) = cur.take() else { break };
            if let Some(s) = self.get_session(&id)? {
                chain.push(s.agent_pubkey);
            }
            cur = self
                .conn
                .query_row("SELECT parent_session_id FROM session_links WHERE session_id = ?1", [&id], |r| r.get(0))
                .optional()?;
        }
        chain.reverse();
        Ok(chain)
    }

    /// Estimated USD an agent's sessions recorded since `since` (Unix seconds).
    pub fn agent_spend_since(&self, pubkey: &str, since: i64) -> Result<f64, Error> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(u.cost_usd_est), 0) FROM usage u JOIN sessions s ON s.id = u.session_id
             WHERE s.agent_pubkey = ?1 AND u.updated_at >= ?2",
            params![pubkey, since],
            |r| r.get(0),
        )?)
    }

    /// May a new session of `target` start?  `caller` is the session asking
    /// (`$AXEL_SESSION` of an agent), or `None` when the owner asks: the owner
    /// is only held to the target's spend limit.
    pub fn check_summon(&self, caller: Option<&str>, target: &str, day_start: i64) -> Result<SummonCheck, Error> {
        self.check_summon_with(caller, target, day_start, false)
    }

    /// [`Self::check_summon`], with `reach_granted` when the owner just let
    /// this one summon past the caller's reach; loops, depth and budget still apply.
    pub fn check_summon_with(&self, caller: Option<&str>, target: &str, day_start: i64, reach_granted: bool) -> Result<SummonCheck, Error> {
        let target_policy = self.agent_policy(target)?;
        if let SpendLimit::DailyUsd(limit) = target_policy.budget.spend {
            let spent = self.agent_spend_since(target, day_start)?;
            if spent >= limit {
                return Ok(SummonCheck::Refused(Refusal::OverBudget { agent: target.to_string(), spent, limit }));
            }
        }
        let Some(caller) = caller else {
            return Ok(SummonCheck::Allowed { parent: None, mode_cap: None });
        };
        let Some(parent) = self.get_session(caller)? else {
            // Stale $AXEL_SESSION (another DB, a deleted session): treat as the owner.
            return Ok(SummonCheck::Allowed { parent: None, mode_cap: None });
        };
        let from = parent.agent_pubkey.clone();
        let chain = self.summon_chain(caller)?;
        if chain.iter().any(|pk| pk == target) {
            return Ok(SummonCheck::Refused(Refusal::Cycle { chain, to: target.to_string() }));
        }
        let from_policy = self.agent_policy(&from)?;
        if !reach_granted && !from_policy.reach.allows(target) {
            return Ok(SummonCheck::Refused(Refusal::NotReachable { from, to: target.to_string() }));
        }
        let depth = self.session_depth(caller)? + 1;
        if depth > from_policy.budget.max_depth {
            return Ok(SummonCheck::Refused(Refusal::TooDeep { agent: from, depth, max: from_policy.budget.max_depth }));
        }
        Ok(SummonCheck::Allowed { parent: Some(caller.to_string()), mode_cap: Some(parent.runtime_mode) })
    }
}
