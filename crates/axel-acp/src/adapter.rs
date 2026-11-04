//! Which program speaks ACP for a given harness.

use axel_store::Harness;

/// The command line that starts the ACP adapter for `harness`.
///
/// Overridable with `AXEL_ACP_<HARNESS>` (harness name upper-cased, non
/// alphanumerics as `_`; e.g. `AXEL_ACP_CLAUDE="npx -y @zed-industries/claude-code-acp"`).
/// Defaults: claude → `axel claude-acp`, codex → `axel codex-acp` (both built in), opencode →
/// [`Harness::acp_command`] for the other builtins (`opencode acp`, `goose acp`,
/// `cursor-agent acp`, …); a custom command is split like a shell would (quotes
/// honoured, no expansion).
pub fn adapter_command(harness: &Harness) -> Vec<String> {
    if let Ok(v) = std::env::var(env_var_name(harness))
        && !v.trim().is_empty()
    {
        return shell_split(&v);
    }
    default_command(harness)
}

/// `AXEL_ACP_<HARNESS>`.
pub fn env_var_name(harness: &Harness) -> String {
    let name: String = harness
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("AXEL_ACP_{name}")
}

fn default_command(harness: &Harness) -> Vec<String> {
    match harness {
        // Built in: `axel claude-acp` drives the `claude` CLI directly (see
        // `crate::claude`).  `AXEL_ACP_CLAUDE=claude-code-acp` goes back to
        // Zed's Node adapter.
        Harness::Claude => match std::env::current_exe() {
            Ok(exe) => vec![exe.to_string_lossy().into_owned(), "claude-acp".into()],
            Err(_) => vec!["axel".into(), "claude-acp".into()],
        },
        // Built in too: `axel codex-acp` drives `codex app-server` (see
        // `crate::codex`).  `AXEL_ACP_CODEX=codex-acp` goes back to Zed's.
        Harness::Codex => match std::env::current_exe() {
            Ok(exe) => vec![exe.to_string_lossy().into_owned(), "codex-acp".into()],
            Err(_) => vec!["axel".into(), "codex-acp".into()],
        },
        Harness::Custom(cmd) => shell_split(cmd),
        // The rest speak ACP themselves, or through an adapter of their own.
        builtin => builtin.acp_command().unwrap_or_default().iter().map(|w| w.to_string()).collect(),
    }
}

/// Split a command line on whitespace, honouring single and double quotes
/// and backslash escapes.  No variable expansion or globbing.
pub fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(&n) = chars.peek()
                    && matches!(n, '"' | '\\' | '$' | '`')
                {
                    cur.push(n);
                    chars.next();
                } else {
                    cur.push(c);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            None if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let claude = adapter_command(&Harness::Claude);
        assert_eq!(claude.len(), 2, "{claude:?}");
        assert_eq!(claude[1], "claude-acp", "the built-in adapter: this binary's `claude-acp`");
        assert_eq!(adapter_command(&Harness::Codex)[1], "codex-acp", "built in");
        assert_eq!(adapter_command(&Harness::Opencode), vec!["opencode", "acp"]);
        assert_eq!(adapter_command(&Harness::Goose), vec!["goose", "acp"]);
        assert_eq!(adapter_command(&Harness::Cursor), vec!["cursor-agent", "acp"]);
        assert_eq!(adapter_command(&Harness::Pi), vec!["pi-acp"]);
        assert_eq!(adapter_command(&Harness::Amp), vec!["amp-acp"]);
        assert_eq!(adapter_command(&Harness::Grok), vec!["grok", "agent", "stdio"]);
        for h in Harness::BUILTIN {
            assert!(!adapter_command(&h).is_empty(), "{h} has an adapter");
        }
        assert_eq!(
            adapter_command(&Harness::Custom("python my_agent.py --flag".into())),
            vec!["python", "my_agent.py", "--flag"]
        );
    }

    #[test]
    fn env_names() {
        assert_eq!(env_var_name(&Harness::Claude), "AXEL_ACP_CLAUDE");
        assert_eq!(
            env_var_name(&Harness::Goose),
            "AXEL_ACP_GOOSE"
        );
        assert_eq!(
            env_var_name(&Harness::Custom("my-agent v2".into())),
            "AXEL_ACP_MY_AGENT_V2"
        );
    }

    #[test]
    fn env_override_wins() {
        // A harness name no other test touches, so the env var is private to this test.
        let h = Harness::Custom("zz-override-test".into());
        // SAFETY: tests in this crate do not read this variable concurrently.
        unsafe {
            std::env::set_var(
                "AXEL_ACP_ZZ_OVERRIDE_TEST",
                "npx -y \"@scope/pkg name\" --x",
            )
        };
        assert_eq!(
            adapter_command(&h),
            vec!["npx", "-y", "@scope/pkg name", "--x"]
        );
        unsafe { std::env::remove_var("AXEL_ACP_ZZ_OVERRIDE_TEST") };
        assert_eq!(adapter_command(&h), vec!["zz-override-test"]);
    }

    #[test]
    fn splitting() {
        assert_eq!(shell_split("a  b\tc"), vec!["a", "b", "c"]);
        assert_eq!(shell_split("a 'b c' \"d e\""), vec!["a", "b c", "d e"]);
        assert_eq!(shell_split(r#"a b\ c "d\"e""#), vec!["a", "b c", "d\"e"]);
        assert_eq!(shell_split("  "), Vec::<String>::new());
        assert_eq!(shell_split("''"), vec![""]);
    }
}
