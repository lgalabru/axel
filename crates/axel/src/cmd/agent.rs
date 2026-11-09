//! `axel agent …`

use anyhow::{Context, Result, bail};
use axel_identity::{Keypair, attest};
use axel_store::{Agent, AgentColor, AgentPolicy, Harness, NewAgent, Reach, RuntimeMode, SpendLimit};

use super::Ctx;
use super::policy::reach_label;
use crate::cli::{AgentCommand, PolicyArgs, ReachArg};

pub fn run(cmd: AgentCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    match cmd {
        AgentCommand::New { name, runs, harness, model, skills, prompt, color, runtime_mode, host, policy } => {
            if super::policy::caller_agent().is_some() {
                bail!("agents can't create agents; ask the owner to run this");
            }
            // Validate the policy before creating anything.
            let mut p = AgentPolicy::default();
            apply_policy(&ctx, &name, &mut p, &policy)?;
            let host_id = host.as_deref().map(|h| host_choice(&ctx, h)).transpose()?.flatten();
            let (harness, model) = harness_and_model(runs.as_deref().or(harness.as_deref()), model)?;
            let agent = create(&ctx, &name, harness, model, parse_skills(&skills), prompt, color.map(AgentColor::from), runtime_mode.into())?;
            ctx.store.set_agent_policy(&agent.pubkey, &p)?;
            ctx.store.set_agent_host(&agent.pubkey, host_id.as_deref())?;
            println!("{}  {}  {}", agent.name, runs_label(&agent.harness, agent.model.as_deref()), agent.pubkey);
            print_policy(&ctx, &p);
            print_host(&ctx, &agent.pubkey)?;
            Ok(())
        }
        AgentCommand::Edit { name, runs, model, name_to, skills, prompt, color, runtime_mode, host, policy } => {
            if super::policy::caller_agent().is_some() {
                bail!("agents can't edit agents; ask the owner to run this");
            }
            let a = ctx.store.agent_by_name(&name)?.with_context(|| format!("no agent named {name:?}"))?;
            let mut p = ctx.store.agent_policy(&a.pubkey)?;
            apply_policy(&ctx, &a.name, &mut p, &policy)?;
            ctx.store.set_agent_policy(&a.pubkey, &p)?;
            if let Some(new_name) = &name_to {
                let new_name = valid_name(new_name)?;
                if new_name != a.name {
                    if ctx.store.agent_by_name(new_name)?.is_some() {
                        bail!("an agent named {new_name} already exists");
                    }
                    ctx.store.rename_agent(&a.pubkey, new_name)?;
                    println!("{} is now {new_name}; sessions already running keep calling it {} until they restart", a.name, a.name);
                }
            }
            if skills.is_some() || prompt.is_some() || color.is_some() {
                let skills = skills.as_deref().map(parse_skills);
                ctx.store.set_agent_profile(&a.pubkey, skills.as_deref(), prompt.as_deref(), color.map(AgentColor::from))?;
            }
            if let Some(m) = runtime_mode {
                ctx.store.set_agent_runtime_mode(&a.pubkey, m.into())?;
            }
            if runs.is_some() || model.is_some() {
                let (harness, model) = match runs.as_deref() {
                    Some(spec) => harness_and_model(Some(spec), model)?,
                    // A new model, same harness.
                    None => (a.harness.clone(), model.filter(|m| m != "default")),
                };
                ctx.store.set_agent_harness(&a.pubkey, &harness, model.as_deref())?;
            }
            if let Some(h) = &host {
                ctx.store.set_agent_host(&a.pubkey, host_choice(&ctx, h)?.as_deref())?;
            }
            let a = ctx.store.agent_by_pubkey(&a.pubkey)?.context("agent vanished")?;
            println!("{}  {}  mode {}", a.name, runs_label(&a.harness, a.model.as_deref()), a.runtime_mode);
            print_policy(&ctx, &p);
            print_host(&ctx, &a.pubkey)?;
            Ok(())
        }
        AgentCommand::Ls { json } => {
            let agents = ctx.store.list_agents()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&agents)?);
            } else {
                for a in agents {
                    let unread = ctx.store.unread_count(&a.pubkey).unwrap_or(0);
                    let p = ctx.store.agent_policy(&a.pubkey)?;
                    let host = ctx.store.agent_host(&a.pubkey)?.map(|h| format!("@{}", h.label)).unwrap_or_default();
                    println!(
                        "{:<16} {:<6} {:<9} {:<10} {:<10} {:>3} unread  reach {:<18} {}",
                        a.name,
                        host,
                        a.harness,
                        a.model.as_deref().unwrap_or("-"),
                        a.runtime_mode,
                        unread,
                        reach_label(&ctx, &p.reach),
                        a.pubkey
                    );
                }
            }
            Ok(())
        }
        AgentCommand::Show { name } => {
            let a = ctx.store.agent_by_name(&name)?.with_context(|| format!("no agent named {name:?}"))?;
            println!("{}", serde_json::to_string_pretty(&a)?);
            let has_key = ctx.keystore.agent(&a.pubkey.parse()?)?.is_some();
            println!("key in keystore: {}", if has_key { "yes" } else { "MISSING" });
            let p = ctx.store.agent_policy(&a.pubkey)?;
            print_policy(&ctx, &p);
            print_host(&ctx, &a.pubkey)?;
            let spent = ctx.store.agent_spend_since(&a.pubkey, axel_store::local_day_start())?;
            println!("spent today: ${spent:.2} (estimated; managed sessions don't report usage yet)");
            if let Some(tag) = &a.auth_tag {
                let tag = axel_identity::AuthTag::from_tag(tag)?;
                let ok = axel_identity::verify_attestation(&tag, &a.pubkey.parse()?).is_ok();
                println!("owner attestation: {} (owner {})", if ok { "valid" } else { "INVALID" }, tag.owner);
            }
            Ok(())
        }
        AgentCommand::Archive { name } => {
            let a = ctx.store.agent_by_name(&name)?.with_context(|| format!("no agent named {name:?}"))?;
            ctx.store.archive_agent(&a.pubkey)?;
            println!("archived {}", a.name);
            Ok(())
        }
    }
}

/// An agent name as people type it: letters, digits, `-` and `_`, so it
/// works as an `@mention`.
fn valid_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        bail!("agent names are [A-Za-z0-9_-]+ so they work as @mentions");
    }
    Ok(name)
}

/// Create an agent: key → keystore → owner attestation → store row.
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &Ctx,
    name: &str,
    harness: Harness,
    model: Option<String>,
    skills: Vec<String>,
    prompt: Option<String>,
    color: Option<AgentColor>,
    runtime_mode: RuntimeMode,
) -> Result<Agent> {
    let name = valid_name(name)?;
    if ctx.store.agent_by_name(name)?.is_some() {
        bail!("an agent named {name} already exists");
    }
    let kp = Keypair::generate();
    ctx.keystore.put_agent(&kp).context("storing agent key")?;
    let owner = ctx.owner()?;
    let tag = attest(&owner, &kp.public(), "").context("attesting agent")?;
    let agent = ctx.store.insert_agent(&NewAgent {
        pubkey: kp.public().to_string(),
        name: name.to_string(),
        harness,
        model,
        skills,
        prompt,
        color,
        runtime_mode,
        auth_tag: Some(tag.to_tag()),
    })?;
    Ok(agent)
}

/// The harness and model from `HARNESS[/MODEL]` (or `--harness`) and
/// `--model`; claude when neither names one.  A model in both places must agree.
fn harness_and_model(spec: Option<&str>, flag: Option<String>) -> Result<(Harness, Option<String>)> {
    let (harness, from_spec) = spec.map(axel_store::parse_harness_model).unwrap_or_default();
    let model = match (from_spec, flag) {
        (Some(a), Some(b)) if a != b => bail!("two models: {a} (from {}) and --model {b}", spec.unwrap_or_default()),
        (a, b) => a.or(b),
    };
    if !harness.is_builtin() && harness.program().is_empty() {
        bail!("no harness named");
    }
    Ok((harness, model))
}

/// `claude/opus`, or `claude` with the harness's default model.
fn runs_label(harness: &Harness, model: Option<&str>) -> String {
    match model {
        Some(m) => format!("{harness}/{m}"),
        None => harness.to_string(),
    }
}

/// Apply `agent new`/`agent edit` policy flags: `--reach` first (replaces the
/// list), then `--allow`, then `--disallow`.
fn apply_policy(ctx: &Ctx, me: &str, p: &mut AgentPolicy, args: &PolicyArgs) -> Result<()> {
    let touches = args.reach.is_some() || !args.allow.is_empty() || !args.disallow.is_empty() || args.daily_usd.is_some() || args.max_depth.is_some();
    if touches && super::policy::caller_agent().is_some() {
        bail!("agents can't change permissions or budgets; ask the owner to run this");
    }
    if let Some(r) = args.reach {
        p.reach = match r {
            ReachArg::Nobody => Reach::Nobody,
            ReachArg::Anyone => Reach::Anyone,
        };
    }
    let resolve = |n: &str| -> Result<String> {
        let n = n.trim().trim_start_matches('@');
        if n.eq_ignore_ascii_case(me) {
            bail!("{me} always reaches itself; --allow/--disallow take other agents");
        }
        Ok(ctx.store.agent_by_name(n)?.with_context(|| format!("no agent named {n:?}"))?.pubkey)
    };
    for n in args.allow.iter().filter(|n| !n.trim().is_empty()) {
        let pk = resolve(n)?;
        p.reach.allow(&pk);
    }
    for n in args.disallow.iter().filter(|n| !n.trim().is_empty()) {
        let pk = resolve(n)?;
        if !p.reach.disallow(&pk) {
            bail!("{me} can reach any agent; use --reach nobody --allow … to list who it may reach");
        }
    }
    if let Some(v) = &args.daily_usd {
        p.budget.spend = match v.trim().to_ascii_lowercase().as_str() {
            "unlimited" | "none" | "off" => SpendLimit::Unlimited,
            n => {
                let usd: f64 = n.trim_start_matches('$').parse().with_context(|| format!("--daily-usd takes a number or `unlimited`, not {v}"))?;
                if !(usd.is_finite() && usd >= 0.0) {
                    bail!("--daily-usd must be zero or more");
                }
                SpendLimit::DailyUsd(usd)
            }
        };
    }
    if let Some(d) = args.max_depth {
        p.budget.max_depth = d;
    }
    Ok(())
}

/// `--host`: a known host's id, or `None` for `local`.
fn host_choice(ctx: &Ctx, label: &str) -> Result<Option<String>> {
    if super::host::is_here(ctx, label)? {
        return Ok(None);
    }
    let h = ctx.store.host(label)?.with_context(|| format!("no host named {label:?}; add it with `axel host add {label}`"))?;
    Ok(Some(h.id))
}

fn print_host(ctx: &Ctx, agent: &str) -> Result<()> {
    match ctx.store.agent_host(agent)? {
        Some(h) => println!("  runs on {} (`@name@local` or `--host local` runs it here)", h.label),
        None => println!("  runs here"),
    }
    Ok(())
}

fn print_policy(ctx: &Ctx, p: &AgentPolicy) {
    let spend = match p.budget.spend {
        SpendLimit::Unlimited => "unlimited".to_string(),
        SpendLimit::DailyUsd(usd) => format!("${usd:.2}/day"),
    };
    println!("  reach   {}", reach_label(ctx, &p.reach));
    println!("  budget  {spend} · summon depth ≤ {}", p.budget.max_depth);
}

fn parse_skills(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_and_model_come_from_the_argument_or_the_flags() {
        assert_eq!(harness_and_model(None, None).unwrap(), (Harness::Claude, None), "claude, its default model");
        assert_eq!(harness_and_model(Some("claude/opus"), None).unwrap(), (Harness::Claude, Some("opus".into())));
        assert_eq!(harness_and_model(Some("codex"), Some("gpt-5.5".into())).unwrap(), (Harness::Codex, Some("gpt-5.5".into())));
        assert_eq!(harness_and_model(Some("claude/opus"), Some("opus".into())).unwrap(), (Harness::Claude, Some("opus".into())), "the same model twice is fine");
        let two = harness_and_model(Some("claude/opus"), Some("fable".into())).unwrap_err().to_string();
        assert!(two.contains("two models"), "{two}");
        assert!(harness_and_model(Some("  "), None).is_err(), "a blank harness");
    }

    #[test]
    fn an_agent_shows_as_harness_slash_model() {
        assert_eq!(runs_label(&Harness::Claude, Some("opus")), "claude/opus");
        assert_eq!(runs_label(&Harness::Goose, None), "goose");
    }
}
