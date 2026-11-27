//! How a thread's panes are drawn (RFD 0007): as terminal text, or as web
//! pages drawn by Ladybird.  The rest of Axel asks a [`Display`] for its
//! [`PaneDisplay`] and doesn't care which it got: the same panes, the same
//! layout, the same Rex labels either way.
//!
//! A thread view is three panes, the conversation, the composer and the
//! activity, and Ctrl+D opens a fourth, the controls ([`Pane`]).  A display
//! knows what Rex runs to show each one ([`PaneDisplay::command`]) and how
//! to show it in this process's terminal ([`PaneDisplay::run`]).

use anyhow::{Context, Result};
use axel_i18n::t;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One of the panes a thread shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    /// The conversation.
    Conversation,
    /// Where the owner writes to the thread's agents.
    Composer,
    /// Who is working, and what they spent.
    Activity,
    /// The thread controls (Ctrl+D): its numbers, and what to do to it.
    Controls,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Conversation, Pane::Composer, Pane::Activity, Pane::Controls];

    /// The name on the command line (`--view`) and in the page
    /// (`<body data-view>`).  The conversation's is `messages`, as it was.
    pub fn as_str(self) -> &'static str {
        match self {
            Pane::Conversation => "messages",
            Pane::Composer => "composer",
            Pane::Activity => "activity",
            Pane::Controls => "controls",
        }
    }

    pub fn parse(text: &str) -> Option<Pane> {
        Pane::ALL.into_iter().find(|p| p.as_str() == text.trim())
    }
}

/// `--view`, read on the command line.
pub fn parse_pane(text: &str) -> std::result::Result<Pane, String> {
    Pane::parse(text).ok_or_else(|| t!("a view is messages, composer, activity or controls").to_string())
}

/// How panes are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    /// Terminal text: the default.
    #[default]
    Terminal,
    /// Web pages drawn by Ladybird.  Experimental: off until the owner turns
    /// it on (`axel panes web`), and used only where Ladybird is installed.
    Web,
}

impl Display {
    pub const ALL: [Display; 2] = [Display::Terminal, Display::Web];

    pub fn as_str(self) -> &'static str {
        match self {
            Display::Terminal => "terminal",
            Display::Web => "web",
        }
    }

    pub fn parse(text: &str) -> Option<Display> {
        Display::ALL.into_iter().find(|d| d.as_str() == text.trim())
    }

    /// What the owner chose: `$AXEL_PANES` when set, else the saved choice.
    pub fn saved() -> Display {
        std::env::var("AXEL_PANES").ok().and_then(|v| Display::parse(&v)).unwrap_or_else(|| load_settings().panes)
    }

    /// The display thread views open in: the owner's choice, where it can
    /// draw.  Web without Ladybird is terminal text.
    pub fn chosen() -> Display {
        match Display::saved() {
            Display::Web if super::web::ladybird().is_some() => Display::Web,
            _ => Display::Terminal,
        }
    }

    /// The panes of this display.
    pub fn panes(self) -> &'static dyn PaneDisplay {
        match self {
            Display::Terminal => &TerminalPanes,
            Display::Web => &WebPanes,
        }
    }
}

/// A saved choice this build doesn't know (a newer build's) reads as the
/// default rather than making the whole settings file unreadable.
impl<'de> Deserialize<'de> for Display {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Display, D::Error> {
        Ok(Display::parse(&String::deserialize(d)?).unwrap_or_default())
    }
}

/// A way of drawing a thread's panes.
pub trait PaneDisplay: Sync {
    /// What Rex runs in a new pane to show `pane` of `thread_id`.
    fn command(&self, thread_id: &str, pane: Pane) -> Vec<String>;
    /// Show `pane` of `thread_id` in this process's terminal, until the pane
    /// closes.
    fn run(&self, thread_id: &str, pane: Pane) -> Result<()>;
    /// Rows a composer pane starts with.
    fn composer_rows(&self) -> u16;
}

/// Terminal text: ratatui views and the plain conversation stream.
pub struct TerminalPanes;

impl PaneDisplay for TerminalPanes {
    fn command(&self, thread_id: &str, pane: Pane) -> Vec<String> {
        let args: &[&str] = match pane {
            Pane::Conversation => &["thread", "stream", thread_id],
            Pane::Composer => &["thread", "compose", thread_id],
            Pane::Activity => &["ui", "--screen", "usage", "--thread", thread_id],
            Pane::Controls => &["thread", "manage", thread_id],
        };
        command_of(args)
    }

    fn run(&self, thread_id: &str, pane: Pane) -> Result<()> {
        match pane {
            Pane::Conversation => super::ui::run_stream(thread_id),
            Pane::Composer => super::ui::run_compose(thread_id),
            Pane::Activity => super::ui::run_inline(Some(crate::cli::ScreenArg::Usage), Some(thread_id.to_string())),
            Pane::Controls => super::manage::run(thread_id),
        }
    }

    fn composer_rows(&self) -> u16 {
        // The smallest a pane gets: a line to type on and its borders.
        super::ui::PANE_MIN_ROWS
    }
}

/// Web pages drawn by Ladybird, each falling back to its terminal pane
/// where Ladybird can't draw.
pub struct WebPanes;

impl PaneDisplay for WebPanes {
    fn command(&self, thread_id: &str, pane: Pane) -> Vec<String> {
        command_of(&["web", "pane", thread_id, "--view", pane.as_str()])
    }

    fn run(&self, thread_id: &str, pane: Pane) -> Result<()> {
        super::web::pane(thread_id, pane)
    }

    fn composer_rows(&self) -> u16 {
        // As the terminal composer: the web one is drawn to fit the same rows,
        // so the row of composer and activity is the same size in both.
        TerminalPanes.composer_rows()
    }
}

/// This binary, then `args`.
fn command_of(args: &[&str]) -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "axel".into());
    std::iter::once(exe).chain(args.iter().map(|a| a.to_string())).collect()
}

// ---------------------------------------------------------------- the choice

/// `$AXEL_HOME/settings.json`: what the owner chose here, local to this
/// machine.  Unknown fields from a newer build are kept.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    panes: Display,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

fn settings_path() -> std::path::PathBuf {
    crate::paths::home().join("settings.json")
}

fn load_settings() -> Settings {
    // A missing or unreadable file is the defaults: setting a choice writes it.
    std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// `axel panes [DISPLAY]`: show or set how thread views are drawn.
pub fn panes_cmd(choice: Option<Display>) -> Result<()> {
    let ladybird = super::web::ladybird();
    let Some(choice) = choice else {
        let browser = ladybird.map_or_else(|| t!("not installed").to_string(), |p| p.display().to_string());
        println!("{}", t!("panes: {choice} (Ladybird: {browser})", choice = Display::saved().as_str(), browser = browser));
        return Ok(());
    };
    let mut settings = load_settings();
    settings.panes = choice;
    let path = settings_path();
    let dir = path.parent().context("settings live in a directory")?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), &settings)?;
    tmp.persist(&path)?;
    assert_eq!(load_settings().panes, choice, "the choice reads back as written");
    println!("{}", t!("thread views open as {choice} panes from now on", choice = choice.as_str()));
    if choice == Display::Web && ladybird.is_none() {
        println!("{}", t!("Ladybird isn't installed, so they stay terminal panes until it is: ladybird.org"));
    }
    Ok(())
}

/// `axel panes`'s argument, read on the command line.
pub fn parse_display(text: &str) -> std::result::Result<Display, String> {
    Display::parse(text).ok_or_else(|| t!("panes are web or terminal").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pane_and_display_reads_back_from_its_name() {
        for pane in Pane::ALL {
            assert_eq!(Pane::parse(pane.as_str()), Some(pane));
        }
        for display in Display::ALL {
            assert_eq!(Display::parse(display.as_str()), Some(display));
        }
        assert_eq!(Pane::parse("sideways"), None);
        assert_eq!(Display::parse("auto"), None);
        assert_eq!(serde_json::from_str::<Display>("\"hologram\"").unwrap(), Display::Terminal, "a newer build's choice reads as the default");
    }

    #[test]
    fn each_display_runs_each_pane_through_its_own_command() {
        let terminal = Display::Terminal.panes();
        assert_eq!(terminal.command("T", Pane::Composer)[1..], ["thread", "compose", "T"]);
        assert_eq!(terminal.command("T", Pane::Controls)[1..], ["thread", "manage", "T"]);
        let web = Display::Web.panes();
        assert_eq!(web.command("T", Pane::Conversation)[1..], ["web", "pane", "T", "--view", "messages"]);
        assert_eq!(web.composer_rows(), terminal.composer_rows(), "both lay a thread out the same");
    }
}
