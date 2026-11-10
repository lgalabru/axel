//! Drivers for the programs agents run most: what a command's output is,
//! known by the program that printed it, shown the way that program's own
//! terminal output reads (cargo's diagnostics and test results, git's diffs,
//! log and status, grep's matches), with a one-line summary for the header.

use super::{change_bg, Change, Cell, Lang, Link, Linker, colour_lines, emit_linked, grep_view, parse_grep_line, GrepLine, MAX_LINES};

/// The program whose output a command shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tool {
    Grep,
    Cargo,
    Git(Git),
    Other,
}

/// What a `git` command prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Git {
    /// `git diff`, `git show`, `git log -p`: a patch.
    Diff,
    /// `git log`.
    Log,
    /// `git status`.
    Status,
    Other,
}

/// Programs that only filter or trim what's piped into them.
const FILTERS: [&str; 9] = ["grep", "rg", "head", "tail", "sed", "awk", "sort", "uniq", "cut"];

/// The tool whose output a command ends with: the last step's first program
/// (`cd x && cargo clippy 2>&1 | grep -A6 error | head` is cargo's output,
/// filtered), skipping `VAR=value`, `env` and `time` in front of it.
pub(super) fn tool_of(cmd: &str) -> Tool {
    let Some(step) = cmd.split("&&").flat_map(|s| s.split("||")).flat_map(|s| s.split(';')).map(str::trim).filter(|s| !s.is_empty() && !s.starts_with("cd ")).last() else {
        return Tool::Other;
    };
    let stages: Vec<Vec<&str>> = step.split('|').map(|s| program_words(s.trim())).collect();
    let Some(first) = stages.first() else { return Tool::Other };
    let program = first.first().map(|p| p.rsplit('/').next().unwrap_or(p)).unwrap_or("");
    match program {
        "grep" | "rg" | "egrep" | "fgrep" => Tool::Grep,
        "cargo" => Tool::Cargo,
        "git" => Tool::Git(match git_subcommand(&first[1..]) {
            Some("diff" | "show") => Git::Diff,
            Some("log") if first.iter().any(|w| *w == "-p" || *w == "--patch") => Git::Diff,
            Some("log") => Git::Log,
            Some("status") => Git::Status,
            _ => Git::Other,
        }),
        // `cat file | grep -n x`: what's shown is the search.
        "cat" if stages.iter().skip(1).any(|s| matches!(s.first().copied(), Some("grep" | "rg"))) => Tool::Grep,
        p if FILTERS.contains(&p) => Tool::Other,
        _ => Tool::Other,
    }
}

/// git's subcommand, past its own options (`-C dir`, `-c key=value`, `--no-pager`).
pub(super) fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut words = args.iter().copied();
    while let Some(w) = words.next() {
        match w {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                words.next();
            }
            w if w.starts_with('-') => {}
            w => return Some(w),
        }
    }
    None
}

/// What a command does, for a header when the agent didn't say: the
/// programs it runs, in order, a tool's subcommands together
/// (`git add, commit, push, log`, `gh pr view · cat · gh pr edit`). `cd`
/// and what a pipe only filters are left out.
pub(super) fn title_of(cmd: &str) -> String {
    // A heredoc's body isn't commands; what runs before and after it is.
    let full = format!("$ {cmd}");
    let script = match super::split_heredoc(&full) {
        Some(h) => {
            let after = h.rest.split_once('\n').map_or("", |(_, a)| a);
            format!("{}\n{after}", h.head.trim_start_matches("$ ").split("<<").next().unwrap_or(""))
        }
        None => cmd.to_string(),
    };
    let mut runs: Vec<(String, Vec<String>)> = Vec::new();
    for (op, step) in super::shell_steps(&script) {
        let words = program_words(step);
        let Some(program) = words.first().map(|p| p.rsplit('/').next().unwrap_or(p)) else { continue };
        if program == "cd" || (op == "|" && FILTERS.contains(&program)) {
            continue;
        }
        let sub = match program {
            "git" => git_subcommand(&words[1..]).map(str::to_string),
            "gh" => {
                let w: Vec<&str> = words[1..].iter().copied().filter(|w| !w.starts_with('-')).take(2).collect();
                (!w.is_empty()).then(|| w.join(" "))
            }
            "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "just" | "docker" | "kubectl" | "brew" | "go" | "rex" | "axel" | "make" | "uv" | "pip" => {
                words[1..].iter().find(|w| !w.starts_with('-') && !w.starts_with('+')).map(|w| w.to_string())
            }
            _ => None,
        };
        match (runs.last_mut(), sub) {
            (Some((p, subs)), Some(sub)) if p == program => {
                if subs.last() != Some(&sub) && !subs.contains(&sub) {
                    subs.push(sub);
                }
            }
            (Some((p, subs)), None) if p == program && subs.is_empty() => {}
            (_, sub) => runs.push((program.to_string(), sub.into_iter().collect())),
        }
    }
    if runs.is_empty() {
        return "Run a command".into();
    }
    let more = runs.len().saturating_sub(5);
    let mut parts: Vec<String> = runs.into_iter().take(5).map(|(p, subs)| if subs.is_empty() { p } else { format!("{p} {}", subs.join(", ")) }).collect();
    if more > 0 {
        parts.push(format!("+{more}"));
    }
    parts.join(" · ")
}

/// A stage's words from its program on.
fn program_words(stage: &str) -> Vec<&str> {
    let words: Vec<&str> = stage.split_whitespace().collect();
    let skip = words.iter().take_while(|w| matches!(**w, "env" | "time" | "command" | "exec") || (w.contains('=') && !w.starts_with('-') && !w.starts_with('='))).count();
    words[skip..].to_vec()
}

/// The output as `tool` shows it, wrapped to `width`; `None` for a tool
/// without a driver (or output that isn't what the driver expects).
pub(super) fn tool_view(tool: Tool, cmd: &str, output: &str, width: usize, links: &Linker) -> Option<String> {
    match tool {
        Tool::Grep => grep_view(cmd, output, width, links),
        Tool::Cargo => Some(cargo_view(output, width, &links.after_cd(cmd))),
        Tool::Git(Git::Diff) => Some(git_diff_view(output, width, &links.after_cd(cmd))),
        Tool::Git(Git::Log) => Some(git_log_view(output, width)),
        Tool::Git(Git::Status) => Some(git_status_view(output, width, &links.after_cd(cmd))),
        // Something else, ending in a search (`sed -n 1,9p a.rs; grep -n x b.rs`).
        Tool::Git(Git::Other) | Tool::Other => grep_view(cmd, output, width, links),
    }
}

/// The header's word on how it went, from the output: `3 errors · 1 warning`,
/// `462 passed`, `12 matches`.
pub(super) fn summary(tool: Tool, output: &str) -> Option<String> {
    match tool {
        Tool::Cargo => cargo_summary(output),
        Tool::Grep => {
            let n = output.lines().filter(|l| matches!(parse_grep_line(l), Some(GrepLine::Hit { matched: true, .. }))).count();
            (n > 0).then(|| plural(n, "match", "matches"))
        }
        _ => None,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ================================================================== cargo

const ERROR: &str = "1;31";
const WARNING: &str = "1;33";
const NOTE: &str = "1;36";
const GUTTER: &str = "1;34";
const VERB: &str = "1;32";

/// What a cargo diagnostic is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Error,
    Warning,
}

/// The counts in cargo's output: `2 errors · 1 warning`, or the tests run.
fn cargo_summary(output: &str) -> Option<String> {
    let (mut errors, mut warnings, mut passed, mut failed, mut tests) = (0, 0, 0, 0, false);
    for l in output.lines() {
        if (l.starts_with("error:") || l.starts_with("error[")) && !l.starts_with("error: could not compile") && !l.starts_with("error: aborting") {
            errors += 1;
        } else if l.starts_with("warning: ") && !l.contains(" generated ") && !l.contains("warnings emitted") {
            warnings += 1;
        } else if let Some(rest) = l.strip_prefix("test result: ") {
            tests = true;
            passed += count_before(rest, " passed");
            failed += count_before(rest, " failed");
        }
    }
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(plural(errors, "error", "errors"));
    }
    if warnings > 0 {
        parts.push(plural(warnings, "warning", "warnings"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if tests && passed + failed > 0 {
        parts.push(format!("{passed} passed"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The number right before `word` (`5 passed;` → 5).
fn count_before(s: &str, word: &str) -> usize {
    s.find(word).and_then(|at| s[..at].rsplit([' ', '.', ';']).next()?.parse().ok()).unwrap_or(0)
}

/// Cargo's output as cargo colours it: `error`/`warning` headings, `-->`
/// locations (Cmd+click opens them), the gutter and its code, markers, and
/// the verbs (`Compiling`, `Finished`) and test results.
fn cargo_view(output: &str, width: usize, links: &Linker) -> String {
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut spans: Vec<Vec<Link>> = Vec::new();
    let mut level = Level::Error;
    for l in output.lines().take(MAX_LINES) {
        let mut line_links = Vec::new();
        let cells = cargo_line(l, &mut level, links, &mut line_links);
        lines.push(cells);
        spans.push(line_links);
    }
    let mut out = emit_linked(&lines, &spans, width, "", None);
    let more = output.lines().count().saturating_sub(MAX_LINES);
    if more > 0 {
        out.push_str(&format!("{}\x1b[2m… {more} more lines\x1b[0m\r\n", super::INDENT));
    }
    out
}

fn cargo_line(l: &str, level: &mut Level, links: &Linker, line_links: &mut Vec<Link>) -> Vec<Cell> {
    let paint = |s: &str, c: Option<&'static str>| -> Vec<Cell> { s.chars().map(|ch| (ch, c)).collect() };
    let trimmed = l.trim_start();
    let pad = &l[..l.len() - trimmed.len()];
    // error[E0425]: message / warning: message
    for (word, lvl, colour) in [("error", Level::Error, ERROR), ("warning", Level::Warning, WARNING)] {
        if let Some(rest) = l.strip_prefix(word)
            && (rest.starts_with(':') || rest.starts_with('['))
        {
            *level = lvl;
            let colon = rest.find(':').unwrap_or(rest.len());
            let mut cells = paint(&l[..word.len() + colon], Some(colour));
            cells.extend(paint(&rest[colon..], Some("1")));
            return cells;
        }
    }
    // note: / help: / = note: …
    let label = trimmed.strip_prefix("= ").unwrap_or(trimmed);
    for word in ["note", "help"] {
        if let Some(rest) = label.strip_prefix(word).filter(|r| r.starts_with(':')) {
            let mut cells = paint(&l[..l.len() - label.len()], Some(GUTTER));
            cells.extend(paint(word, Some(if word == "help" { NOTE } else { "1" })));
            cells.extend(paint(rest, None));
            return cells;
        }
    }
    // --> path:12:5
    if let Some(loc) = trimmed.strip_prefix("--> ").or_else(|| trimmed.strip_prefix("::: ")) {
        let mut cells = paint(pad, None);
        cells.extend(paint(&trimmed[..4], Some(GUTTER)));
        let from = cells.len();
        cells.extend(paint(loc, None));
        let mut parts = loc.split(':');
        if let (Some(path), Some(line)) = (parts.next(), parts.next().and_then(|n| n.parse().ok()))
            && let Some(url) = links.url(path, Some(line))
        {
            line_links.push(Link { from, to: cells.len(), url });
        }
        return cells;
    }
    // 12 |     code   /   |     ^^^^ marker
    if let Some(bar) = trimmed.find('|')
        && trimmed[..bar].trim().chars().all(|c| c.is_ascii_digit())
        && (bar == 0 || trimmed[..bar].ends_with(' '))
    {
        let mut cells = paint(&l[..pad.len() + bar + 1], Some(GUTTER));
        let body = &trimmed[bar + 1..];
        let code_like = !trimmed[..bar].trim().is_empty();
        if code_like {
            cells.extend(colour_lines(body, Lang::Named("rust")).into_iter().next().unwrap_or_default());
        } else {
            let colour = match level {
                Level::Error => ERROR,
                Level::Warning => WARNING,
            };
            let marks = body.trim_start();
            let lead = &body[..body.len() - marks.len()];
            cells.extend(paint(lead, None));
            cells.extend(paint(marks, if marks.starts_with(['^', '-', '~', '+']) { Some(colour) } else { None }));
        }
        return cells;
    }
    // Compiling pay v0.1.0 / Finished `dev` profile
    let verb = trimmed.split_whitespace().next().unwrap_or("");
    if !pad.is_empty()
        && matches!(verb, "Compiling" | "Checking" | "Finished" | "Running" | "Fresh" | "Documenting" | "Doc-tests" | "Downloaded" | "Downloading" | "Updating" | "Locking" | "Adding" | "Removing" | "Building" | "Blocking" | "Installing" | "Installed" | "Replacing" | "Packaging" | "Verifying" | "Uploading")
    {
        let mut cells = paint(&l[..pad.len() + verb.len()], Some(VERB));
        cells.extend(paint(&trimmed[verb.len()..], None));
        return cells;
    }
    // test name ... ok
    if l.starts_with("test ")
        && let Some(at) = l.rfind(" ... ")
    {
        let result = &l[at + 5..];
        let colour = match result {
            r if r.starts_with("ok") => Some("32"),
            r if r.starts_with("FAILED") => Some(ERROR),
            r if r.starts_with("ignored") => Some("33"),
            _ => None,
        };
        let mut cells = paint(&l[..at + 5], None);
        cells.extend(paint(result, colour));
        return cells;
    }
    // test result: ok. 5 passed; …
    if let Some(rest) = l.strip_prefix("test result: ") {
        let colour = if rest.starts_with("ok") { "1;32" } else { ERROR };
        let mut cells = paint("test result: ", None);
        let word = rest.split('.').next().unwrap_or("");
        cells.extend(paint(word, Some(colour)));
        cells.extend(paint(&rest[word.len()..], None));
        return cells;
    }
    if l.starts_with("---- ") || l == "failures:" || l.starts_with("thread '") {
        return paint(l, Some("1"));
    }
    paint(l, None)
}

// ==================================================================== git

/// A patch as `git diff` colours it, each line's code highlighted in its
/// file's language, additions and removals on green and red backgrounds.
fn git_diff_view(output: &str, width: usize, links: &Linker) -> String {
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut spans: Vec<Vec<Link>> = Vec::new();
    let removed_bg = change_bg(Change::Removed);
    let added_bg = change_bg(Change::Added);
    let mut bgs: Vec<Option<&str>> = Vec::new();
    let mut file: Option<String> = None;
    let mut new_line: Option<usize> = None;
    let paint = |s: &str, c: Option<&'static str>| -> Vec<Cell> { s.chars().map(|ch| (ch, c)).collect() };
    let code = |s: &str, file: &Option<String>| -> Vec<Cell> {
        match file {
            Some(f) => colour_lines(s, Lang::File(f)).into_iter().next().unwrap_or_default(),
            None => paint(s, None),
        }
    };
    for l in output.lines().take(MAX_LINES) {
        let mut line_links = Vec::new();
        let (cells, bg) = if l.starts_with("diff --git") || l.starts_with("commit ") {
            (paint(l, Some(if l.starts_with("commit ") { "33" } else { "1" })), None)
        } else if let Some(path) = l.strip_prefix("+++ ") {
            let path = path.strip_prefix("b/").unwrap_or(path);
            file = (path != "/dev/null").then(|| path.to_string());
            let mut cells = paint("+++ ", Some("1"));
            let from = cells.len();
            cells.extend(paint(path, Some("1")));
            if let Some(url) = links.url(path, None) {
                line_links.push(Link { from, to: cells.len(), url });
            }
            (cells, None)
        } else if l.starts_with("--- ") || l.starts_with("index ") || l.starts_with("new file") || l.starts_with("deleted file") || l.starts_with("similarity") || l.starts_with("rename ") {
            (paint(l, Some("1")), None)
        } else if let Some(rest) = l.strip_prefix("@@") {
            // @@ -10,6 +12,8 @@ fn context
            let close = rest.find("@@").map(|i| i + 4).unwrap_or(l.len()).min(l.len());
            new_line = rest.split_whitespace().find_map(|w| w.strip_prefix('+')).and_then(|w| w.split(',').next()?.parse().ok());
            let mut cells = paint(&l[..close], Some("36"));
            if let (Some(f), Some(n)) = (&file, new_line)
                && let Some(url) = links.url(f, Some(n))
            {
                line_links.push(Link { from: 0, to: cells.len(), url });
            }
            cells.extend(code(&l[close..], &file));
            (cells, None)
        } else if let Some(rest) = l.strip_prefix('+') {
            new_line = new_line.map(|n| n + 1);
            let mut cells = paint("+", Some("32"));
            cells.extend(code(rest, &file));
            (cells, Some(added_bg.as_str()))
        } else if let Some(rest) = l.strip_prefix('-') {
            let mut cells = paint("-", Some("31"));
            cells.extend(code(rest, &file));
            (cells, Some(removed_bg.as_str()))
        } else if let Some(rest) = l.strip_prefix(' ') {
            new_line = new_line.map(|n| n + 1);
            let mut cells = paint(" ", None);
            cells.extend(code(rest, &file));
            (cells, None)
        } else if l.starts_with("Author:") || l.starts_with("Date:") || l.starts_with("Merge:") {
            (paint(l, Some("2")), None)
        } else {
            (paint(l, None), None)
        };
        lines.push(cells);
        spans.push(line_links);
        bgs.push(bg);
    }
    // Lines on a background are emitted on their own so it runs to the edge.
    let mut out = String::new();
    for ((cells, links), bg) in lines.into_iter().zip(spans).zip(bgs) {
        out.push_str(&emit_linked(&[cells], &[links], width, "", bg));
    }
    out
}

/// `git log`: hashes yellow, refs bold, authors and dates dim.
fn git_log_view(output: &str, width: usize) -> String {
    let paint = |s: &str, c: Option<&'static str>| -> Vec<Cell> { s.chars().map(|ch| (ch, c)).collect() };
    let lines: Vec<Vec<Cell>> = output
        .lines()
        .take(MAX_LINES)
        .map(|l| {
            let hash_len = l.chars().take_while(|c| c.is_ascii_hexdigit()).count();
            if let Some(rest) = l.strip_prefix("commit ") {
                let hash = rest.chars().take_while(|c| c.is_ascii_hexdigit()).count();
                let mut cells = paint("commit ", Some("33"));
                cells.extend(paint(&rest[..hash], Some("33")));
                cells.extend(decorated(&rest[hash..], &paint));
                cells
            } else if (7..=40).contains(&hash_len) && l[hash_len..].starts_with(' ') {
                // --oneline
                let mut cells = paint(&l[..hash_len], Some("33"));
                cells.extend(decorated(&l[hash_len..], &paint));
                cells
            } else if l.starts_with("Author:") || l.starts_with("Date:") || l.starts_with("Merge:") {
                paint(l, Some("2"))
            } else {
                paint(l, None)
            }
        })
        .collect();
    emit_linked(&lines, &[], width, "", None)
}

/// A hash's line after the hash: ` (HEAD -> main, origin/main) subject`, the
/// refs coloured as git does.
fn decorated(rest: &str, paint: &dyn Fn(&str, Option<&'static str>) -> Vec<Cell>) -> Vec<Cell> {
    let trimmed = rest.trim_start();
    let lead = &rest[..rest.len() - trimmed.len()];
    let Some(close) = trimmed.strip_prefix('(').and_then(|r| r.find(')')) else {
        return paint(rest, None);
    };
    let mut cells = paint(lead, None);
    cells.extend(paint("(", Some("33")));
    for (i, r) in trimmed[1..close + 1].split(", ").enumerate() {
        if i > 0 {
            cells.extend(paint(", ", Some("33")));
        }
        let colour = if r.starts_with("HEAD") {
            "1;36"
        } else if r.starts_with("tag: ") {
            "1;33"
        } else if r.contains('/') {
            "1;31"
        } else {
            "1;32"
        };
        cells.extend(paint(r, Some(colour)));
    }
    cells.extend(paint(")", Some("33")));
    cells.extend(paint(&trimmed[close + 2..], None));
    cells
}

/// `git status`, long or short: staged changes green, the rest red, as git
/// colours them; each path opens its file.
fn git_status_view(output: &str, width: usize, links: &Linker) -> String {
    let paint = |s: &str, c: Option<&'static str>| -> Vec<Cell> { s.chars().map(|ch| (ch, c)).collect() };
    let mut staged = false;
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    for l in output.lines().take(MAX_LINES) {
        let mut line_links = Vec::new();
        let mut link = |cells: &mut Vec<Cell>, path: &str, colour: Option<&'static str>| {
            let from = cells.len();
            cells.extend(paint(path, colour));
            if let Some(url) = links.url(path.split(" -> ").last().unwrap_or(path), None) {
                line_links.push(Link { from, to: cells.len(), url });
            }
        };
        let cells = if l.starts_with("Changes to be committed") {
            staged = true;
            paint(l, None)
        } else if l.starts_with("Changes not staged") || l.starts_with("Untracked files") || l.starts_with("Unmerged paths") {
            staged = false;
            paint(l, None)
        } else if let Some(rest) = l.strip_prefix('\t') {
            let colour = Some(if staged { "32" } else { "31" });
            let mut cells = paint("\t", None);
            match rest.split_once(":   ") {
                Some((what, path)) => {
                    cells.extend(paint(&format!("{what}:   "), colour));
                    link(&mut cells, path.trim(), colour);
                }
                None => link(&mut cells, rest.trim(), colour),
            }
            cells
        } else if l.len() > 3 && l.as_bytes()[2] == b' ' && l[..2].chars().all(|c| " MADRCU?!".contains(c)) {
            // --short: `XY path`, X staged (green), Y not (red).
            let (x, y) = (&l[..1], &l[1..2]);
            let mut cells = paint(x, Some(if x == "?" { "31" } else { "32" }));
            cells.extend(paint(y, Some("31")));
            cells.push((' ', None));
            link(&mut cells, &l[3..], None);
            cells
        } else {
            paint(l, if l.starts_with("  (") { Some("2") } else { None })
        };
        lines.push(cells);
        spans.push(line_links);
    }
    emit_linked(&lines, &spans, width, "", None)
}

#[cfg(test)]
mod tests {
    use super::super::{Editor, url_path};
    use super::*;

    fn plain(s: &str) -> String {
        super::super::tests::plain(s)
    }

    #[test]
    fn the_tool_is_the_last_steps_program() {
        assert_eq!(tool_of("cd x && sed -i '' 's/a/b/' f.rs && cargo fmt --all && cargo clippy -p pay -- -D warnings 2>&1 | grep -E \"^(error|warning: )\" -A6 | head -30"), Tool::Cargo);
        assert_eq!(tool_of("RUST_LOG=debug cargo test -p axel"), Tool::Cargo);
        assert_eq!(tool_of("grep -rn x crates | grep -v y"), Tool::Grep);
        assert_eq!(tool_of("cat a.rs | grep -n fn"), Tool::Grep);
        assert_eq!(tool_of("sed -n 1,9p a.rs; grep -n x b.rs"), Tool::Grep);
        assert_eq!(tool_of("git -C repo diff --stat HEAD~1"), Tool::Git(Git::Diff));
        assert_eq!(tool_of("git show HEAD"), Tool::Git(Git::Diff));
        assert_eq!(tool_of("git log -p -3"), Tool::Git(Git::Diff));
        assert_eq!(tool_of("git log --oneline -5"), Tool::Git(Git::Log));
        assert_eq!(tool_of("cd r && git status --short"), Tool::Git(Git::Status));
        assert_eq!(tool_of("ls -la"), Tool::Other);
    }

    #[test]
    fn cargo_counts_what_went_wrong() {
        let out = "warning: unused import: `FileAccountsStore`\n  --> crates/cli/src/fanout.rs:3:5\nerror[E0425]: cannot find value `x`\nerror: could not compile `pay` (lib) due to 1 previous error\nwarning: `pay` (lib) generated 1 warning";
        assert_eq!(cargo_summary(out).as_deref(), Some("1 error · 1 warning"));
        let tests = "test result: ok. 12 passed; 0 failed; 0 ignored\ntest result: FAILED. 3 passed; 1 failed; 0 ignored";
        assert_eq!(cargo_summary(tests).as_deref(), Some("1 failed · 15 passed"));
        assert_eq!(cargo_summary("   Compiling pay v0.1.0\n    Finished `dev` profile"), None);
    }

    #[test]
    fn cargo_diagnostics_read_as_cargo_prints_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/cli/src")).unwrap();
        std::fs::write(dir.path().join("crates/cli/src/fanout.rs"), "x").unwrap();
        let links = Linker::new(Some(dir.path().to_path_buf()), Editor::Zed);
        let out = "warning: unused import\n  --> crates/cli/src/fanout.rs:3:5\n   |\n 3 | use a::{BackendKind, FileAccountsStore};\n   |                      ^^^^^^^^^^^^^^^^^\n   = note: `#[warn(unused_imports)]` on by default\n    Finished `dev` profile\ntest a ... ok\ntest b ... FAILED\ntest result: FAILED. 1 passed; 1 failed";
        let view = cargo_view(out, 120, &links);
        assert_eq!(plain(&view).lines().count(), 10, "one line each: {}", plain(&view));
        assert!(view.contains("\x1b[1;33mwarning"), "warning heading in bold yellow");
        assert!(view.contains("\x1b[1;33m^^^^"), "markers in the diagnostic's colour");
        assert!(view.contains("\x1b[1;32m    Finished"), "cargo's verbs in bold green");
        assert!(view.contains("\x1b[32mok") && view.contains("\x1b[1;31mFAILED"));
        let url = format!("zed://file{}:3", url_path(&dir.path().join("crates/cli/src/fanout.rs")));
        assert!(view.contains(&format!("\x1b]8;;{url}\x1b\\")), "the location opens the file at its line");
    }

    #[test]
    fn a_git_patch_is_coloured_and_links_to_its_hunks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        let links = Linker::new(Some(dir.path().to_path_buf()), Editor::Zed);
        let out = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,2 +12,3 @@ fn main() {\n     let a = 1;\n-    let b = 2;\n+    let b = 3;";
        let view = git_diff_view(out, 80, &links);
        assert_eq!(plain(&view).lines().map(str::trim_end).collect::<Vec<_>>()[5], "-    let b = 2;");
        assert!(view.contains(&change_bg(Change::Removed)) && view.contains(&change_bg(Change::Added)));
        let file = url_path(&dir.path().join("a.rs"));
        assert!(view.contains(&format!("zed://file{file}:12")), "the hunk opens at its first new line");
        assert!(view.contains(&format!("zed://file{file}\x1b\\")), "the +++ path opens the file");
    }

    #[test]
    fn a_command_without_a_description_is_named_by_what_it_runs() {
        let push = "cd /w && git add a.rs && git commit -q -m \"x && y\" && git add -A rust && git commit -m z && git push -q origin HEAD 2>&1 | tail -3; git log --oneline -4";
        assert_eq!(title_of(push), "git add, commit, push, log");
        assert_eq!(title_of("gh pr view 543 --json body --jq .body > /tmp/543.md && cat >> /tmp/543.md <<'EOF'\n## x\nEOF\ngh pr edit 543"), "gh pr view · cat · gh pr edit");
        assert_eq!(title_of("cd /w && python3 - <<'EOF'\nimport os\nEOF\ngrep -n x a.toml; grep -n y b.rs"), "python3 · grep", "what runs after the heredoc counts too");
        assert_eq!(title_of("cd x && cargo fmt --all && cargo clippy -p pay -- -D warnings 2>&1 | grep -E error | head -30"), "cargo fmt, clippy");
        assert_eq!(title_of("cd x"), "Run a command");
    }

    #[test]
    fn git_log_and_status_are_coloured_like_git() {
        let subjects = git_log_view("119ab8d0 fix(send): wait\nc8ac7a1a feat(cli): from", 80);
        assert!(subjects.contains("\x1b[33m119ab8d0\x1b[0m fix(send)") || !subjects.contains("\x1b[33mf"), "a subject's hex letters aren't the hash: {subjects:?}");
        assert!(!subjects.contains("\x1b[33m fea") && !subjects.contains("\x1b[33mfea"), "{subjects:?}");
        let log = git_log_view("e3cb239 (HEAD -> main, origin/main) Tool call links\n08f0e6c edit", 80);
        assert!(log.contains("\x1b[33me3cb239") && log.contains("\x1b[1;36mHEAD -> main") && log.contains("\x1b[1;31morigin/main"));
        assert_eq!(plain(&log).lines().next().unwrap().trim(), "e3cb239 (HEAD -> main, origin/main) Tool call links");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        let links = Linker::new(Some(dir.path().to_path_buf()), Editor::File);
        let short = git_status_view("M  a.rs\n M b.rs\n?? c.rs", 80, &links);
        assert!(short.contains("\x1b[32mM") && short.contains("\x1b[31m?"));
        assert!(short.contains("file://"), "a path that's there opens");
        let long = git_status_view("Changes to be committed:\n\tmodified:   a.rs\nChanges not staged for commit:\n\tmodified:   b.rs", 80, &links);
        assert!(long.contains("\x1b[32mmodified:") && long.contains("\x1b[31mmodified:"));
    }
}
