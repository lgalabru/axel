//! `axel agent …`

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_identity::{Keypair, attest};
use axel_store::{Agent, AgentColor, AgentPolicy, Effort, Harness, NewAgent, Reach, RuntimeMode, SpendLimit};

use super::{Ctx, claude_profile::ClaudeProfile};
use super::policy::reach_label;
use super::text::{pad, pad_left};
use crate::cli::{AgentColorArg, AgentCommand, EffortArg, PolicyArgs, ReachArg, RuntimeModeArg};

pub fn run(cmd: AgentCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    match cmd {
        AgentCommand::New { name, runs, harness, model, claude_config_dir, effort, skills, prompt, color, runtime_mode, host, policy } => {
            new_agent(&ctx, NewArgs { name, runs, harness, model, claude_config_dir, effort, skills, prompt, color, runtime_mode, host, policy })
        }
        AgentCommand::Edit { name, runs, model, claude_config_dir, inherit_claude_config, effort, name_to, skills, prompt, color, runtime_mode, host, policy } => {
            edit_agent(&ctx, EditArgs { name, runs, model, claude_config_dir, inherit_claude_config, effort, name_to, skills, prompt, color, runtime_mode, host, policy })
        }
        AgentCommand::Ls { json } => list(&ctx, json),
        AgentCommand::Show { name } => show(&ctx, &name),
        AgentCommand::Archive { name } => {
            let a = ctx.store.agent_by_name(&name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
            ctx.store.archive_agent(&a.pubkey)?;
            println!("{}", t!("archived {name}", name = a.name));
            Ok(())
        }
    }
}

/// `axel agent new`'s arguments.
struct NewArgs {
    name: String,
    runs: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    claude_config_dir: Option<String>,
    effort: Option<EffortArg>,
    skills: String,
    prompt: Option<String>,
    color: Option<AgentColorArg>,
    runtime_mode: RuntimeModeArg,
    host: Option<String>,
    policy: PolicyArgs,
}

/// `axel agent edit`'s arguments.
struct EditArgs {
    name: String,
    runs: Option<String>,
    model: Option<String>,
    claude_config_dir: Option<String>,
    inherit_claude_config: bool,
    effort: Option<EffortArg>,
    name_to: Option<String>,
    skills: Option<String>,
    prompt: Option<String>,
    color: Option<AgentColorArg>,
    runtime_mode: Option<RuntimeModeArg>,
    host: Option<String>,
    policy: PolicyArgs,
}

fn new_agent(ctx: &Ctx, args: NewArgs) -> Result<()> {
    let NewArgs { name, runs, harness, model, claude_config_dir, effort, skills, prompt, color, runtime_mode, host, policy } = args;
    if super::policy::caller_agent().is_some() {
        bail!("agents can't create agents; ask the owner to run this");
    }
    // Validate the policy before creating anything.
    let mut p = AgentPolicy::default();
    apply_policy(ctx, &name, &mut p, &policy)?;
    let host_id = host.as_deref().map(|h| host_choice(ctx, h)).transpose()?.flatten();
    let (harness, model) = harness_and_model(runs.as_deref().or(harness.as_deref()), model)?;
    let profile = claude_config_dir.as_deref().map(|dir| ClaudeProfile::parse(dir, &harness)).transpose()?;
    if profile.is_some() && host_id.is_some() {
        bail!("{}", t!("agents with a Claude config directory run locally"));
    }
    let agent = create(ctx, &name, harness, model, parse_skills(&skills), prompt, color.map(AgentColor::from), runtime_mode.into())?;
    ctx.store.set_agent_policy(&agent.pubkey, &p)?;
    ctx.store.set_agent_host(&agent.pubkey, host_id.as_deref())?;
    let effort: Option<Effort> = effort.and_then(Into::into);
    if effort.is_some() {
        ctx.store.set_agent_effort(&agent.pubkey, effort)?;
    }
    if let Some(profile) = profile {
        profile.save(&agent.pubkey)?;
    }
    println!("{}  {}  {}", agent.name, runs_label(&agent.harness, agent.model.as_deref()), agent.pubkey);
    print_policy(ctx, &p);
    print_effort(&agent.harness, effort);
    print_host(ctx, &agent.pubkey)?;
    Ok(())
}

fn edit_agent(ctx: &Ctx, args: EditArgs) -> Result<()> {
    let EditArgs { name, runs, model, claude_config_dir, inherit_claude_config, effort, name_to, skills, prompt, color, runtime_mode, host, policy } = args;
    if super::policy::caller_agent().is_some() {
        bail!("agents can't edit agents; ask the owner to run this");
    }
    let a = ctx.store.agent_by_name(&name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
    let harness_change = runs.as_deref().map(|spec| harness_and_model(Some(spec), model.clone())).transpose()?;
    let harness = harness_change.as_ref().map_or(&a.harness, |(harness, _)| harness);
    let new_profile = claude_config_dir.as_deref().map(|dir| ClaudeProfile::parse(dir, harness)).transpose()?;
    let current_profile = ClaudeProfile::load(&a.pubkey)?;
    let selected_profile = new_profile.as_ref().or(current_profile.as_ref()).filter(|_| !inherit_claude_config);
    let host_id = host.as_deref().map(|h| host_choice(ctx, h)).transpose()?;
    if selected_profile.is_some() && *harness != Harness::Claude {
        bail!("{}", t!("use --inherit-claude-config when switching to a different harness"));
    }
    if selected_profile.is_some() && match &host_id {
        Some(host) => host.is_some(),
        None => ctx.store.agent_host(&a.pubkey)?.is_some(),
    } {
        bail!("{}", t!("agents with a Claude config directory run locally"));
    }
    let mut p = ctx.store.agent_policy(&a.pubkey)?;
    apply_policy(ctx, &a.name, &mut p, &policy)?;
    ctx.store.set_agent_policy(&a.pubkey, &p)?;
    if let Some(new_name) = &name_to {
        rename(ctx, &a, new_name)?;
    }
    if skills.is_some() || prompt.is_some() || color.is_some() {
        let skills = skills.as_deref().map(parse_skills);
        ctx.store.set_agent_profile(&a.pubkey, skills.as_deref(), prompt.as_deref(), color.map(AgentColor::from))?;
    }
    if let Some(m) = runtime_mode {
        ctx.store.set_agent_runtime_mode(&a.pubkey, m.into())?;
    }
    if runs.is_some() || model.is_some() {
        let (harness, model) = harness_change
            .unwrap_or_else(|| (a.harness.clone(), model.filter(|m| m != "default")));
        ctx.store.set_agent_harness(&a.pubkey, &harness, model.as_deref())?;
    }
    if let Some(effort) = effort {
        ctx.store.set_agent_effort(&a.pubkey, effort.into())?;
    }
    if let Some(host_id) = host_id {
        ctx.store.set_agent_host(&a.pubkey, host_id.as_deref())?;
    }
    if let Some(profile) = new_profile {
        profile.save(&a.pubkey)?;
    } else if inherit_claude_config {
        ClaudeProfile::clear(&a.pubkey)?;
    }
    let a = ctx.store.agent_by_pubkey(&a.pubkey)?.context(t!("the agent vanished"))?;
    println!("{}", t!("{name}  {runs}  mode {mode}", name = a.name, runs = runs_label(&a.harness, a.model.as_deref()), mode = a.runtime_mode));
    print_policy(ctx, &p);
    print_effort(&a.harness, a.effort);
    print_host(ctx, &a.pubkey)?;
    Ok(())
}

/// Give `agent` a new name, unless it already has it or another agent does.
fn rename(ctx: &Ctx, agent: &Agent, new_name: &str) -> Result<()> {
    let new_name = valid_name(new_name)?;
    if new_name == agent.name {
        return Ok(());
    }
    if ctx.store.agent_by_name(new_name)?.is_some() {
        bail!("{}", t!("an agent named {name} already exists", name = new_name));
    }
    ctx.store.rename_agent(&agent.pubkey, new_name)?;
    println!("{}", t!("{old} is now {new}; sessions already running keep calling it {old} until they restart", old = agent.name, new = new_name));
    Ok(())
}

fn list(ctx: &Ctx, json: bool) -> Result<()> {
    let agents = ctx.store.list_agents()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&agents)?);
        return Ok(());
    }
    for a in agents {
        // An unreadable inbox shows as none unread; the rest of the row still helps.
        let unread = ctx.store.unread_count(&a.pubkey).unwrap_or(0);
        let p = ctx.store.agent_policy(&a.pubkey)?;
        let host = ctx.store.agent_host(&a.pubkey)?.map(|h| format!("@{}", h.label)).unwrap_or_default();
        // Padded by terminal columns: names and the translated cells may be Chinese.
        let unread = t!("{n} unread", n = pad_left(&unread.to_string(), 3));
        let reach = t!("reach {who}", who = pad(&reach_label(ctx, &p.reach), 18));
        println!(
            "{} {} {} {} {} {} {unread}  {reach} {}",
            pad(&a.name, 16),
            pad(&host, 6),
            pad(&a.harness.to_string(), 9),
            pad(a.model.as_deref().unwrap_or("-"), 10),
            pad(a.effort.map_or("-", Effort::as_str), 6),
            pad(&a.runtime_mode.to_string(), 10),
            a.pubkey
        );
    }
    Ok(())
}

fn show(ctx: &Ctx, name: &str) -> Result<()> {
    let a = ctx.store.agent_by_name(name)?.with_context(|| t!("no agent named {name}", name = format!("{name:?}")))?;
    println!("{}", serde_json::to_string_pretty(&a)?);
    if let Some(profile) = ClaudeProfile::load(&a.pubkey)? {
        println!("{}", t!("Claude config directory: {dir}", dir = profile.label()));
    }
    let has_key = ctx.keystore.agent(&a.pubkey.parse()?)?.is_some();
    println!("{}", if has_key { t!("key in keystore: yes") } else { t!("key in keystore: MISSING") });
    let p = ctx.store.agent_policy(&a.pubkey)?;
    print_policy(ctx, &p);
    for line in super::policy::team_reach_lines(ctx, &ctx.store.effective_reach(&a.pubkey)?) {
        println!("  {line}");
    }
    print_effort(&a.harness, a.effort);
    print_host(ctx, &a.pubkey)?;
    let spent = ctx.store.agent_spend_since(&a.pubkey, axel_store::local_day_start())?;
    println!("{}", t!("spent today: ${spent} (estimated; managed sessions don't report usage yet)", spent = format!("{spent:.2}")));
    if let Some(tag) = &a.auth_tag {
        let tag = axel_identity::AuthTag::from_tag(tag)?;
        let ok = axel_identity::verify_attestation(&tag, &a.pubkey.parse()?).is_ok();
        let line = if ok { t!("owner attestation: valid (owner {owner})", owner = tag.owner) } else { t!("owner attestation: INVALID (owner {owner})", owner = tag.owner) };
        println!("{line}");
    }
    print_instances(ctx, &a)?;
    let me = ctx.store.principal(&ctx.owner()?.public().to_base58())?;
    if ctx.store.name_clashes(&me)?.iter().any(|clash| clash[0].name == a.name) {
        println!("{}", t!("another agent of yours is also named {name}: settle it with `axel device merge {name}`", name = a.name));
    }
    Ok(())
}

/// Where the agent's profile runs: one line per instance (RFD 0004).
fn print_instances(ctx: &Ctx, a: &axel_store::Agent) -> Result<()> {
    for instance in ctx.store.profile_instances(&a.profile_id)? {
        let place = match &instance.device {
            None => t!("this machine").to_string(),
            Some(device) => ctx.store.device(device)?.map_or_else(|| device.clone(), |d| d.label),
        };
        println!("{}", t!("instance {key} on {place}", key = instance.pubkey, place = place));
    }
    Ok(())
}

/// An agent name as people type it: letters, digits, `-` and `_`, so it
/// works as an `@mention`.
pub(super) fn valid_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        bail!("{}", t!("agent names are letters, digits, - and _ so they work as @mentions"));
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
        bail!("{}", t!("an agent named {name} already exists", name = name));
    }
    let kp = Keypair::generate();
    ctx.keystore.put_agent(&kp).context(t!("storing the agent key"))?;
    let owner = ctx.owner()?;
    let tag = attest(&owner, &kp.public(), "").context(t!("attesting the agent"))?;
    let agent = ctx.store.insert_agent(&NewAgent {
        pubkey: kp.public().to_string(),
        name: name.to_string(),
        harness,
        model,
        effort: None,
        skills,
        prompt,
        color,
        runtime_mode,
        auth_tag: Some(tag.to_tag()),
        profile_id: None,
        device: None,
    })?;
    Ok(agent)
}

/// The harness and model from `HARNESS[/MODEL]` (or `--harness`) and
/// `--model`; claude when neither names one.  A model in both places must agree.
fn harness_and_model(spec: Option<&str>, flag: Option<String>) -> Result<(Harness, Option<String>)> {
    let (harness, from_spec) = spec.map(axel_store::parse_harness_model).unwrap_or_default();
    let model = match (from_spec, flag) {
        (Some(a), Some(b)) if a != b => bail!("{}", t!("two models: {a} (from {spec}) and --model {b}", a = a, spec = spec.unwrap_or_default(), b = b)),
        (a, b) => a.or(b),
    };
    if !harness.is_builtin() && harness.program().is_empty() {
        bail!("{}", t!("no harness named"));
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
            ReachArg::Team => Reach::Team(Vec::new()),
            ReachArg::Lead => Reach::Lead(Vec::new()),
        };
    }
    let resolve = |n: &str| -> Result<String> {
        let n = n.trim().trim_start_matches('@');
        if n.eq_ignore_ascii_case(me) {
            bail!("{}", t!("{me} always reaches itself; --allow/--disallow take other agents", me = me));
        }
        Ok(ctx.store.agent_by_name(n)?.with_context(|| t!("no agent named {name}", name = format!("{n:?}")))?.pubkey)
    };
    for n in args.allow.iter().filter(|n| !n.trim().is_empty()) {
        let pk = resolve(n)?;
        p.reach.allow(&pk);
    }
    for n in args.disallow.iter().filter(|n| !n.trim().is_empty()) {
        let pk = resolve(n)?;
        if !p.reach.disallow(&pk) {
            bail!("{}", t!("{me} can reach any agent; use --reach nobody --allow … to list who it may reach", me = me));
        }
    }
    if let Some(v) = &args.daily_usd {
        p.budget.spend = match v.trim().to_ascii_lowercase().as_str() {
            "unlimited" | "none" | "off" => SpendLimit::Unlimited,
            n => {
                let usd: f64 = n.trim_start_matches('$').parse().with_context(|| t!("--daily-usd takes a number or `unlimited`, not {value}", value = v))?;
                if !(usd.is_finite() && usd >= 0.0) {
                    bail!("{}", t!("--daily-usd must be zero or more"));
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
    let h = ctx.store.host(label)?.with_context(|| t!("no host named {quoted}; add it with `axel host add {label}`", quoted = format!("{label:?}"), label = label))?;
    Ok(Some(h.id))
}

fn print_host(ctx: &Ctx, agent: &str) -> Result<()> {
    match ctx.store.agent_host(agent)? {
        Some(h) => println!("  {}", t!("runs on {host} (`@name@local` or `--host local` runs it here)", host = h.label)),
        None => println!("  {}", t!("runs here")),
    }
    Ok(())
}

fn print_policy(ctx: &Ctx, p: &AgentPolicy) {
    let spend = match p.budget.spend {
        SpendLimit::Unlimited => t!("unlimited").to_string(),
        SpendLimit::DailyUsd(usd) => t!("${usd}/day", usd = format!("{usd:.2}")),
    };
    println!("  {}", t!("reach   {who}", who = reach_label(ctx, &p.reach)));
    println!("  {}", t!("budget  {spend} · summon depth ≤ {depth}", spend = spend, depth = p.budget.max_depth));
}

/// The effort when the agent has one, and a warning where its harness
/// cannot be started at it.
fn print_effort(harness: &Harness, effort: Option<Effort>) {
    if let Some(effort) = effort {
        println!("  {}", t!("effort  {effort}", effort = effort));
        if let Some(warning) = super::effort::warning(harness, effort) {
            println!("  {}", t!("warning: {warning}", warning = warning));
        }
    }
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
