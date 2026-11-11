//! An agent's reasoning effort where the binary applies it: what can be said
//! in advance about a harness that will not take a level, and pane launches.
//! Managed sessions get it through `HostConfig`.

use axel_store::{Effort, Harness};

/// What can be said before any session starts about a harness that will not
/// take `effort`. A managed session of another harness finds out from its
/// ACP `configOptions`, and says so itself.
pub fn warning(harness: &Harness, effort: Effort) -> Option<String> {
    let label = harness.label();
    if harness.has_builtin_adapter() {
        // Axel's own adapter starts the harness at the level, when the harness has it.
        if harness.native_effort(effort).is_some() {
            return None;
        }
        let top = Effort::ALL.iter().rev().find(|e| harness.native_effort(**e).is_some());
        let stops = top.map(|top| format!(" (it stops at {top})")).unwrap_or_default();
        return Some(format!("{label} has no `{effort}` reasoning effort{stops}; its sessions run at {label}'s default"));
    }
    harness.pane_effort_args(effort).is_none().then(|| {
        format!("{label} panes cannot be started at effort {effort}; a managed (ACP) session applies it when the harness offers that level, and says so when it doesn't")
    })
}

/// Start a pane's harness at `effort`: its own flag, ahead of the other
/// arguments. A harness with no such flag launches unchanged, with a warning.
pub fn pane(harness: &Harness, spec: &mut axel_config::launch::LaunchSpec, effort: Option<Effort>, mut warn: impl FnMut(&str)) {
    let Some(effort) = effort else { return };
    match harness.pane_effort_args(effort) {
        Some(mut args) => {
            args.append(&mut spec.args);
            spec.args = args;
        }
        None => warn(&warning(harness, effort).unwrap_or_else(|| format!("{} cannot take effort {effort}", harness.label()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axel_config::launch::LaunchSpec;

    fn launched(harness: Harness, args: &[&str], effort: Option<Effort>) -> (Vec<String>, Vec<String>) {
        let mut spec = LaunchSpec { program: harness.program().into(), args: args.iter().map(|a| a.to_string()).collect(), env: vec![] };
        let mut warnings = Vec::new();
        pane(&harness, &mut spec, effort, |w| warnings.push(w.to_string()));
        (spec.args, warnings)
    }

    #[test]
    fn a_pane_starts_its_harness_at_the_agents_effort() {
        let (args, warnings) = launched(Harness::Claude, &["--model", "opus", "fix it"], Some(Effort::Xhigh));
        assert_eq!(args, ["--effort", "xhigh", "--model", "opus", "fix it"]);
        assert!(warnings.is_empty());
        // Codex takes overrides ahead of its `resume` subcommand.
        let (args, _) = launched(Harness::Codex, &["resume", "0199-thread", "--model", "gpt-5.5"], Some(Effort::High));
        assert_eq!(args, ["-c", "model_reasoning_effort=high", "resume", "0199-thread", "--model", "gpt-5.5"]);
        assert_eq!(launched(Harness::Pi, &[], Some(Effort::Max)).0, ["--thinking", "max"]);
        assert_eq!(launched(Harness::Grok, &[], Some(Effort::Low)).0, ["--reasoning-effort", "low"]);
        let (args, warnings) = launched(Harness::Grok, &["go"], Some(Effort::Xhigh));
        assert_eq!(args, ["--reasoning-effort", "xhigh", "go"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn no_effort_changes_nothing_and_says_nothing() {
        for harness in [Harness::Claude, Harness::Codex, Harness::Goose, Harness::Custom("maki acp".into())] {
            let (args, warnings) = launched(harness, &["--model", "m", "go"], None);
            assert_eq!(args, ["--model", "m", "go"]);
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn a_pane_that_cannot_take_the_effort_launches_unchanged_with_a_warning() {
        let (args, warnings) = launched(Harness::Codex, &["go"], Some(Effort::Max));
        assert_eq!(args, ["go"]);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0], "Codex has no `max` reasoning effort (it stops at xhigh); its sessions run at Codex's default");
        for harness in [Harness::Opencode, Harness::Goose, Harness::Cursor, Harness::Kimi, Harness::Amp, Harness::Custom("maki acp".into())] {
            let (args, warnings) = launched(harness.clone(), &["go"], Some(Effort::High));
            assert_eq!(args, ["go"], "{harness}");
            assert!(warnings.len() == 1 && warnings[0].contains("panes cannot be started at effort high"), "{harness}: {warnings:?}");
        }
        let (args, warnings) = launched(Harness::Grok, &[], Some(Effort::Max));
        assert!(args.is_empty() && warnings.len() == 1);
    }
}
