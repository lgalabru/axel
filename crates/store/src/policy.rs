//! Agent policies: who an agent may reach, and how much it may spend.
//!
//! Local-only, like `agents`: the owner sets them on this machine and they are
//! enforced here, at the two places one agent acts on another — `axel send`
//! (messaging) and launching a session (summoning).  An agent's teams add a
//! reach rule of their own (RFD 0005); [`EffectiveReach`] combines the two.  An agent without a row has
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

use crate::{Error, RuntimeMode, SECONDS_PER_DAY, Store, Team, TeamReach, now};

/// How deep a summon chain may go by default (owner → a → b → c).
pub const DEFAULT_MAX_DEPTH: u32 = 3;

/// The most `session_links` a chain walk follows.  Far past any depth a
/// policy would allow (`DEFAULT_MAX_DEPTH` is 3), so it only stops a link
/// table that a bug or a hand edit turned into a cycle.
const SUMMON_CHAIN_LINKS_MAX: usize = 64;

/// The depth of a session summoned from one at `parent_depth`.
fn child_depth(parent_depth: u32) -> Result<u32, Error> {
    parent_depth
        .checked_add(1)
        .ok_or_else(|| Error::Corrupt(format!("summon depth {parent_depth} has no successor")))
}

/// Which agents an agent may message or summon.  The owner is always reachable.
///
/// `Team` and `Lead` defer to the agent's teams (RFD 0005); the agents they
/// list are allowed on top, as `Only` lists them.  See [`EffectiveReach`] for
/// how an agent's own reach and its teams' rules combine.
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
    /// The other members of its teams, and these agents.
    Team(Vec<String>),
    /// In each of its teams, the lead if it is a member, every member if it
    /// is the lead; and these agents.
    Lead(Vec<String>),
}

impl Reach {
    /// Whether this reach names `pubkey`, or is `Anyone`.  Says nothing of
    /// teams: [`Store::may_reach`] is the whole answer.
    pub fn allows(&self, pubkey: &str) -> bool {
        match self {
            Reach::Nobody => false,
            Reach::Anyone => true,
            Reach::Only(pks) | Reach::Team(pks) | Reach::Lead(pks) => pks.iter().any(|p| p == pubkey),
        }
    }

    /// The agents named one by one (empty for `Nobody` and `Anyone`).
    pub fn listed(&self) -> &[String] {
        match self {
            Reach::Nobody | Reach::Anyone => &[],
            Reach::Only(pks) | Reach::Team(pks) | Reach::Lead(pks) => pks,
        }
    }

    /// Add one agent (turns `Nobody` into `Only`; `Anyone` stays).
    pub fn allow(&mut self, pubkey: &str) {
        match self {
            Reach::Anyone => {}
            Reach::Nobody => *self = Reach::Only(vec![pubkey.to_string()]),
            Reach::Only(pks) | Reach::Team(pks) | Reach::Lead(pks) => {
                if !pks.iter().any(|p| p == pubkey) {
                    pks.push(pubkey.to_string());
                }
            }
        }
    }

    /// Remove one agent (an emptied `Only` becomes `Nobody`; `Anyone` can't
    /// exclude one agent, so it is left alone and `false` is returned).
    pub fn disallow(&mut self, pubkey: &str) -> bool {
        match self {
            Reach::Anyone => false,
            Reach::Nobody => true,
            Reach::Team(pks) | Reach::Lead(pks) => {
                pks.retain(|p| p != pubkey);
                true
            }
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

/// What one team's rule lets an agent reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamReachPart {
    pub team_id: String,
    pub team_name: String,
    pub reach: TeamReach,
    /// The lead's profile id, when the team has one.
    pub lead: Option<String>,
    /// The profiles this rule lets the agent reach.
    pub profiles: Vec<String>,
}

/// An agent's reach once its teams are counted (RFD 0005, "Policy
/// template"), with where each part came from.
///
/// - Its own reach's *rule* is `Anyone`, `Team` or `Lead`.  `Nobody` and
///   `Only` set no rule: `Nobody` is the default, and Axel can't tell an
///   untouched agent from one set to nobody on purpose.
/// - Its teams' rule is the union of every live team with a rule other
///   than `none`: a member of two teams reaches into both.
/// - With both, the stricter wins (both must allow); with one, that one;
///   with neither, nobody.
/// - The agents its own reach lists by name (`--allow`) are allowed on top:
///   the owner named each one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveReach {
    /// The agent's own reach, as set with `axel agent edit`.
    pub own: Reach,
    /// When `own` is `Team` or `Lead`: the profiles that rule reaches,
    /// across every live team of the agent.
    pub own_profiles: Vec<String>,
    /// The teams whose rule applies.
    pub teams: Vec<TeamReachPart>,
}

impl EffectiveReach {
    /// Whether the agent may reach `target` (an instance pubkey), an
    /// instance of `target_profile`.
    pub fn allows(&self, target: &str, target_profile: Option<&str>) -> bool {
        if self.own.listed().iter().any(|p| p == target) {
            return true;
        }
        let in_profiles = |profiles: &[String]| target_profile.is_some_and(|t| profiles.iter().any(|p| p == t));
        let own_rule = match &self.own {
            Reach::Nobody | Reach::Only(_) => None,
            Reach::Anyone => Some(true),
            Reach::Team(_) | Reach::Lead(_) => Some(in_profiles(&self.own_profiles)),
        };
        let team_rule = (!self.teams.is_empty()).then(|| self.teams.iter().any(|t| in_profiles(&t.profiles)));
        match (own_rule, team_rule) {
            (None, None) => false,
            (Some(own), None) => own,
            (None, Some(teams)) => teams,
            (Some(own), Some(teams)) => own && teams,
        }
    }

    /// Whether any team's rule takes part.
    pub fn from_teams(&self) -> bool {
        !self.teams.is_empty() || matches!(self.own, Reach::Team(_) | Reach::Lead(_))
    }
}

/// The profiles `me` may reach in `team` under `rule`.
fn team_targets(team: &Team, me: &str, rule: TeamReach) -> Vec<String> {
    debug_assert!(team.has_member(me), "only a member's reach comes from a team");
    let others = || team.members.iter().map(|m| m.profile_id.clone()).filter(|p| p != me).collect();
    match rule {
        TeamReach::None => Vec::new(),
        TeamReach::Team => others(),
        TeamReach::Lead => match team.lead.as_deref() {
            Some(lead) if lead == me => others(),
            Some(lead) if team.has_member(lead) => vec![lead.to_string()],
            // A lead rule with no lead reaches no one: fail closed.
            _ => Vec::new(),
        },
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
        // Local midnight can be skipped by a DST change; a day back is then
        // a near enough start for a daily spending window.
        .unwrap_or_else(|| now.timestamp() - SECONDS_PER_DAY)
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

    /// `pubkey`'s reach once its teams are counted.  An instance this
    /// replica doesn't know has no profile, so no team applies to it.
    pub fn effective_reach(&self, pubkey: &str) -> Result<EffectiveReach, Error> {
        let own = self.agent_policy(pubkey)?.reach;
        let Some(agent) = self.agent_by_pubkey(pubkey)? else {
            return Ok(EffectiveReach { own, ..Default::default() });
        };
        let teams = self.teams_of_profile(&agent.profile_id)?;
        let me = agent.profile_id.as_str();
        let own_rule = match own {
            Reach::Team(_) => Some(TeamReach::Team),
            Reach::Lead(_) => Some(TeamReach::Lead),
            Reach::Nobody | Reach::Anyone | Reach::Only(_) => None,
        };
        let mut own_profiles: Vec<String> = Vec::new();
        if let Some(rule) = own_rule {
            for team in &teams {
                for profile in team_targets(team, me, rule) {
                    if !own_profiles.contains(&profile) {
                        own_profiles.push(profile);
                    }
                }
            }
        }
        let parts: Vec<TeamReachPart> = teams
            .iter()
            .filter(|t| t.reach != TeamReach::None)
            .map(|t| TeamReachPart {
                team_id: t.team_id.clone(),
                team_name: t.name.clone(),
                reach: t.reach,
                lead: t.lead.clone(),
                profiles: team_targets(t, me, t.reach),
            })
            .collect();
        debug_assert!(parts.iter().all(|p| !p.profiles.iter().any(|q| q == me)), "a rule never lists the agent itself");
        Ok(EffectiveReach { own, own_profiles, teams: parts })
    }

    /// May `from` message or summon `to` (instance pubkeys)?  Its own
    /// reach and its teams' rules together; see [`EffectiveReach`].
    pub fn may_reach(&self, from: &str, to: &str) -> Result<bool, Error> {
        let reach = self.effective_reach(from)?;
        let target_profile = self.agent_by_pubkey(to)?.map(|a| a.profile_id);
        Ok(reach.allows(to, target_profile.as_deref()))
    }

    /// Record that `child` was summoned from `parent`; returns the child's depth.
    pub fn link_session(&self, child: &str, parent: &str) -> Result<u32, Error> {
        let depth = child_depth(self.session_depth(parent)?)?;
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
        let mut current = Some(session_id.to_string());
        // Bounded walk: a corrupt link table can't hang us.
        for _ in 0..SUMMON_CHAIN_LINKS_MAX {
            let Some(id) = current.take() else { break };
            if let Some(session) = self.get_session(&id)? {
                chain.push(session.agent_pubkey);
            }
            current = self
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
            "SELECT COALESCE(SUM(u.cost_usd_est), 0) FROM effective_usage_log u JOIN sessions s ON s.id = u.session_id
             WHERE s.agent_pubkey = ?1 AND u.at >= ?2",
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
        if !reach_granted && !self.may_reach(&from, target)? {
            return Ok(SummonCheck::Refused(Refusal::NotReachable { from, to: target.to_string() }));
        }
        let depth = child_depth(self.session_depth(caller)?)?;
        if depth > from_policy.budget.max_depth {
            return Ok(SummonCheck::Refused(Refusal::TooDeep { agent: from, depth, max: from_policy.budget.max_depth }));
        }
        Ok(SummonCheck::Allowed { parent: Some(caller.to_string()), mode_cap: Some(parent.runtime_mode) })
    }
}
