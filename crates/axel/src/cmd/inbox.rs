//! `axel send` and `axel inbox`.

use anyhow::{Context, Result, bail};
use axel_inbox::{EnvelopeBuilder, Profile, kind, resolve_mentions};
use axel_mcp::{Delivery, Outcome, SendReport};

use super::Ctx;

pub fn send(text: &str) -> Result<()> {
    let ctx = Ctx::open()?;
    let agents = ctx.store.list_agents()?;
    let profiles: Vec<Profile> = agents.iter().map(|a| Profile { pubkey: a.pubkey.clone(), name: a.name.clone() }).collect();
    let recipients = resolve_mentions(text, &profiles);
    if recipients.is_empty() {
        bail!("no recipients: mention at least one agent with @name");
    }
    let hosts = host_hints(text, &agents);
    let report = send_message(&ctx, &recipients, &hosts, text)?;
    let names: Vec<&str> = report.deliveries.iter().map(|d| d.to.as_str()).collect();
    println!("sent {} to {}", &report.message_id[..12.min(report.message_id.len())], names.join(", "));
    let mut failed = Vec::new();
    for d in &report.deliveries {
        match &d.outcome {
            Outcome::Failed { .. } => {
                eprintln!("{}: {}", d.to, d.outcome.describe());
                failed.push(d.to.clone());
            }
            o => println!("{}: {}", d.to, o.describe()),
        }
    }
    if !failed.is_empty() {
        bail!("could not deliver to {} (the message is in their inbox)", failed.join(", "));
    }
    Ok(())
}

/// `(agent pubkey, host label)`: where an agent should run if it is summoned.
pub type HostHints = Vec<(String, String)>;

/// Recipients named in `to` (names, `@` optional, `name@host` to run it on
/// a host) as pubkeys, and the hosts asked for.
pub fn resolve_names(ctx: &Ctx, to: &[String]) -> Result<(Vec<String>, HostHints)> {
    let mut out: Vec<String> = Vec::new();
    let mut hosts = Vec::new();
    for n in to {
        let n = n.trim().trim_start_matches('@');
        let (name, host) = match n.split_once('@') {
            Some((a, h)) => (a, Some(h)),
            None => (n, None),
        };
        let a = ctx.store.agent_by_name(name)?.with_context(|| format!("no agent named {name:?}; see list_agents"))?;
        if let Some(h) = host {
            hosts.push((a.pubkey.clone(), h.to_string()));
        }
        if !out.contains(&a.pubkey) {
            out.push(a.pubkey);
        }
    }
    Ok((out, hosts))
}

/// `@name@host` in a message: run that agent on that host if it has to be
/// summoned.  Returns `(pubkey, host)` pairs.
fn host_hints(text: &str, agents: &[axel_store::Agent]) -> HostHints {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut out = Vec::new();
    for (i, _) in text.match_indices('@') {
        if i > 0 && text[..i].chars().next_back().is_some_and(|c| word(c) || c == '@') {
            continue;
        }
        let rest = &text[i + 1..];
        let name: String = rest.chars().take_while(|c| word(*c)).collect();
        let Some(after) = rest[name.len()..].strip_prefix('@') else { continue };
        let host: String = after.chars().take_while(|c| word(*c) || *c == '.').collect();
        if let (false, Some(a)) = (host.is_empty(), agents.iter().find(|a| a.name.eq_ignore_ascii_case(&name))) {
            out.push((a.pubkey.clone(), host));
        }
    }
    out
}

/// `axel send` and the MCP `send` tool: check policy, sign and post one
/// message to `recipients` (pubkeys), then get it in front of each of them
/// on the caller's thread.  A policy refusal is an error; a delivery that
/// fails is reported per recipient (the message is in its inbox regardless).
pub fn send_message(ctx: &Ctx, recipients: &[String], hosts: &[(String, String)], text: &str) -> Result<SendReport> {
    super::policy::check_message(ctx, recipients)?;
    let thread_id = std::env::var("AXEL_THREAD").ok().filter(|t| !t.trim().is_empty());
    let from_agent = super::policy::caller_agent();
    if let (Some(t), Some(_)) = (&thread_id, &from_agent) {
        check_chatter(ctx, t)?;
    }
    // Sender: the agent we're running as, else the owner.
    let signer = match &from_agent {
        Some(pk) => ctx.agent_signer(pk, thread_id.as_deref())?,
        None => ctx.owner()?,
    };
    let mut b = EnvelopeBuilder::new(kind::NOTE, text);
    for r in recipients {
        b = b.mention(r);
    }
    if let Some(t) = &thread_id {
        b = b.thread(t);
    }
    if let Some(s) = super::policy::caller_session() {
        b = b.session(&s);
    }
    let env = ctx.store.sign_with_attestation(&signer, b, thread_id.as_deref())?;
    ctx.store.post(&env)?;
    let sender = from_agent.as_ref().map(|pk| axel_acp::socket::Sender { pubkey: pk.clone(), name: super::policy::name_of(ctx, pk) });
    // An agent working here for another machine (its key isn't here) can
    // reach the sessions on this host; others get it when that machine syncs.
    let on_remote_host = from_agent.as_ref().is_some_and(|pk| pk.parse().ok().and_then(|k| ctx.keystore.agent(&k).ok().flatten()).is_none());
    let deliveries = recipients
        .iter()
        .filter(|r| Some(*r) != from_agent.as_ref())
        .map(|r| {
            let host = hosts.iter().find(|(pk, _)| pk == r).map(|(_, h)| h.as_str());
            let outcome = match &thread_id {
                None => Outcome::InboxOnly,
                Some(t) => deliver(ctx, t, r, host, on_remote_host, text, sender.clone())
                    .unwrap_or_else(|e| Outcome::Failed { error: format!("{e:#}") }),
            };
            Delivery { to: super::policy::name_of(ctx, r), outcome }
        })
        .collect();
    Ok(SendReport { message_id: env.id, thread: thread_id, deliveries })
}

/// Agent-to-agent messages a thread takes before the owner must speak again.
/// Summon chains are bounded by depth; this bounds agents that are already
/// running from talking to each other forever.
pub const MAX_AGENT_MESSAGES: usize = 20;

/// Refuse when agents have sent [`MAX_AGENT_MESSAGES`] on `thread_id` since
/// the owner last wrote there.
fn check_chatter(ctx: &Ctx, thread_id: &str) -> Result<()> {
    let owner = ctx.owner()?.public().to_base58();
    let since_ms = ctx
        .store
        .thread_items(thread_id, 2000)?
        .iter()
        .filter(|it| it.kind == axel_store::ItemKind::UserMessage && it.content.get("from").is_none())
        .map(|it| it.at)
        .max()
        .unwrap_or(0);
    let sent = ctx
        .store
        .thread_messages(thread_id)?
        .iter()
        .filter(|e| e.from != owner && (e.created_at as i64) * 1000 >= since_ms)
        .count();
    if sent >= MAX_AGENT_MESSAGES {
        bail!(
            "agents have sent {sent} messages on this thread since the owner last wrote; stop and wait for the owner (the limit is {MAX_AGENT_MESSAGES})"
        );
    }
    Ok(())
}

/// Get `text` in front of `recipient` on `thread_id`: into its live managed
/// session (queued if it is mid-turn), or by summoning it onto the thread.
///
/// `host` asks for the agent to be summoned on that host; `on_remote_host`
/// means this machine only runs delegated work, so nothing is summoned here.
fn deliver(
    ctx: &Ctx,
    thread_id: &str,
    recipient: &str,
    host: Option<&str>,
    on_remote_host: bool,
    text: &str,
    from: Option<axel_acp::socket::Sender>,
) -> Result<Outcome> {
    let live = ctx.store.list_sessions(axel_store::SessionFilter {
        thread_id: Some(thread_id.to_string()),
        agent_pubkey: Some(recipient.to_string()),
        ..Default::default()
    })?;
    let live: Vec<_> = live.into_iter().filter(|s| s.ended_at.is_none()).collect();
    let prompt = axel_acp::socket::Command::Prompt { text: text.to_string(), attachments: vec![], steer: false, from: from.clone() };
    if !on_remote_host
        && let Some(s) = live.iter().find(|s| s.runtime == axel_store::SessionRuntime::Managed && super::host::host_of(ctx, s).ok().flatten().is_some())
    {
        return match super::host::relay(ctx, s, &prompt)? {
            axel_acp::socket::Reply::Queued { .. } => Ok(Outcome::Queued),
            axel_acp::socket::Reply::Error { message } => bail!("{message}"),
            _ => Ok(Outcome::Delivered),
        };
    }
    if let Some(sock) = live.iter().filter(|s| s.runtime == axel_store::SessionRuntime::Managed).find_map(|s| s.socket_path.clone()) {
        let cmd = prompt;
        return match axel_acp::socket::send(&sock, &cmd) {
            Ok(axel_acp::socket::Reply::Queued { .. }) => Ok(Outcome::Queued),
            Ok(axel_acp::socket::Reply::Error { message }) => bail!("{message}"),
            Ok(_) => Ok(Outcome::Delivered),
            Err(e) => bail!("its session host is unreachable ({e})"),
        };
    }
    if on_remote_host {
        // Not running here: the owner's machine delivers it when it syncs.
        return Ok(Outcome::AwaitingSync);
    }
    if !live.is_empty() {
        // A pane session: Axel can't type into a harness it doesn't drive.
        return Ok(Outcome::InPane);
    }
    let name = super::policy::name_of(ctx, recipient);
    let target = match host {
        Some(h) => format!("{name}@{h}"),
        None => name,
    };
    let session = super::ui::summon(ctx, thread_id, &target, text, &[])?;
    Ok(Outcome::Summoned { session })
}

/// A message an agent on `host` sent, just pulled from it: the host could
/// reach only the sessions it runs, so get it in front of the recipients
/// that live elsewhere, under the sender's policy, as if it were sending
/// from here.  Returns a line per recipient for the sync log.
pub fn deliver_from_host(ctx: &Ctx, host: &axel_store::Environment, env: &axel_inbox::Envelope) -> Vec<String> {
    let Some(thread_id) = env.thread_id() else { return vec![] };
    let author = axel_store::author(env).to_string();
    let sender_name = super::policy::name_of(ctx, &author);
    let _acting = super::policy::act_as(&author, env.session_id());
    let mut out = Vec::new();
    for r in env.mentions() {
        if r == author || !ours(ctx, r) {
            continue;
        }
        // Already handled where it was sent: the recipient runs there.
        let there = ctx
            .store
            .list_sessions(axel_store::SessionFilter {
                thread_id: Some(thread_id.to_string()),
                agent_pubkey: Some(r.to_string()),
                ..Default::default()
            })
            .unwrap_or_default()
            .into_iter()
            .any(|s| s.ended_at.is_none() && s.environment_id == host.id);
        if there {
            continue;
        }
        let name = super::policy::name_of(ctx, r);
        let outcome = super::policy::check_message(ctx, &[r.to_string()]).and_then(|()| {
            let from = axel_acp::socket::Sender { pubkey: author.clone(), name: sender_name.clone() };
            deliver(ctx, thread_id, r, None, false, &env.content, Some(from))
        });
        out.push(match outcome {
            Ok(o) => format!("{sender_name} → {name}: {}", o.describe()),
            Err(e) => format!("{sender_name} → {name}: {e:#}"),
        });
    }
    out
}

/// An agent whose key is on this machine: it runs here unless told otherwise.
fn ours(ctx: &Ctx, pubkey: &str) -> bool {
    pubkey.parse().ok().and_then(|k| ctx.keystore.agent(&k).ok().flatten()).is_some()
}

/// The newest messages addressed to the caller (`$AXEL_AGENT`, else the owner).
pub fn inbox_messages(ctx: &Ctx, limit: usize) -> Result<Vec<axel_mcp::InboxMessage>> {
    let owner_pk = ctx.owner()?.public().to_string();
    let me = super::policy::caller_agent().unwrap_or_else(|| owner_pk.clone());
    Ok(ctx
        .store
        .inbox(&me, limit)?
        .into_iter()
        .map(|m| axel_mcp::InboxMessage {
            from: if m.from == owner_pk { "owner".into() } else { super::policy::name_of(ctx, &m.from) },
            thread: m.tags.iter().find(|t| t.first().map(String::as_str) == Some("t")).and_then(|t| t.get(1).cloned()),
            at: m.created_at,
            id: m.id,
            text: m.content,
        })
        .collect())
}

pub fn show(agent: Option<&str>, json: bool) -> Result<()> {
    let ctx = Ctx::open()?;
    let pk = match agent {
        Some(name) => ctx.store.agent_by_name(name)?.with_context(|| format!("no agent named {name:?}"))?.pubkey,
        None => ctx.owner()?.public().to_string(),
    };
    let msgs = ctx.store.inbox(&pk, 50)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&msgs)?);
        return Ok(());
    }
    let agents = ctx.store.list_agents()?;
    let owner_pk = ctx.owner()?.public().to_string();
    let name_of = |pk: &str| {
        if pk == owner_pk { return "you".to_string(); }
        agents.iter().find(|a| a.pubkey == pk).map(|a| a.name.clone()).unwrap_or_else(|| format!("{}…", &pk[..8]))
    };
    for m in msgs {
        let when = chrono::DateTime::from_timestamp(m.created_at as i64, 0).map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string()).unwrap_or_default();
        println!("{when}  {:<12} {:<18} {}", name_of(&m.from), kind::name(m.kind), m.content.lines().next().unwrap_or(""));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> axel_store::Agent {
        axel_store::Agent {
            pubkey: format!("pk-{name}"),
            name: name.into(),
            harness: axel_store::Harness::Claude,
            model: None,
            skills: vec![],
            prompt: None,
            color: None,
            runtime_mode: Default::default(),
            auth_tag: None,
            created_at: 0,
            last_seen_at: None,
            archived_at: None,
        }
    }

    #[test]
    fn name_at_host_mentions_pick_the_host() {
        let agents = [agent("atlas"), agent("nova")];
        assert_eq!(
            host_hints("@atlas@mini fix the test, and @nova review it", &agents),
            vec![("pk-atlas".to_string(), "mini".to_string())]
        );
        assert!(host_hints("mail me at ludo@mini.local", &agents).is_empty(), "not a mention");
        assert!(host_hints("@nobody@mini hi", &agents).is_empty(), "unknown agent");
        assert_eq!(host_hints("(@Nova@studio.lan)", &agents), vec![("pk-nova".to_string(), "studio.lan".to_string())]);
    }
}
