//! `axel setup`: the first-run walkthrough.  `axel` runs it by itself the first
//! time, when there is no agent yet.  It checks this machine (Rex, the harness
//! CLIs, their ACP adapters), offers to install what's missing, and creates the
//! first agent.  Dialoguer, themed like the TUI: warm accent, orange values,
//! dim hints.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use axel_store::{AgentColor, Harness, RuntimeMode};
use console::{Style, Term, style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use super::Ctx;
use super::launch::which;

// The TUI's palette (`axel-tui/src/theme.rs`), as 256-colour indices.
const ACCENT: u8 = 179;
const YOU: u8 = 208;
const OK: u8 = 108;
const DANGER: u8 = 167;
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
            .color256(ACCENT)
            .bold(),
        prompt_suffix: style("".to_string()).for_stderr(),
        success_prefix: style("  ✓".to_string()).for_stderr().color256(OK),
        success_suffix: style("·".to_string()).for_stderr().color256(DIM),
        error_prefix: style("  ✗".to_string()).for_stderr().color256(DANGER),
        error_style: c(DANGER),
        hint_style: c(DIM),
        values_style: c(YOU),
        active_item_style: c(YOU).bold(),
        inactive_item_style: Style::new().for_stderr(),
        active_item_prefix: style("    ▸".to_string()).for_stderr().color256(ACCENT),
        inactive_item_prefix: style("     ".to_string()).for_stderr(),
        checked_item_prefix: style("    ◼".to_string()).for_stderr().color256(ACCENT),
        unchecked_item_prefix: style("    ◻".to_string()).for_stderr().color256(DIM),
        picked_item_prefix: style("    ▸".to_string()).for_stderr().color256(ACCENT),
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

/// A harness Axel can run, and what it needs.
struct HarnessInfo {
    harness: Harness,
    label: &'static str,
    program: &'static str,
    install: &'static str,
}

fn harnesses() -> Vec<HarnessInfo> {
    vec![
        HarnessInfo {
            harness: Harness::Claude,
            label: "Claude Code",
            program: "claude",
            install: "npm i -g @anthropic-ai/claude-code",
        },
        HarnessInfo {
            harness: Harness::Codex,
            label: "Codex",
            program: "codex",
            install: "npm i -g @openai/codex",
        },
        HarnessInfo {
            harness: Harness::Opencode,
            label: "OpenCode",
            program: "opencode",
            install: "npm i -g opencode-ai",
        },
    ]
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

pub fn run() -> Result<Next> {
    if !interactive() {
        bail!("axel setup asks questions; run it in a terminal");
    }
    let ctx = Ctx::open()?;
    let theme = theme();
    let term = Term::stderr();
    let _ = term.clear_screen();

    eprintln!();
    eprintln!(
        "  {}  {}",
        wordmark(),
        dim("agent orchestration, designed for Rex")
    );
    eprintln!("  {}", dim("Let's check this machine and create an agent."));

    // ---- this machine ---------------------------------------------------------
    heading("This machine");
    let rex = axel_rex::detect().is_some();
    check(
        rex,
        "Rex",
        if rex {
            "you're in a Rex session".into()
        } else {
            dim("not here: Axel runs inline (superlogical.com/rex)")
        },
    );
    check(
        which("git").is_some(),
        "git",
        which("git")
            .map(|p| tilde(&p))
            .unwrap_or_else(|| "not found".into()),
    );

    let mut installed: Vec<&HarnessInfo> = Vec::new();
    let all = harnesses();
    for h in &all {
        let found = which(h.program);
        let acp = adapter(&h.harness);
        let managed = acp.as_deref().and_then(which).is_some();
        let detail = match (&found, managed) {
            (Some(p), true) => format!("{}  {}", tilde(p), dim("· managed")),
            (Some(p), false) => format!("{}  {}", tilde(p), dim("· in a pane (no ACP adapter)")),
            (None, _) => dim(&format!("not found  ({})", h.install)),
        };
        check(found.is_some(), h.label, detail);
        if found.is_some() {
            installed.push(h);
        }
    }
    if installed.is_empty() {
        eprintln!();
        eprintln!(
            "  {}",
            style("Axel runs your agents' own CLIs, and none is installed.").color256(DANGER)
        );
        eprintln!(
            "  {}",
            dim("Install one of them, log in once, then run `axel` again:")
        );
        for h in &all {
            eprintln!("    {}  {}", style(h.label).bold(), dim(h.install));
        }
        return Ok(Next::Quit);
    }

    // ---- agents, one or more --------------------------------------------------
    let mut again = false;
    loop {
        let first = ctx
            .store
            .list_agents()
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if first {
            heading("Your first agent");
            eprintln!(
                "  {}",
                dim("An agent is a name you can @mention, its own key, and how to run it.")
            );
        } else if !std::mem::replace(&mut again, true) {
            heading("A new agent");
        }
        eprintln!();

        let taken = |n: &str| ctx.store.agent_by_name(n).ok().flatten().is_some();
        let suggestion = NAMES
            .iter()
            .find(|n| !taken(n))
            .map(|n| n.to_string())
            .unwrap_or_default();
        let name: String = Input::with_theme(&theme)
            .with_prompt("Name")
            .default(suggestion)
            .validate_with(|s: &String| -> Result<(), String> {
                let s = s.trim();
                if s.is_empty()
                    || !s
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    Err("letters, digits, - and _ only, so it works as @name".into())
                } else if ctx.store.agent_by_name(s).ok().flatten().is_some() {
                    Err(format!("there's already an agent named {s}"))
                } else {
                    Ok(())
                }
            })
            .interact_text()?;
        let name = name.trim().to_string();

        let harness = if installed.len() == 1 {
            let h = installed[0];
            eprintln!(
                "  {} {} {} {}",
                style("✓").color256(OK),
                style("Harness").bold(),
                dim("·"),
                style(h.label).color256(YOU)
            );
            h.harness.clone()
        } else {
            let labels: Vec<String> = installed.iter().map(|h| h.label.to_string()).collect();
            let i = Select::with_theme(&theme)
                .with_prompt("Harness")
                .items(&labels)
                .default(0)
                .interact()?;
            installed[i].harness.clone()
        };

        let skills = pick_skills(&ctx, &theme)?;

        let colors = AgentColor::ALL;
        let color_items: Vec<String> = colors
            .iter()
            .map(|c| format!("{} {c}", style("●").color256(color_index(*c))))
            .collect();
        let used = ctx.store.list_agents().map(|a| a.len()).unwrap_or(0);
        let default_color = colors
            .iter()
            .position(|c| *c == AgentColor::Orange)
            .unwrap_or(0);
        let ci = Select::with_theme(&theme)
            .with_prompt("Colour")
            .items(&color_items)
            .default((default_color + used) % colors.len())
            .interact()?;

        let modes = [
            (
                RuntimeMode::Autopilot,
                "autopilot",
                "never asks; the guard still refuses irreversible commands",
            ),
            (
                RuntimeMode::Edits,
                "edits",
                "file edits go through, anything else asks you",
            ),
            (
                RuntimeMode::Supervised,
                "supervised",
                "asks before every action",
            ),
            (RuntimeMode::Plan, "plan", "read-only: looks and proposes"),
        ];
        let mode_items: Vec<String> = modes
            .iter()
            .map(|(_, n, d)| format!("{n:<11} {d}"))
            .collect();
        let mi = Select::with_theme(&theme)
            .with_prompt("Mode")
            .items(&mode_items)
            .default(0)
            .interact()?;
        let mode = modes[mi].0;

        let agent = super::agent::create(
            &ctx,
            &name,
            harness.clone(),
            None,
            skills,
            None,
            Some(colors[ci]),
            mode,
        )
        .context("creating the agent")?;
        let key: String = agent.pubkey.chars().take(6).collect();
        eprintln!();
        eprintln!(
            "  {} {} {}",
            style("✓").color256(OK),
            style(&agent.name).color256(color_index(colors[ci])).bold(),
            dim(&format!("· key {key}… · attested by you"))
        );

        // Claude Code asks once per machine to accept bypass mode, which autopilot uses.
        if harness == Harness::Claude && mode == RuntimeMode::Autopilot && !claude_bypass_accepted()
        {
            eprintln!();
            let yes = Confirm::with_theme(&theme)
                .with_prompt("Skip Claude Code's one-time autopilot prompt?")
                .default(true)
                .interact()?;
            if yes {
                match accept_claude_bypass() {
                    Ok(path) => check(
                        true,
                        "Claude Code",
                        dim(&format!("autopilot accepted in {}", tilde(&path))),
                    ),
                    Err(e) => check(
                        false,
                        "Claude Code",
                        format!("couldn't update its settings: {e:#}"),
                    ),
                }
            }
        }

        // ---- next -----------------------------------------------------------------
        eprintln!();
        let choices = [
            format!("Open Axel and give @{} something to do", agent.name),
            "Create another agent".to_string(),
        ];
        let pick = Select::with_theme(&theme)
            .with_prompt("What next?")
            .items(&choices)
            .default(0)
            .interact()?;
        if pick == 1 {
            again = true;
            heading("Another agent");
            eprintln!(
                "  {}",
                dim("A builder and a reviewer on another model make a good pair.")
            );
            continue;
        }
        eprintln!();
        eprintln!(
            "  {}",
            dim("Type what you want done in the prompt and press Enter, e.g.")
        );
        eprintln!(
            "    {} {}",
            style("›").color256(ACCENT).bold(),
            style(format!("@{} add a health check endpoint", agent.name)).color256(YOU)
        );
        if !rex {
            eprintln!(
                "  {}",
                dim("Inside Rex, `axel` floats over your window; here it runs inline.")
            );
        }
        eprintln!();
        return Ok(Next::Open);
    }
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
    eprintln!("  {mark} {:<16} {detail}", style(what).bold());
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
fn pick_skills(_ctx: &Ctx, theme: &ColorfulTheme) -> Result<Vec<String>> {
    let global = crate::paths::global_skills_dir();
    let found = axel_config::find_all_skills(&[], Some(global.as_path())).unwrap_or_default();
    if found.is_empty() {
        eprintln!(
            "  {} {} {} {}",
            style("✓").color256(OK),
            style("Skills").bold(),
            dim("·"),
            dim("none yet: all of them, once you add some")
        );
        eprintln!("    {}", dim(&format!("skills live in {}", tilde(&global))));
        return Ok(vec!["*".into()]);
    }
    let items: Vec<String> = found
        .values()
        .map(|s| match &s.description {
            Some(d) => format!(
                "{:<20} {}",
                s.name,
                d.chars().take(48).collect::<String>()
            ),
            None => s.name.clone(),
        })
        .collect();
    let picked = MultiSelect::with_theme(theme)
        .with_prompt("Skills (Space picks, Enter when done)")
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
    let path = claude_settings_path().context("no home directory")?;
    let mut v: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).context("reading Claude Code's settings.json")?
        }
        _ => serde_json::json!({}),
    };
    let obj = v
        .as_object_mut()
        .context("Claude Code's settings.json isn't an object")?;
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
