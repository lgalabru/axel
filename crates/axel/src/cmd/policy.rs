//! Enforcing agent policies (reach + budget) at the CLI's entry points: who is
//! asking (`$AXEL_SESSION` / `$AXEL_AGENT` of an agent's processes, else the
//! owner), and human-readable refusals that say how to grant access.

use anyhow::{Result, bail};
use axel_store::{Refusal, Reach, SummonCheck, local_day_start};

use super::Ctx;

thread_local! {
    /// Who is asking, when this process acts for an agent that isn't its
    /// own caller: syncing delivers a remote agent's message here, and the
    /// summons it causes must be checked as that agent's session.
    static ACTING_AS: std::cell::RefCell<Option<(String, Option<String>)>> = const { std::cell::RefCell::new(None) };
}

/// Until the guard drops, policy checks treat `agent` (in `session`) as the
/// caller instead of this process's own `$AXEL_AGENT` / `$AXEL_SESSION`.
pub fn act_as(agent: &str, session: Option<&str>) -> ActingAs {
    let prev = ACTING_AS.with(|a| a.replace(Some((agent.to_string(), session.map(String::from)))));
    ActingAs(prev)
}

pub struct ActingAs(Option<(String, Option<String>)>);

impl Drop for ActingAs {
    fn drop(&mut self) {
        let prev = self.0.take();
        ACTING_AS.with(|a| *a.borrow_mut() = prev);
    }
}

/// The session an agent's process runs in, if this command was run by one.
pub fn caller_session() -> Option<String> {
    if let Some((_, s)) = ACTING_AS.with(|a| a.borrow().clone()) {
        return s;
    }
    std::env::var("AXEL_SESSION").ok().filter(|s| !s.trim().is_empty())
}

/// The agent running this command, if any (`$AXEL_AGENT`).
pub fn caller_agent() -> Option<String> {
    if let Some((a, _)) = ACTING_AS.with(|a| a.borrow().clone()) {
        return Some(a);
    }
    std::env::var("AXEL_AGENT").ok().filter(|s| !s.trim().is_empty())
}

/// Display name of a pubkey: the agent's name, else a short prefix.
pub fn name_of(ctx: &Ctx, pk: &str) -> String {
    ctx.store
        .agent_by_pubkey(pk)
        .ok()
        .flatten()
        .map(|a| a.name)
        .unwrap_or_else(|| pk.chars().take(8).collect::<String>() + "…")
}

/// Why a summon was refused, with the command that would allow it.
pub fn describe(ctx: &Ctx, r: &Refusal) -> String {
    match r {
        Refusal::NotReachable { from, to } => {
            let (f, t) = (name_of(ctx, from), name_of(ctx, to));
            format!("{f} may not reach {t}; allow it with `axel agent edit {f} --allow {t}`")
        }
        Refusal::Cycle { chain, to } => {
            let path: Vec<String> = chain.iter().map(|pk| name_of(ctx, pk)).collect();
            format!(
                "{} is already in this summon chain ({} → {}); refusing a loop",
                name_of(ctx, to),
                path.join(" → "),
                name_of(ctx, to)
            )
        }
        Refusal::TooDeep { agent, depth, max } => {
            let n = name_of(ctx, agent);
            format!("the summon chain would reach depth {depth}, past {n}'s limit of {max}; raise it with `axel agent edit {n} --max-depth N`")
        }
        Refusal::OverBudget { agent, spent, limit } => {
            let n = name_of(ctx, agent);
            format!("{n} has spent ${spent:.2} today and reached its ${limit:.2} daily budget; raise it with `axel agent edit {n} --daily-usd N`")
        }
    }
}

thread_local! {
    /// Agents the owner just let the caller reach, for one send (see [`grant_reach_once`]).
    static REACH_GRANTED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// While the returned guard lives, the caller may reach `pubkeys` (message,
/// and summon if need be): the owner allowed this one message.
pub fn grant_reach_once(pubkeys: &[String]) -> ReachGrant {
    REACH_GRANTED.with(|g| g.borrow_mut().extend(pubkeys.iter().cloned()));
    ReachGrant(pubkeys.to_vec())
}

/// Ends a [`grant_reach_once`] when dropped.
pub struct ReachGrant(Vec<String>);

impl Drop for ReachGrant {
    fn drop(&mut self) {
        REACH_GRANTED.with(|g| g.borrow_mut().retain(|pk| !self.0.contains(pk)));
    }
}

fn reach_granted(pubkey: &str) -> bool {
    REACH_GRANTED.with(|g| g.borrow().iter().any(|p| p == pubkey))
}

/// Check that a new session of `target` may start, from whoever is asking.
pub fn check_summon(ctx: &Ctx, target: &str) -> Result<(Option<String>, Option<axel_store::RuntimeMode>)> {
    match ctx.store.check_summon_with(caller_session().as_deref(), target, local_day_start(), reach_granted(target))? {
        SummonCheck::Allowed { parent, mode_cap } => Ok((parent, mode_cap)),
        SummonCheck::Refused(r) => bail!("{}", describe(ctx, &r)),
    }
}

/// Check that the agent sending a message may reach every recipient.  The
/// owner (no `$AXEL_AGENT`) may message anyone; an agent may always message
/// itself and the owner (recipients here are agents only), and may answer
/// any agent allowed to reach it: if alice may ask bob, bob may reply.
/// Replying never summons — starting a session still checks reach.
pub fn check_message(ctx: &Ctx, recipients: &[String]) -> Result<()> {
    let Some(from) = caller_agent() else { return Ok(()) };
    let denied: Vec<String> = denied_reach(ctx, &from, recipients)?.iter().map(|r| name_of(ctx, r)).collect();
    if denied.is_empty() {
        return Ok(());
    }
    bail!("{}", axel_mcp::SendError::NoReach { from: name_of(ctx, &from), to: denied })
}

/// The recipients (pubkeys) `from` may not message: not in its reach, and not
/// agents that may reach it (whom it may always answer).
pub fn denied_reach(ctx: &Ctx, from: &str, recipients: &[String]) -> Result<Vec<String>> {
    let reach = ctx.store.agent_policy(from)?.reach;
    let mut denied = Vec::new();
    for r in recipients.iter().filter(|r| **r != from && !reach.allows(r) && !reach_granted(r)) {
        if !ctx.store.agent_policy(r)?.reach.allows(from) {
            denied.push(r.clone());
        }
    }
    Ok(denied)
}

/// One-line description of a reach, with names.
pub fn reach_label(ctx: &Ctx, reach: &Reach) -> String {
    match reach {
        Reach::Nobody => "nobody (only you)".into(),
        Reach::Anyone => "any agent".into(),
        Reach::Only(pks) => pks.iter().map(|pk| name_of(ctx, pk)).collect::<Vec<_>>().join(", "),
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;

    #[test]
    fn a_one_time_grant_lasts_as_long_as_its_guard() {
        assert!(!reach_granted("pk-atlas"));
        {
            let _g = grant_reach_once(&["pk-atlas".to_string()]);
            assert!(reach_granted("pk-atlas"));
            assert!(!reach_granted("pk-nova"), "only who the owner allowed");
        }
        assert!(!reach_granted("pk-atlas"), "gone with the guard");
    }
}
