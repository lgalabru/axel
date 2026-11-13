//! Permission policy for managed sessions.
//!
//! The agent asks (`session/request_permission`) before running a tool; the
//! host answers by **policy** instead of forwarding every question to a
//! human.  [`decide`] maps the session's [`RuntimeMode`] and the tool call to
//! a [`Decision`]; [`shell_deny_reason`] is the short list of irreversible
//! shell commands that even `autopilot` refuses (the same list the
//! `PreToolUse` guard hook uses for pane sessions).

use axel_store::RuntimeMode;
use serde_json::Value;

/// The ACP tool kinds (`ToolCall.kind`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

impl ToolKind {
    /// Every kind, in protocol order.
    pub const ALL: [ToolKind; 10] = [
        ToolKind::Read,
        ToolKind::Edit,
        ToolKind::Delete,
        ToolKind::Move,
        ToolKind::Search,
        ToolKind::Execute,
        ToolKind::Think,
        ToolKind::Fetch,
        ToolKind::SwitchMode,
        ToolKind::Other,
    ];

    /// The protocol's snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolKind::Read => "read",
            ToolKind::Edit => "edit",
            ToolKind::Delete => "delete",
            ToolKind::Move => "move",
            ToolKind::Search => "search",
            ToolKind::Execute => "execute",
            ToolKind::Think => "think",
            ToolKind::Fetch => "fetch",
            ToolKind::SwitchMode => "switch_mode",
            ToolKind::Other => "other",
        }
    }

    /// Parse the protocol name; anything unknown is [`ToolKind::Other`].
    pub fn parse(s: &str) -> ToolKind {
        match s {
            "read" => ToolKind::Read,
            "edit" => ToolKind::Edit,
            "delete" => ToolKind::Delete,
            "move" => ToolKind::Move,
            "search" => ToolKind::Search,
            "execute" => ToolKind::Execute,
            "think" => ToolKind::Think,
            "fetch" => ToolKind::Fetch,
            "switch_mode" => ToolKind::SwitchMode,
            _ => ToolKind::Other,
        }
    }

    /// Kinds that never change anything on disk or in the world.
    fn is_read_only(self) -> bool {
        matches!(self, ToolKind::Read | ToolKind::Search | ToolKind::Think)
    }
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// What the host should answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Pick the first `allow_*` option (preferring `allow_once`).
    Allow,
    /// Pick the first `reject_*` option, with a reason for the log.
    Deny { reason: String },
    /// Park a `Permission` item and wait for a human answer.
    Ask,
}

/// Decide a permission request.
///
/// | mode | read / search / think | fetch | edit / move | execute / delete / other / switch_mode |
/// |---|---|---|---|---|
/// | autopilot | allow | allow | allow | allow, unless the guard denies |
/// | edits | allow | allow | allow | ask |
/// | supervised | allow | ask | ask | ask |
/// | plan | allow | allow | deny | deny |
///
/// In every mode a tool named in `disallowed_tools` (matched against `title`
/// and the raw input's `tool`/`name`) is denied, and a shell command matching
/// [`shell_deny_reason`] is denied.
pub fn decide(
    mode: RuntimeMode,
    tool_kind: ToolKind,
    title: &str,
    raw_input: &Value,
    disallowed_tools: &[String],
) -> Decision {
    if let Some(tool) = disallowed_match(title, raw_input, disallowed_tools) {
        return Decision::Deny {
            reason: format!("tool {tool} is disallowed for this agent"),
        };
    }
    if let Some(cmd) = shell_command(raw_input)
        && let Some(reason) = shell_deny_reason(cmd)
    {
        return Decision::Deny { reason };
    }
    match mode {
        RuntimeMode::Autopilot => Decision::Allow,
        RuntimeMode::Edits => match tool_kind {
            ToolKind::Read
            | ToolKind::Search
            | ToolKind::Think
            | ToolKind::Fetch
            | ToolKind::Edit
            | ToolKind::Move => Decision::Allow,
            ToolKind::Execute | ToolKind::Delete | ToolKind::SwitchMode | ToolKind::Other => {
                Decision::Ask
            }
        },
        RuntimeMode::Supervised => {
            if tool_kind.is_read_only() {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        RuntimeMode::Plan => match tool_kind {
            ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch => {
                Decision::Allow
            }
            _ => Decision::Deny {
                reason: format!("{tool_kind} is not allowed in plan mode"),
            },
        },
    }
}

/// The shell command inside a tool call's raw input, if there is one
/// (`command` as a string, or as an argv array).
fn shell_command(raw_input: &Value) -> Option<&str> {
    raw_input.get("command").and_then(Value::as_str)
}

fn disallowed_match<'a>(
    title: &str,
    raw_input: &Value,
    disallowed_tools: &'a [String],
) -> Option<&'a str> {
    if disallowed_tools.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = vec![title];
    for key in ["tool", "tool_name", "name"] {
        if let Some(n) = raw_input.get(key).and_then(Value::as_str) {
            names.push(n);
        }
    }
    // Titles are often "Bash: cargo test" or "Read file.rs"; match the first word too.
    let head = title
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .unwrap_or("");
    names.push(head);
    disallowed_tools
        .iter()
        .find(|d| names.iter().any(|n| n.eq_ignore_ascii_case(d)))
        .map(String::as_str)
}

/// Why a shell command must never run unattended, or `None` if it is fine.
///
/// Whitespace is normalised before matching.  The list is deliberately short:
/// force pushes, hard resets, `git clean`, discarding the working tree,
/// `rm -rf` on roots, `sudo`, raw disk writes, fork bombs, and pushes to
/// `main`/`master`.
pub fn shell_deny_reason(cmd: &str) -> Option<String> {
    let c = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    let rules: &[(&str, &str)] = &[
        ("git push --force", "force push"),
        ("git push -f", "force push"),
        ("git push --force-with-lease", "force push"),
        ("git reset --hard", "hard reset"),
        ("git clean -f", "git clean"),
        ("git checkout -- .", "discarding all changes"),
        ("git restore .", "discarding all changes"),
        ("git branch -D", "branch force-delete"),
        ("rm -rf /", "rm -rf on an absolute path"),
        ("rm -rf ~", "rm -rf on home"),
        ("rm -rf *", "rm -rf wildcard"),
        ("rm -rf .", "rm -rf current dir"),
        ("rm -fr /", "rm -rf on an absolute path"),
        ("sudo ", "sudo"),
        ("mkfs", "mkfs"),
        ("dd if=", "dd"),
        (":(){", "fork bomb"),
        ("> /dev/sd", "raw disk write"),
    ];
    for (needle, why) in rules {
        if c.contains(needle) {
            return Some((*why).to_string());
        }
    }
    // Pushing to main/master directly.
    if c.starts_with("git push")
        && (c.contains(" main") || c.contains(" master"))
        && c.contains("origin")
    {
        return Some("push to a protected branch".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denies_irreversible() {
        assert!(shell_deny_reason("git push --force origin feature").is_some());
        assert!(shell_deny_reason("git  reset   --hard HEAD~1").is_some());
        assert!(shell_deny_reason("rm -rf /tmp/x && rm -rf /").is_some());
        assert!(shell_deny_reason("git push origin main").is_some());
    }

    #[test]
    fn allows_ordinary() {
        assert!(shell_deny_reason("cargo test --offline").is_none());
        assert!(shell_deny_reason("git push origin axel/feature").is_none());
        assert!(shell_deny_reason("rm -rf target").is_none());
        assert!(shell_deny_reason("git status").is_none());
    }

    fn d(mode: RuntimeMode, kind: ToolKind) -> Decision {
        decide(mode, kind, "Tool", &json!({}), &[])
    }

    #[test]
    fn autopilot_allows_everything_the_guard_does_not_deny() {
        for kind in ToolKind::ALL {
            assert_eq!(d(RuntimeMode::Autopilot, kind), Decision::Allow, "{kind}");
        }
        let denied = decide(
            RuntimeMode::Autopilot,
            ToolKind::Execute,
            "Bash",
            &json!({"command": "git push --force origin main"}),
            &[],
        );
        assert_eq!(
            denied,
            Decision::Deny {
                reason: "force push".into()
            }
        );
    }

    #[test]
    fn edits_asks_for_execute_delete_other() {
        use ToolKind::*;
        for kind in [Read, Search, Think, Fetch, Edit, Move] {
            assert_eq!(d(RuntimeMode::Edits, kind), Decision::Allow, "{kind}");
        }
        for kind in [Execute, Delete, Other, SwitchMode] {
            assert_eq!(d(RuntimeMode::Edits, kind), Decision::Ask, "{kind}");
        }
    }

    #[test]
    fn supervised_asks_for_everything_but_reads() {
        use ToolKind::*;
        for kind in [Read, Search, Think] {
            assert_eq!(d(RuntimeMode::Supervised, kind), Decision::Allow, "{kind}");
        }
        for kind in [Fetch, Edit, Move, Execute, Delete, Other, SwitchMode] {
            assert_eq!(d(RuntimeMode::Supervised, kind), Decision::Ask, "{kind}");
        }
    }

    #[test]
    fn plan_denies_writes() {
        use ToolKind::*;
        for kind in [Read, Search, Think, Fetch] {
            assert_eq!(d(RuntimeMode::Plan, kind), Decision::Allow, "{kind}");
        }
        for kind in [Edit, Move, Execute, Delete, Other, SwitchMode] {
            assert!(
                matches!(d(RuntimeMode::Plan, kind), Decision::Deny { .. }),
                "{kind}"
            );
        }
    }

    #[test]
    fn disallowed_tools_deny_in_every_mode() {
        let disallowed = vec!["WebFetch".to_string()];
        for &mode in RuntimeMode::ALL {
            let got = decide(
                mode,
                ToolKind::Fetch,
                "WebFetch: https://x",
                &json!({}),
                &disallowed,
            );
            assert!(matches!(got, Decision::Deny { .. }), "{mode:?}");
            let by_input = decide(
                mode,
                ToolKind::Other,
                "fetch",
                &json!({"tool": "webfetch"}),
                &disallowed,
            );
            assert!(matches!(by_input, Decision::Deny { .. }), "{mode:?}");
        }
        assert_eq!(
            decide(
                RuntimeMode::Autopilot,
                ToolKind::Fetch,
                "Read",
                &json!({}),
                &disallowed
            ),
            Decision::Allow
        );
    }

    #[test]
    fn shell_guard_applies_before_asking() {
        let got = decide(
            RuntimeMode::Supervised,
            ToolKind::Execute,
            "Bash",
            &json!({"command": "sudo rm -rf /"}),
            &[],
        );
        assert!(matches!(got, Decision::Deny { .. }));
    }

    #[test]
    fn tool_kind_round_trips() {
        for kind in ToolKind::ALL {
            assert_eq!(ToolKind::parse(kind.as_str()), kind);
        }
        assert_eq!(ToolKind::parse("banana"), ToolKind::Other);
    }
}
