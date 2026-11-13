//! Turn an [`AgentProfile`] into something a launcher can exec: program, argv and
//! environment. Ported from Axel v1's `ClaudeCommand` builder, generalised to every
//! harness. Nothing here reads secrets: an API key is referenced only by the *name* of
//! its environment variable, under [`API_KEY_ENV_KEY`].

use std::fmt;

use serde::Serialize;

use crate::{AgentProfile, Harness, HarnessKind, config::AXEL_MD};

/// Env key whose value is the *name* of the environment variable holding the API key
/// (from `api.api_key_env`). The launcher resolves it; this crate never reads the key.
pub const API_KEY_ENV_KEY: &str = "AXEL_API_KEY_ENV";

/// Program + arguments + extra environment for one agent launch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl LaunchSpec {
    /// `[program, args...]`.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }

    /// Shell-quoted command line, for display or for a `sh -c` wrapper.
    pub fn command_line(&self) -> String {
        self.argv()
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Value of an env entry.
    pub fn env_var(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Display for LaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.command_line())
    }
}

/// Environment variable each builtin harness reads its API key from, for a launcher
/// that resolves [`API_KEY_ENV_KEY`].
pub fn api_key_target(kind: HarnessKind) -> Option<&'static str> {
    match kind {
        HarnessKind::Claude => Some("ANTHROPIC_API_KEY"),
        HarnessKind::Codex => Some("OPENAI_API_KEY"),
        HarnessKind::Opencode | HarnessKind::Custom => None,
    }
}

/// Build the launch spec for `profile`. `prompt` overrides `profile.prompt`.
///
/// - claude: `claude [--model M] [--allowedTools a,b] [--disallowedTools c] <harness args> <profile args> [prompt]`
/// - codex: `codex -c project_doc_fallback_filenames=["AXEL.md"] [--model M] <args> [prompt]`
/// - opencode: `opencode [--model M] <args> [--prompt prompt]`
/// - custom: `<command words> <harness args> <profile args>` (prompt ignored); with no
///   command, `$SHELL` (or `sh`).
///
/// With an `api` block the env gets the harness's base URL variable
/// (`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `AXEL_API_BASE_URL`), the model
/// (`ANTHROPIC_MODEL` / `CODEX_MODEL`), `AXEL_API_PROVIDER` and [`API_KEY_ENV_KEY`].
pub fn build(profile: &AgentProfile, prompt: Option<&str>) -> LaunchSpec {
    let kind = profile.harness.kind;
    let prompt = non_blank(prompt).or_else(|| non_blank(profile.prompt.as_deref()));

    let (program, mut args) = program_and_prefix(&profile.harness);

    match kind {
        HarnessKind::Claude => {
            push_opt(&mut args, "--model", profile.model.as_deref());
            push_list(&mut args, "--allowedTools", &profile.allowed_tools);
            push_list(&mut args, "--disallowedTools", &profile.disallowed_tools);
        }
        HarnessKind::Codex => {
            args.push("-c".to_string());
            args.push(format!("project_doc_fallback_filenames=[\"{AXEL_MD}\"]"));
            push_opt(&mut args, "--model", profile.model.as_deref());
        }
        HarnessKind::Opencode => {
            push_opt(&mut args, "--model", profile.model.as_deref());
        }
        HarnessKind::Custom => {}
    }

    args.extend(profile.harness.args.iter().cloned());
    args.extend(profile.args.iter().cloned());

    if let Some(prompt) = prompt {
        match kind {
            HarnessKind::Claude | HarnessKind::Codex => args.push(prompt.to_string()),
            HarnessKind::Opencode => {
                args.push("--prompt".to_string());
                args.push(prompt.to_string());
            }
            HarnessKind::Custom => {}
        }
    }

    let mut env = Vec::new();
    if let Some(api) = &profile.api {
        let (url_key, model_key) = match kind {
            HarnessKind::Claude => ("ANTHROPIC_BASE_URL", Some("ANTHROPIC_MODEL")),
            HarnessKind::Codex => ("OPENAI_BASE_URL", Some("CODEX_MODEL")),
            HarnessKind::Opencode | HarnessKind::Custom => ("AXEL_API_BASE_URL", None),
        };
        if let Some(url) = &api.base_url {
            env.push((url_key.to_string(), url.clone()));
        }
        if let (Some(key), Some(model)) = (model_key, &profile.model) {
            env.push((key.to_string(), model.clone()));
        }
        if let Some(provider) = &api.provider {
            env.push(("AXEL_API_PROVIDER".to_string(), provider.clone()));
        }
        if let Some(key_env) = &api.api_key_env {
            env.push((API_KEY_ENV_KEY.to_string(), key_env.clone()));
        }
    }

    LaunchSpec { program, args, env }
}

/// Executable and the arguments baked into `harness.command` (it may be a command line
/// such as `npm run dev`).
fn program_and_prefix(harness: &Harness) -> (String, Vec<String>) {
    if let Some(command) = &harness.command {
        let mut words = split_command(command);
        if !words.is_empty() {
            let program = words.remove(0);
            return (program, words);
        }
    }
    match harness.kind.program() {
        Some(program) => (program.to_string(), Vec::new()),
        None => (
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
            Vec::new(),
        ),
    }
}

fn non_blank(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

fn push_opt(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = non_blank(value) {
        args.push(flag.to_string());
        args.push(value.to_string());
    }
}

fn push_list(args: &mut Vec<String>, flag: &str, values: &[String]) {
    if !values.is_empty() {
        args.push(flag.to_string());
        args.push(values.join(","));
    }
}

/// Split a command line into words the way a POSIX shell would for the common cases:
/// whitespace separates words; single quotes are literal; double quotes allow `\"`,
/// `\\`, `\$` and `` \` `` escapes; a backslash outside quotes escapes the next char.
/// No expansion is performed.
pub fn split_command(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some('"') => match c {
                '"' => quote = None,
                '\\' => match chars.next() {
                    Some(n @ ('"' | '\\' | '$' | '`')) => current.push(n),
                    Some(n) => {
                        current.push('\\');
                        current.push(n);
                    }
                    None => current.push('\\'),
                },
                _ => current.push(c),
            },
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        current.push(n);
                        in_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                _ => {
                    current.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Quote a word for a POSIX shell. Plain words are returned unchanged.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+%".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiConfig;

    fn profile(kind: HarnessKind) -> AgentProfile {
        AgentProfile::new(kind.as_str(), Harness::builtin(kind))
    }

    #[test]
    fn claude_basic_and_full() {
        let spec = build(&profile(HarnessKind::Claude), None);
        assert_eq!(spec.program, "claude");
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert_eq!(spec.command_line(), "claude");

        let mut p = profile(HarnessKind::Claude);
        p.model = Some("sonnet".into());
        p.allowed_tools = vec!["Read".into(), "Write".into()];
        p.disallowed_tools = vec!["Bash".into()];
        p.args = vec!["--verbose".into()];
        p.prompt = Some("default prompt".into());
        let spec = build(&p, Some("Fix the login bug"));
        assert_eq!(
            spec.args,
            [
                "--model",
                "sonnet",
                "--allowedTools",
                "Read,Write",
                "--disallowedTools",
                "Bash",
                "--verbose",
                "Fix the login bug"
            ]
        );
        assert_eq!(
            spec.command_line(),
            "claude --model sonnet --allowedTools Read,Write --disallowedTools Bash --verbose 'Fix the login bug'"
        );

        let spec = build(&p, None);
        assert_eq!(spec.args.last().map(String::as_str), Some("default prompt"));
        let spec = build(&p, Some("   "));
        assert_eq!(spec.args.last().map(String::as_str), Some("default prompt"));
    }

    #[test]
    fn codex_sets_project_doc_fallback() {
        let mut p = profile(HarnessKind::Codex);
        p.model = Some("gpt-5-codex".into());
        p.harness.args = vec!["--full-auto".into()];
        let spec = build(&p, Some("hi"));
        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            [
                "-c",
                "project_doc_fallback_filenames=[\"AXEL.md\"]",
                "--model",
                "gpt-5-codex",
                "--full-auto",
                "hi"
            ]
        );
    }

    #[test]
    fn opencode_model_and_prompt() {
        let mut p = profile(HarnessKind::Opencode);
        p.model = Some("anthropic/claude-sonnet-4".into());
        let spec = build(&p, Some("go"));
        assert_eq!(spec.program, "opencode");
        assert_eq!(
            spec.args,
            ["--model", "anthropic/claude-sonnet-4", "--prompt", "go"]
        );
    }

    #[test]
    fn custom_splits_command_and_ignores_prompt() {
        let mut p = AgentProfile::new(
            "backend",
            Harness::custom(
                "npm run \"dev server\"",
                vec!["--port".into(), "3000".into()],
            ),
        );
        p.args = vec!["--open".into()];
        let spec = build(&p, Some("ignored"));
        assert_eq!(spec.program, "npm");
        assert_eq!(spec.args, ["run", "dev server", "--port", "3000", "--open"]);

        let explicit = AgentProfile::new(
            "c",
            Harness {
                kind: HarnessKind::Claude,
                command: Some("/opt/bin/claude".into()),
                args: vec![],
            },
        );
        assert_eq!(build(&explicit, None).program, "/opt/bin/claude");

        let shell = AgentProfile::new("sh", Harness::builtin(HarnessKind::Custom));
        let spec = build(&shell, None);
        assert_eq!(
            spec.program,
            std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
        );
    }

    #[test]
    fn api_env_never_includes_the_key() {
        let api = ApiConfig {
            protocol: None,
            provider: Some("openrouter".into()),
            base_url: Some("https://openrouter.ai/api/v1".into()),
            api_key_env: Some("OPENROUTER_API_KEY".into()),
        };

        let mut p = profile(HarnessKind::Claude);
        p.model = Some("anthropic/claude-sonnet-4".into());
        p.api = Some(api.clone());
        let spec = build(&p, None);
        assert_eq!(
            spec.env_var("ANTHROPIC_BASE_URL"),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(
            spec.env_var("ANTHROPIC_MODEL"),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(spec.env_var("AXEL_API_PROVIDER"), Some("openrouter"));
        assert_eq!(spec.env_var(API_KEY_ENV_KEY), Some("OPENROUTER_API_KEY"));
        assert!(
            spec.env
                .iter()
                .all(|(k, _)| k != "ANTHROPIC_API_KEY" && k != "OPENAI_API_KEY")
        );

        let mut p = profile(HarnessKind::Codex);
        p.model = Some("gpt-5".into());
        p.api = Some(api.clone());
        let spec = build(&p, None);
        assert_eq!(
            spec.env_var("OPENAI_BASE_URL"),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(spec.env_var("CODEX_MODEL"), Some("gpt-5"));

        let mut p = profile(HarnessKind::Opencode);
        p.api = Some(api);
        let spec = build(&p, None);
        assert_eq!(
            spec.env_var("AXEL_API_BASE_URL"),
            Some("https://openrouter.ai/api/v1")
        );
        assert!(spec.env_var("CODEX_MODEL").is_none());

        // No api block: model is only a CLI flag.
        let mut p = profile(HarnessKind::Claude);
        p.model = Some("opus".into());
        assert!(build(&p, None).env.is_empty());

        assert_eq!(
            api_key_target(HarnessKind::Claude),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn split_and_quote() {
        assert_eq!(split_command("a  b\tc"), ["a", "b", "c"]);
        assert_eq!(
            split_command("tail -f '/var/log/my app.log'"),
            ["tail", "-f", "/var/log/my app.log"]
        );
        assert_eq!(
            split_command(r#"echo "say \"hi\" $x" done"#),
            ["echo", "say \"hi\" $x", "done"]
        );
        assert_eq!(split_command(r"a\ b c"), ["a b", "c"]);
        assert_eq!(split_command("''"), [""]);
        assert!(split_command("   ").is_empty());

        assert_eq!(shell_quote("plain-word.1"), "plain-word.1");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
