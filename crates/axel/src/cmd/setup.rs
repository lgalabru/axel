//! `axel setup`: the first-run walkthrough.  `axel` runs it by itself the first
//! time, when there is no agent yet.  It checks this machine (Rex, the harness
//! CLIs, their ACP adapters), offers to install what's missing, and creates the
//! first agent.  Dialoguer, themed like the TUI: coral red accent and values,
//! dim hints.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use axel_i18n::t;
use axel_store::{AgentColor, Harness, RuntimeMode};
use console::{Style, Term, style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use super::Ctx;
use super::launch::which;

// The TUI's palette (`crates/tui/src/theme.rs`).
const ACCENT: console::Color = console::Color::TrueColor(255, 117, 101);
const YOU: console::Color = ACCENT;
const OK: u8 = 108;
const DANGER: u8 = 167;
const WARN: u8 = 179;
const DIM: u8 = 244;
/// The mascot's spectrum, for the wordmark: sun, tangerine, pink, violet, blue.
const SPECTRUM: [u8; 5] = [220, 208, 205, 135, 33];

/// Dialoguer's colourful theme, recoloured to match the TUI.
fn theme() -> ColorfulTheme {
    let c = |n: u8| Style::new().for_stderr().color256(n);
    ColorfulTheme {
        defaults_style: c(DIM),
        prompt_style: Style::new().for_stderr().bold(),
        prompt_prefix: style("  ›".to_string())
            .for_stderr()
            .fg(ACCENT)
            .bold(),
        prompt_suffix: style("".to_string()).for_stderr(),
        success_prefix: style("  ✓".to_string()).for_stderr().color256(OK),
        success_suffix: style("·".to_string()).for_stderr().color256(DIM),
        error_prefix: style("  ✗".to_string()).for_stderr().color256(DANGER),
        error_style: c(DANGER),
        hint_style: c(DIM),
        values_style: Style::new().for_stderr().fg(YOU),
        active_item_style: Style::new().for_stderr().fg(YOU).bold(),
        inactive_item_style: Style::new().for_stderr(),
        active_item_prefix: style("    ▸".to_string()).for_stderr().fg(ACCENT),
        inactive_item_prefix: style("     ".to_string()).for_stderr(),
        checked_item_prefix: style("    ◼".to_string()).for_stderr().fg(ACCENT),
        unchecked_item_prefix: style("    ◻".to_string()).for_stderr().color256(DIM),
        picked_item_prefix: style("    ▸".to_string()).for_stderr().fg(ACCENT),
        unpicked_item_prefix: style("     ".to_string()).for_stderr(),
    }
}

fn dim(s: &str) -> String {
    style(s).color256(DIM).to_string()
}

fn heading(s: &str) {
    eprintln!("\n  {}", style(s).bold());
}

/// `axel` in the spectrum, one colour per letter.
fn wordmark() -> String {
    "axel"
        .chars()
        .zip(SPECTRUM)
        .map(|(ch, n)| style(ch).color256(n).bold().to_string())
        .collect()
}

/// The program behind the ACP adapter that lets Axel drive a harness (its
/// thread view, policies).  Claude's and Codex's are built in.
fn adapter(h: &Harness) -> Option<String> {
    axel_acp::adapter_command(h).first().cloned()
}

/// Whether `axel` should walk through setup before opening: no agent yet.
pub fn needed(ctx: &Ctx) -> bool {
    ctx.store
        .list_agents()
        .map(|a| a.is_empty())
        .unwrap_or(false)
}

/// Whether we can ask questions here.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// What to do once setup is through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Open the launcher.
    Open,
    /// Back to the shell.
    Quit,
}

/// Names offered as the default, the first one not taken.
const NAMES: [&str; 6] = ["atlas", "nova", "vega", "lyra", "orion", "juno"];

/// How much setup asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupMode {
    /// One question, the agent's name; everything else takes the default.
    Quick,
    /// Every choice: harness, skills, colour, mode, and more agents.
    Custom,
    /// No questions at all, for scripts: the defaults, and the first free name.
    Unattended,
}

/// What this machine has, as setup needs it.
struct Machine {
    rex: bool,
    git: bool,
    /// Installed harnesses, the default first (`Harness::BUILTIN` order).
    harnesses: Vec<Harness>,
}

pub fn run(mode: SetupMode) -> Result<Next> {
    if mode != SetupMode::Unattended && !interactive() {
        bail!("{}", t!("axel setup asks a question; run it in a terminal, or use `axel setup --yes` to take the defaults"));
    }
    let ctx = Ctx::open()?;
    if mode != SetupMode::Unattended {
        // Clearing is cosmetic: on a terminal that refuses it, setup still reads fine.
        let _ = Term::stderr().clear_screen();
    }
    eprintln!();
    eprintln!("  {}  {}", wordmark(), dim(t!("agent orchestration, designed for Rex")));
    let machine = survey();
    if machine.harnesses.is_empty() {
        explain_no_harness();
        return Ok(Next::Quit);
    }
    match mode {
        SetupMode::Custom => run_custom(&ctx, &machine),
        SetupMode::Quick | SetupMode::Unattended => run_quick(&ctx, &machine, mode),
    }
}

/// Look at the machine without printing anything.
fn survey() -> Machine {
    let harnesses: Vec<Harness> = Harness::BUILTIN.iter().filter(|h| which(h.program()).is_some()).cloned().collect();
    Machine { rex: axel_rex::detect().is_some(), git: which("git").is_some(), harnesses }
}

/// The default path: say what matters about the machine in a few lines, ask
/// for a name, and make the agent with the defaults.
fn run_quick(ctx: &Ctx, machine: &Machine, mode: SetupMode) -> Result<Next> {
    let harness = machine.harnesses[0].clone();
    report_quick(machine, &harness);
    let existing = ctx.store.list_agents()?;
    if !existing.is_empty() {
        let names: Vec<&str> = existing.iter().map(|a| a.name.as_str()).take(EXISTING_NAMES_SHOWN_MAX).collect();
        eprintln!();
        eprintln!("  {}", dim(&t!("You have {names}. Here's another.", names = names.join(", "))));
    }
    eprintln!();
    let name = match mode {
        SetupMode::Unattended => suggested_name(ctx),
        _ => ask_name(ctx, &theme(), t!("Name your agent"))?,
    };
    let color = default_color(existing.len());
    let agent = super::agent::create(ctx, &name, harness.clone(), None, vec!["*".into()], None, Some(color), RuntimeMode::Autopilot)
        .context(t!("creating the agent"))?;
    if harness == Harness::Claude && !claude_bypass_accepted() {
        match mode {
            SetupMode::Unattended => eprintln!("  {}", dim(t!("Claude Code will ask once before it runs on autopilot."))),
            _ => offer_claude_bypass(&theme(), true)?,
        }
    }
    print_ready(&agent.name, &harness, color);
    Ok(if mode == SetupMode::Unattended { Next::Quit } else { Next::Open })
}

/// How many existing agents setup names before it stops listing.
const EXISTING_NAMES_SHOWN_MAX: usize = 6;

/// The quick path's view of the machine: the harness the agent will run on
/// and whether it is logged in, then only what needs attention.
fn report_quick(machine: &Machine, harness: &Harness) {
    eprintln!();
    match login_state(harness) {
        Login::Ready => check(true, harness.label(), dim(t!("ready"))),
        Login::Needed => warn(harness.label(), t!("log in once first: {how}", how = harness.login())),
        Login::Unknown => check(true, harness.label(), dim(&t!("log in once if you haven't: {how}", how = harness.login()))),
    }
    let others: Vec<&str> = machine.harnesses[1..].iter().map(|h| h.label()).collect();
    if !others.is_empty() {
        eprintln!("    {}", dim(&t!("also here: {names}", names = others.join(", "))));
    }
    if !machine.git {
        warn("git", t!("not found: agents need it to work in their own branches").into());
    }
    if machine.rex {
        // Agents' questions reach every Rex app through a module in Rex's config.
        if let Err(e) = super::ui::ensure_rex_module() {
            warn("Rex", t!("agents' questions will show in a pane: {error}", error = format!("{e:#}")));
        }
    } else {
        eprintln!("    {}", dim(t!("Not in Rex: Axel runs inline here (superlogical.com/rex).")));
    }
}

/// The end of the quick path: the agent, and the one thing to try.
fn print_ready(name: &str, harness: &Harness, color: AgentColor) {
    eprintln!();
    eprintln!(
        "  {} {} {}",
        style("✓").color256(OK),
        style(t!("{name} is ready", name = name)).color256(color_index(color)).bold(),
        dim(&format!("· {} · {}", harness.label(), t!("autopilot")))
    );
    eprintln!();
    eprintln!("  {}", dim(t!("Give it something to do:")));
    eprintln!("    {} {}", style("›").fg(ACCENT).bold(), style(t!("@{name} add a health check endpoint", name = name)).fg(YOU));
    eprintln!();
    eprintln!("  {}", dim(&t!("Change it: axel agent edit {name}  ·  more agents: axel setup [--custom]", name = name)));
    eprintln!();
}

/// The custom path: the machine in full, then every choice for one agent or more.
fn run_custom(ctx: &Ctx, machine: &Machine) -> Result<Next> {
    let theme = theme();
    let rex = check_machine(machine);
    let mut first = true;
    loop {
        heading(if first && ctx.store.list_agents()?.is_empty() { t!("Your first agent") } else { t!("A new agent") });
        eprintln!("  {}", dim(t!("An agent is a name you can @mention and how to run it.")));
        eprintln!();
        let agent_name = create_agent(ctx, &theme, &machine.harnesses)?;
        first = false;
        eprintln!();
        let choices = [t!("Open Axel and give @{name} something to do", name = agent_name), t!("Create another agent").to_string()];
        let pick = Select::with_theme(&theme).with_prompt(t!("What next?")).items(&choices).default(0).interact()?;
        if pick == 0 {
            print_first_steps(&agent_name, rex);
            return Ok(Next::Open);
        }
        eprintln!("  {}", dim(t!("A builder and a reviewer on another model make a good pair.")));
    }
}

/// Whether a harness's CLI is logged in, where Axel can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Login {
    Ready,
    Needed,
    /// Axel doesn't know where this harness keeps its login.
    Unknown,
}

/// Best effort, from the files and variables each CLI reads its login from.
fn login_state(harness: &Harness) -> Login {
    let has_env = |names: &[&str]| names.iter().any(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()));
    match harness {
        Harness::Claude => {
            if has_env(&["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]) {
                return Login::Ready;
            }
            let state = claude_state_path().and_then(|p| std::fs::read_to_string(p).ok());
            claude_login(state.as_deref())
        }
        Harness::Codex => {
            if has_env(&["OPENAI_API_KEY"]) {
                return Login::Ready;
            }
            let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
            if home.is_some_and(|h| h.join("auth.json").is_file()) { Login::Ready } else { Login::Needed }
        }
        _ => Login::Unknown,
    }
}

/// Claude Code's login, from its state file (`~/.claude.json`): an
/// `oauthAccount` there means it is logged in.
fn claude_login(state_json: Option<&str>) -> Login {
    let logged_in = state_json
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .is_some_and(|v| v.get("oauthAccount").is_some_and(|a| !a.is_null()));
    if logged_in { Login::Ready } else { Login::Needed }
}

/// Claude Code's state file: `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
fn claude_state_path() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
        None => dirs::home_dir().map(|h| h.join(".claude.json")),
    }
}

/// The first of [`NAMES`] no agent has, else `agent-N`.
fn suggested_name(ctx: &Ctx) -> String {
    let taken = |n: &str| ctx.store.agent_by_name(n).ok().flatten().is_some();
    if let Some(n) = NAMES.iter().find(|n| !taken(n)) {
        return n.to_string();
    }
    (2..=NAME_SUFFIX_MAX).map(|i| format!("agent-{i}")).find(|n| !taken(n)).unwrap_or_else(|| format!("agent-{}", axel_store::new_id()))
}

/// How far `agent-N` names are tried before a unique id is used instead.
const NAME_SUFFIX_MAX: u32 = 1000;

/// The palette walked from orange, one step per agent already made.
fn default_color(agents_already: usize) -> AgentColor {
    let colors = AgentColor::ALL;
    let orange = colors.iter().position(|c| *c == AgentColor::Orange).unwrap_or(0);
    colors[(orange + agents_already) % colors.len()]
}

fn warn(what: &str, detail: String) {
    eprintln!("  {} {} {detail}", style("!").color256(WARN).bold(), style(super::text::pad(what, 16)).bold());
}

/// The custom path's report: Rex, git and every harness found, with how
/// Axel would run it.  True when we run inside a Rex session.
fn check_machine(machine: &Machine) -> bool {
    heading(t!("This machine"));
    let rex_detail = if machine.rex { t!("you're in a Rex session").to_string() } else { dim(t!("not here: Axel runs inline (superlogical.com/rex)")) };
    check(machine.rex, "Rex", rex_detail);
    if machine.rex {
        // Agents' questions reach every Rex app through a module in Rex's config.
        match super::ui::ensure_rex_module() {
            Ok(()) => check(true, t!("Rex questions"), t!("on every Rex app, Mac and iPhone").into()),
            Err(e) => check(false, t!("Rex questions"), dim(&t!("in a pane: {error}", error = format!("{e:#}")))),
        }
    }
    check(machine.git, "git", which("git").map(|p| tilde(&p)).unwrap_or_else(|| t!("not found").into()));
    for h in &machine.harnesses {
        let Some(found) = which(h.program()) else { continue };
        let adapter = adapter(h);
        let managed = adapter.as_deref().and_then(which).is_some();
        let how = match &adapter {
            _ if managed => format!("· {}", t!("managed")),
            // Its adapter is a separate install (Pi's, Amp's): name it.
            Some(a) if a != h.program() => format!("· {}", t!("in a pane until you install its ACP adapter, {adapter}", adapter = a)),
            _ => format!("· {}", t!("in a pane (no ACP adapter)")),
        };
        check(true, h.label(), format!("{}  {}", tilde(&found), dim(&how)));
    }
    machine.rex
}

/// How many harnesses the no-harness message gives install commands for;
/// the rest are named on one line.  Two cover most people.
const INSTALLS_SHOWN: usize = 2;

/// No harness on this machine: how to get one, the most common first.
fn explain_no_harness() {
    eprintln!();
    eprintln!("  {}", style(t!("Axel runs your agents' own CLIs, and none is installed.")).color256(DANGER));
    eprintln!("  {}", dim(t!("Install one, log in once, then run `axel` again:")));
    eprintln!();
    for h in Harness::BUILTIN.iter().take(INSTALLS_SHOWN) {
        eprintln!("    {:<13} {}", style(h.label()).bold(), dim(h.install()));
    }
    let rest: Vec<&str> = Harness::BUILTIN.iter().skip(INSTALLS_SHOWN).map(|h| h.label()).collect();
    eprintln!();
    eprintln!("  {}", dim(&t!("Also supported: {names}.", names = rest.join(", "))));
}

/// Ask for one agent's name, harness, skills, colour and mode, create it, and
/// return its name.
fn create_agent(ctx: &Ctx, theme: &ColorfulTheme, installed: &[Harness]) -> Result<String> {
    assert!(!installed.is_empty(), "setup stops before asking when no harness is installed");
    let name = ask_name(ctx, theme, t!("Name"))?;
    let harness = ask_harness(theme, installed)?;
    // Axel drives the harness's own CLI with its own login: without one,
    // the agent can't start (Pi, say, refuses every session).
    match login_state(&harness) {
        Login::Ready => {}
        Login::Needed => warn(harness.label(), t!("log in once first: {how}", how = harness.login())),
        Login::Unknown => eprintln!("  {}", dim(&t!("{harness} has to be logged in once: {how}.", harness = harness.label(), how = harness.login()))),
    }

    let skills = pick_skills(theme)?;
    let color = ask_color(ctx, theme)?;
    let mode = ask_mode(theme)?;

    let agent = super::agent::create(
        ctx,
        &name,
        harness.clone(),
        None,
        skills,
        None,
        Some(color),
        mode,
    )
    .context(t!("creating the agent"))?;
    // Claude Code asks once per machine to accept bypass mode, which autopilot uses.
    if harness == Harness::Claude && mode == RuntimeMode::Autopilot && !claude_bypass_accepted() {
        offer_claude_bypass(theme, false)?;
    }
    eprintln!();
    eprintln!(
        "  {} {}",
        style("✓").color256(OK),
        style(t!("{name} is ready", name = agent.name)).color256(color_index(color)).bold()
    );
    Ok(agent.name)
}

fn ask_name(ctx: &Ctx, theme: &ColorfulTheme, prompt: &str) -> Result<String> {
    let name: String = Input::with_theme(theme)
        .with_prompt(prompt)
        .default(suggested_name(ctx))
        .validate_with(|s: &String| -> Result<(), String> {
            let s = s.trim();
            if s.is_empty() || !s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                Err(t!("letters, digits, - and _ only, so it works as @name").into())
            } else if ctx.store.agent_by_name(s).ok().flatten().is_some() {
                Err(t!("there's already an agent named {name}", name = s))
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    Ok(name.trim().to_string())
}

fn ask_harness(theme: &ColorfulTheme, installed: &[Harness]) -> Result<Harness> {
    if let [only] = installed {
        eprintln!(
            "  {} {} {} {}",
            style("✓").color256(OK),
            style(t!("Harness")).bold(),
            dim("·"),
            style(only.label()).fg(YOU)
        );
        return Ok(only.clone());
    }
    let labels: Vec<String> = installed.iter().map(|h| h.label().to_string()).collect();
    let index = Select::with_theme(theme)
        .with_prompt(t!("Harness"))
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(installed[index].clone())
}

/// The agent's colour; the default walks the palette from orange, one step
/// per agent already made.
fn ask_color(ctx: &Ctx, theme: &ColorfulTheme) -> Result<AgentColor> {
    let colors = AgentColor::ALL;
    let color_items: Vec<String> = colors
        .iter()
        .map(|c| format!("{} {c}", style("●").color256(color_index(*c))))
        .collect();
    let used = ctx.store.list_agents().map(|a| a.len()).unwrap_or(0);
    let default_index = colors.iter().position(|c| *c == default_color(used)).unwrap_or(0);
    let index = Select::with_theme(theme)
        .with_prompt(t!("Colour"))
        .items(&color_items)
        .default(default_index)
        .interact()?;
    Ok(colors[index])
}

fn ask_mode(theme: &ColorfulTheme) -> Result<RuntimeMode> {
    let modes = [
        (
            RuntimeMode::Autopilot,
            "autopilot",
            t!("never asks; the guard still refuses irreversible commands"),
        ),
        (
            RuntimeMode::Edits,
            "edits",
            t!("file edits go through, anything else asks you"),
        ),
        (
            RuntimeMode::Supervised,
            "supervised",
            t!("asks before every action"),
        ),
        (RuntimeMode::Plan, "plan", t!("read-only: looks and proposes")),
    ];
    let mode_items: Vec<String> = modes
        .iter()
        .map(|(_, n, d)| format!("{n:<11} {d}"))
        .collect();
    let index = Select::with_theme(theme)
        .with_prompt(t!("Mode"))
        .items(&mode_items)
        .default(0)
        .interact()?;
    Ok(modes[index].0)
}

fn offer_claude_bypass(theme: &ColorfulTheme, quiet: bool) -> Result<()> {
    eprintln!();
    let yes = Confirm::with_theme(theme)
        .with_prompt(t!("Claude Code asks once before it runs on autopilot. Answer yes for it now?"))
        .default(true)
        .interact()?;
    if yes {
        match accept_claude_bypass() {
            // The answer on screen already says it; only the custom path names the file.
            Ok(_) if quiet => {}
            Ok(path) => check(
                true,
                "Claude Code",
                dim(&t!("autopilot accepted in {path}", path = tilde(&path))),
            ),
            Err(e) => check(
                false,
                "Claude Code",
                t!("couldn't update its settings: {error}", error = format!("{e:#}")),
            ),
        }
    }
    Ok(())
}

fn print_first_steps(agent_name: &str, rex: bool) {
    eprintln!();
    eprintln!(
        "  {}",
        dim(t!("Type what you want done in the prompt and press Enter, e.g."))
    );
    eprintln!(
        "    {} {}",
        style("›").fg(ACCENT).bold(),
        style(t!("@{name} add a health check endpoint", name = agent_name)).fg(YOU)
    );
    if !rex {
        eprintln!(
            "  {}",
            dim(t!("Inside Rex, `axel` floats over your window; here it runs inline."))
        );
    }
    eprintln!();
}

/// A path with the home directory shown as `~`.
fn tilde(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn check(ok: bool, what: &str, detail: String) {
    let mark = if ok {
        style("✓").color256(OK)
    } else {
        style("✗").color256(DANGER)
    };
    // Padded by terminal columns before styling: the escapes take none.
    eprintln!("  {mark} {} {detail}", style(super::text::pad(what, 16)).bold());
}

/// The TUI's colour for an agent colour, as a 256-colour index.
fn color_index(c: AgentColor) -> u8 {
    match c {
        AgentColor::Purple => 5,
        AgentColor::Yellow => 3,
        AgentColor::Red => 1,
        AgentColor::Green => 2,
        AgentColor::Blue => 4,
        AgentColor::Gray => 7,
        AgentColor::Orange => 208,
    }
}

/// Skills from `~/.config/axel/skills` to give the agent.  A few chosen ones beat
/// all of them: each skill's description sits in every session's context.
fn pick_skills(theme: &ColorfulTheme) -> Result<Vec<String>> {
    let global = crate::paths::global_skills_dir();
    // An unreadable skills directory is the same as an empty one here: the
    // agent gets `*`, and launches report the directory's error themselves.
    let found = axel_config::find_all_skills(&[], Some(global.as_path())).unwrap_or_default();
    if found.is_empty() {
        eprintln!(
            "  {} {} {} {}",
            style("✓").color256(OK),
            style(t!("Skills")).bold(),
            dim("·"),
            dim(t!("none yet: all of them, once you add some"))
        );
        eprintln!("    {}", dim(&t!("skills live in {path}", path = tilde(&global))));
        return Ok(vec!["*".into()]);
    }
    let items: Vec<String> = found
        .values()
        .map(|s| match &s.description {
            Some(d) => format!(
                "{} {}",
                super::text::pad(&s.name, 20),
                d.chars().take(48).collect::<String>()
            ),
            None => s.name.clone(),
        })
        .collect();
    let picked = MultiSelect::with_theme(theme)
        .with_prompt(t!("Skills (Space picks, Enter when done)"))
        .items(&items)
        .interact()?;
    let names: Vec<String> = found
        .keys()
        .enumerate()
        .filter(|(i, _)| picked.contains(i))
        .map(|(_, k)| k.clone())
        .collect();
    Ok(if names.is_empty() {
        vec!["*".into()]
    } else {
        names
    })
}

fn claude_settings_path() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    Some(dir.join("settings.json"))
}

fn claude_bypass_accepted() -> bool {
    let Some(p) = claude_settings_path() else {
        return false;
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("skipDangerousModePermissionPrompt")
                .and_then(|b| b.as_bool())
        })
        .unwrap_or(false)
}

fn accept_claude_bypass() -> Result<PathBuf> {
    let path = claude_settings_path().context(t!("no home directory"))?;
    let mut v: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).context(t!("reading Claude Code's settings.json"))?
        }
        _ => serde_json::json!({}),
    };
    let obj = v
        .as_object_mut()
        .context(t!("Claude Code's settings.json isn't an object"))?;
    obj.insert(
        "skipDangerousModePermissionPrompt".into(),
        serde_json::Value::Bool(true),
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_is_logged_in_when_its_state_file_has_an_account() {
        assert_eq!(claude_login(Some(r#"{"oauthAccount": {"emailAddress": "a@b.c"}}"#)), Login::Ready);
        assert_eq!(claude_login(Some(r#"{"oauthAccount": null}"#)), Login::Needed);
        assert_eq!(claude_login(Some(r#"{"numStartups": 3}"#)), Login::Needed, "it ran, but nobody logged in");
        assert_eq!(claude_login(Some("not json")), Login::Needed);
        assert_eq!(claude_login(None), Login::Needed, "it never ran here");
    }

    #[test]
    fn the_default_colour_walks_the_palette_from_orange() {
        assert_eq!(default_color(0), AgentColor::Orange);
        let all: std::collections::HashSet<AgentColor> = (0..AgentColor::ALL.len()).map(default_color).collect();
        assert_eq!(all.len(), AgentColor::ALL.len(), "every colour before any repeats");
        assert_eq!(default_color(AgentColor::ALL.len()), AgentColor::Orange);
    }

    #[test]
    fn home_is_shown_as_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(tilde(&home.join("Library/x")), "~/Library/x");
        assert_eq!(tilde(std::path::Path::new("/usr/bin/git")), "/usr/bin/git");
    }

    #[test]
    fn accepting_claude_bypass_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"model": "opus", "env": {"A": "1"}}"#,
        )
        .unwrap();
        // SAFETY: tests in this module don't read CLAUDE_CONFIG_DIR concurrently.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir.path()) };
        assert!(!claude_bypass_accepted());
        accept_claude_bypass().unwrap();
        assert!(claude_bypass_accepted());
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["env"]["A"], "1");
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }
}
