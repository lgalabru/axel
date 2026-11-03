//! Launcher state machine. Pure: it consumes [`KeyEvent`]s and produces
//! [`Action`]s; it never touches the terminal or the store.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::attach::parse_dropped_paths;
use crate::input::TextInput;
use crate::model::{AgentColor, AgentRow, Harness, NewAgentSpec, ProjectRow, RuntimeMode};
use crate::util::{display_path, expand_tilde, fuzzy_score, slugify};

/// The five steps of the launcher form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Title,
    Where,
    Who,
    Workspace,
    Summary,
}

impl Step {
    pub const COUNT: usize = 5;

    pub fn index(self) -> usize {
        match self {
            Step::Title => 0,
            Step::Where => 1,
            Step::Who => 2,
            Step::Workspace => 3,
            Step::Summary => 4,
        }
    }

    pub fn question(self) -> &'static str {
        match self {
            Step::Title => "What are we building?",
            Step::Where => "Where?",
            Step::Who => "Who?",
            Step::Workspace => "Workspace",
            Step::Summary => "Ready",
        }
    }
}

/// Workspace choice (mirrors `axel_store::WorkspaceKind` without depending on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workspace {
    InPlace,
    Worktree,
    /// No projects attached: `~/.axel/scratch/<slug>`.
    Scratch,
}

impl Workspace {
    fn toggle(self) -> Workspace {
        match self {
            Workspace::InPlace => Workspace::Worktree,
            Workspace::Worktree | Workspace::Scratch => Workspace::InPlace,
        }
    }
}

/// What has keyboard focus on the title step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleFocus {
    Text,
    /// Chip selection (only while attachments exist).
    Attachments,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhereMode {
    List,
    Path,
}

/// What a row of the "Where?" list is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhereRow {
    /// Index into `Launcher::projects`.
    Project(usize),
    OtherPath,
    Scratch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhoMode {
    List,
    NewAgent,
}

/// Fields of the new-agent sub-form, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Harness,
    Mode,
    Model,
    Skills,
    Color,
}

impl Field {
    pub const ALL: [Field; 6] = [Field::Name, Field::Harness, Field::Mode, Field::Model, Field::Skills, Field::Color];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Harness => "Harness",
            Field::Mode => "Mode",
            Field::Model => "Model",
            Field::Skills => "Skills",
            Field::Color => "Color",
        }
    }

    fn next(self) -> Field {
        let i = Field::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Field::ALL[(i + 1) % Field::ALL.len()]
    }

    fn prev(self) -> Field {
        let i = Field::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Field::ALL[(i + Field::ALL.len() - 1) % Field::ALL.len()]
    }

    fn is_text(self) -> bool {
        matches!(self, Field::Name | Field::Model | Field::Skills)
    }
}

/// The new-agent sub-form.
#[derive(Clone, Debug)]
pub struct NewAgentForm {
    pub name: TextInput,
    pub harness_idx: usize,
    pub mode_idx: usize,
    pub model: TextInput,
    pub skills: TextInput,
    pub color_idx: usize,
    pub field: Field,
    pub error: Option<String>,
    /// Set while the glue is creating the agent; blocks double submits.
    pub busy: bool,
}

impl NewAgentForm {
    fn new() -> Self {
        Self {
            name: TextInput::new(),
            harness_idx: 0,
            mode_idx: 0,
            model: TextInput::new(),
            skills: TextInput::with_text("*"),
            color_idx: 0,
            field: Field::Name,
            error: None,
            busy: false,
        }
    }

    pub fn color(&self) -> AgentColor {
        AgentColor::ALL[self.color_idx % AgentColor::ALL.len()]
    }

    pub fn runtime_mode(&self) -> RuntimeMode {
        RuntimeMode::ALL[self.mode_idx % RuntimeMode::ALL.len()]
    }
}

/// What the app should do after feeding a key to the launcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    /// Esc on the first step: return to the dashboard.
    Back,
    OpenInbox,
    /// ↓ from the prompt: move to the recent-threads pane below it.
    FocusThreads,
    CreateAgent(NewAgentSpec),
    Launch(LaunchPlan),
}

/// The result of a completed launcher form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub title: String,
    /// 0..n projects; empty = scratch thread.
    pub projects: Vec<PathBuf>,
    pub agent_pubkey: String,
    pub workspace: Workspace,
    pub branch: Option<String>,
    /// `Some` when spawning another agent onto an existing thread.
    pub thread_id: Option<String>,
    pub attachments: Vec<PathBuf>,
}

/// Existing thread the launcher is spawning an agent onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnTarget {
    pub thread_id: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct Launcher {
    /// Its pane grows to fit the prompt (the floating launcher in Rex): the
    /// prompt stays put at the top while it grows, instead of scrolling.
    pub pane_grows: bool,
    pub step: Step,
    pub cwd: PathBuf,
    pub harnesses: Vec<String>,
    /// Set when the launcher was opened to add an agent to an existing thread:
    /// title is locked and the Where/Workspace steps are skipped.
    pub spawn: Option<SpawnTarget>,

    // step 1
    pub title: TextInput,
    /// Files dropped onto the prompt field.
    pub attachments: Vec<PathBuf>,
    pub title_focus: TitleFocus,
    /// Selected chip while `title_focus == Attachments`.
    pub attach_sel: usize,

    // step 2
    pub projects: Vec<ProjectRow>,
    pub filter: TextInput,
    pub where_sel: usize,
    pub where_mode: WhereMode,
    pub path_input: TextInput,
    /// Toggled projects, in toggle order. Empty = scratch thread.
    pub selected_paths: Vec<PathBuf>,

    // step 3
    pub agents: Vec<AgentRow>,
    pub who_sel: usize,
    pub who_mode: WhoMode,
    pub form: NewAgentForm,
    pub chosen_agent: Option<String>,

    // step 4
    pub workspace: Workspace,
    pub branch: TextInput,
    branch_dirty: bool,

    /// Validation message for the current step.
    pub error: Option<String>,
    /// "runtime: …" shown on the Summary step; set by the app from `Deps::runtime_label`.
    pub runtime_label: Option<String>,
}

impl Launcher {
    pub fn new(cwd: PathBuf, projects: Vec<ProjectRow>, agents: Vec<AgentRow>, harnesses: Vec<String>) -> Self {
        let mut l = Self {
            pane_grows: false,
            step: Step::Title,
            cwd,
            harnesses: if harnesses.is_empty() { vec![Harness::default().to_string()] } else { harnesses },
            spawn: None,
            title: TextInput::new(),
            attachments: Vec::new(),
            title_focus: TitleFocus::Text,
            attach_sel: 0,
            projects: Vec::new(),
            filter: TextInput::new(),
            where_sel: 0,
            where_mode: WhereMode::List,
            path_input: TextInput::new(),
            selected_paths: Vec::new(),
            agents,
            who_sel: 0,
            who_mode: WhoMode::List,
            form: NewAgentForm::new(),
            chosen_agent: None,
            workspace: Workspace::InPlace,
            branch: TextInput::new(),
            branch_dirty: false,
            error: None,
            runtime_label: None,
        };
        l.set_projects(projects);
        l.who_sel = if l.agents.is_empty() { 0 } else { 1 };
        l
    }

    /// A launcher that adds another agent to `thread_id`: starts on "Who?",
    /// title locked, no project/workspace questions.
    pub fn for_spawn(cwd: PathBuf, agents: Vec<AgentRow>, harnesses: Vec<String>, thread_id: &str, title: &str) -> Self {
        let mut l = Self::new(cwd, Vec::new(), agents, harnesses);
        l.title = TextInput::with_text(title);
        l.spawn = Some(SpawnTarget { thread_id: thread_id.to_string(), title: title.to_string() });
        l.step = Step::Who;
        l
    }

    /// Replace the project list; the cwd is moved/inserted at the top and the
    /// cursor rests on it. Toggled selections are kept.
    pub fn set_projects(&mut self, mut projects: Vec<ProjectRow>) {
        projects.retain(|p| p.path != self.cwd);
        let name = self
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned());
        projects.insert(0, ProjectRow { name, path: self.cwd.clone() });
        // Keep typed-in paths that are selected but not in the recent list.
        for path in &self.selected_paths {
            if !projects.iter().any(|p| &p.path == path) {
                projects.push(ProjectRow { name: project_name(path), path: path.clone() });
            }
        }
        self.projects = projects;
        self.where_sel = 0;
    }

    /// Replace the agent list, keeping the selection on the same pubkey when possible.
    pub fn set_agents(&mut self, agents: Vec<AgentRow>) {
        let selected_pk = self.selected_agent().map(|a| a.pubkey.clone());
        self.agents = agents;
        self.who_sel = match selected_pk.and_then(|pk| self.agents.iter().position(|a| a.pubkey == pk)) {
            Some(i) => i + 1,
            None if self.agents.is_empty() => 0,
            None => self.who_sel.clamp(usize::from(!self.agents.is_empty()), self.agents.len()),
        };
    }

    /// Called by the app once the glue has created the agent: reload the list,
    /// select the new agent and return to the list.
    pub fn agent_created(&mut self, agents: Vec<AgentRow>, pubkey: &str) {
        self.agents = agents;
        self.who_sel = self.agents.iter().position(|a| a.pubkey == pubkey).map(|i| i + 1).unwrap_or(0);
        self.who_mode = WhoMode::List;
        self.form = NewAgentForm::new();
        self.error = None;
    }

    /// Called by the app when agent creation failed; stays on the form.
    pub fn agent_create_failed(&mut self, msg: impl Into<String>) {
        self.form.busy = false;
        self.form.error = Some(msg.into());
        self.form.field = Field::Name;
    }

    /// Projects matching the current filter, best match first (as indices into `projects`).
    pub fn filtered_projects(&self) -> Vec<usize> {
        let needle = self.filter.text();
        let mut scored: Vec<(u32, usize)> = self
            .projects
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let hay = format!("{} {}", p.name, display_path(&p.path));
                fuzzy_score(&needle, &hay).map(|s| (s, i))
            })
            .collect();
        if !needle.is_empty() {
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        }
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// Rows of the "Where?" list: filtered projects, then "Other path…", then "scratch".
    pub fn where_list(&self) -> Vec<WhereRow> {
        let mut rows: Vec<WhereRow> = self.filtered_projects().into_iter().map(WhereRow::Project).collect();
        rows.push(WhereRow::OtherPath);
        rows.push(WhereRow::Scratch);
        rows
    }

    pub fn where_row(&self) -> WhereRow {
        let rows = self.where_list();
        rows[self.where_sel.min(rows.len() - 1)]
    }

    pub fn is_path_selected(&self, path: &std::path::Path) -> bool {
        self.selected_paths.iter().any(|p| p == path)
    }

    /// Display names of the selected projects, in toggle order.
    pub fn project_names(&self) -> Vec<String> {
        self.selected_paths
            .iter()
            .map(|path| self.projects.iter().find(|p| &p.path == path).map(|p| p.name.clone()).unwrap_or_else(|| project_name(path)))
            .collect()
    }

    /// "a, b" or "scratch (no project)".
    pub fn where_summary(&self) -> String {
        if self.selected_paths.is_empty() { "scratch (no project)".to_string() } else { self.project_names().join(", ") }
    }

    pub fn selected_agent(&self) -> Option<&AgentRow> {
        if self.who_sel == 0 { None } else { self.agents.get(self.who_sel - 1) }
    }

    pub fn chosen_agent_row(&self) -> Option<&AgentRow> {
        let pk = self.chosen_agent.as_deref()?;
        self.agents.iter().find(|a| a.pubkey == pk)
    }

    pub fn harness(&self) -> &str {
        &self.harnesses[self.form.harness_idx % self.harnesses.len()]
    }

    /// Whether the Workspace step applies (projects attached, not spawning).
    fn asks_workspace(&self) -> bool {
        self.spawn.is_none() && !self.selected_paths.is_empty()
    }

    /// Whether `i` currently opens the inbox (no text field has focus).
    pub fn inbox_hotkey_active(&self) -> bool {
        match self.step {
            Step::Who => self.who_mode == WhoMode::List,
            Step::Workspace => self.workspace != Workspace::Worktree,
            Step::Summary => true,
            Step::Title | Step::Where => false,
        }
    }

    /// Feed a pasted string to whichever text field has focus.
    pub fn handle_paste(&mut self, text: &str) {
        match self.step {
            Step::Title => match parse_dropped_paths(text) {
                Some(paths) => self.attach(paths),
                None => self.title.paste(text),
            },
            Step::Where => match self.where_mode {
                WhereMode::List => {
                    self.filter.insert_str(text);
                    self.where_sel = 0;
                }
                WhereMode::Path => self.path_input.insert_str(text),
            },
            Step::Who if self.who_mode == WhoMode::NewAgent => match self.form.field {
                Field::Name => self.form.name.insert_str(text),
                Field::Model => self.form.model.insert_str(text),
                Field::Skills => self.form.skills.insert_str(text),
                Field::Harness | Field::Mode | Field::Color => {}
            },
            Step::Workspace if self.workspace == Workspace::Worktree => {
                self.branch.insert_str(text);
                self.branch_dirty = true;
            }
            _ => {}
        }
    }

    /// Add dropped files as chips (deduplicated).
    pub fn attach(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if !self.attachments.contains(&p) {
                self.attachments.push(p);
            }
        }
        self.error = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match self.step {
            Step::Title => self.key_title(key),
            Step::Where => self.key_where(key),
            Step::Who => self.key_who(key),
            Step::Workspace => self.key_workspace(key),
            Step::Summary => self.key_summary(key),
        }
    }

    // ----- step 1 -------------------------------------------------------------

    /// Remove chip `i`; the selection moves to its neighbour.
    pub fn remove_attachment(&mut self, i: usize) {
        if i < self.attachments.len() {
            self.attachments.remove(i);
        }
        if self.attachments.is_empty() {
            self.attach_sel = 0;
            self.title_focus = TitleFocus::Text;
        } else {
            self.attach_sel = self.attach_sel.min(self.attachments.len() - 1);
        }
    }

    /// Tab: walk the full form (where, who, workspace) instead of launching
    /// with the defaults.
    fn title_next(&mut self) {
        if self.title.text().trim().is_empty() {
            self.error = Some("Give it a one-line title first.".into());
            self.title_focus = TitleFocus::Text;
        } else {
            self.sync_branch();
            self.title_focus = TitleFocus::Text;
            if let Some(i) = self.default_agent_index() {
                self.who_sel = i + 1;
            }
            self.go(Step::Where);
        }
    }

    /// The first agent an `@name` in the prompt names, anywhere in it.
    fn mentioned_agent_index(&self) -> Option<usize> {
        self.mentions().into_iter().next()
    }

    /// Agents named by `@name`s in the prompt, in order (indices into `agents`).
    fn mentions(&self) -> Vec<usize> {
        let is_name = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
        let chars: Vec<char> = self.title.text().chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '@' && (i == 0 || !is_name(chars[i - 1])) {
                let end = (i + 1..chars.len()).find(|&j| !is_name(chars[j])).unwrap_or(chars.len());
                let name: String = chars[i + 1..end].iter().collect();
                if let Some(a) = self.agents.iter().position(|a| !name.is_empty() && a.name.eq_ignore_ascii_case(&name)) {
                    out.push(a);
                }
                i = end.max(i + 1);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Names and colours of the agents, for colouring `@name`s as they're typed.
    pub fn mention_names(&self) -> Vec<(String, Option<AgentColor>)> {
        self.agents.iter().map(|a| (a.name.clone(), a.color)).collect()
    }

    /// Who Enter launches: the agent named by a leading `@name`, else the
    /// one used most recently, else the first.
    pub fn default_agent_index(&self) -> Option<usize> {
        if self.agents.is_empty() {
            return None;
        }
        self.mentioned_agent_index().or_else(|| {
            self.agents
                .iter()
                .enumerate()
                .filter_map(|(i, a)| a.last_seen_at.map(|t| (t, i)))
                .max()
                .map(|(_, i)| i)
                .or(Some(0))
        })
    }

    pub fn default_agent(&self) -> Option<&AgentRow> {
        self.default_agent_index().and_then(|i| self.agents.get(i))
    }

    /// The prompt as sent: a leading `@name` that picked the agent is
    /// dropped (`@nova fix it` → `fix it`); one later in the sentence stays.
    fn prompt_text(&self) -> String {
        let text = self.title.text();
        let trimmed = text.trim();
        if let Some(i) = self.mentioned_agent_index()
            && let Some((first, rest)) = trimmed.split_once(char::is_whitespace)
            && first.strip_prefix('@').is_some_and(|n| n.eq_ignore_ascii_case(&self.agents[i].name))
            && !rest.trim().is_empty()
        {
            return rest.trim().to_string();
        }
        trimmed.to_string()
    }

    /// Enter on the prompt: launch at once with the defaults (the default
    /// agent, in place in the current directory).  With no agent yet, ask
    /// who instead.
    fn quick_launch(&mut self) -> Action {
        self.title_focus = TitleFocus::Text;
        if self.title.text().trim().is_empty() {
            self.error = Some("Type what you want done first.".into());
            return Action::None;
        }
        self.sync_branch();
        let Some(agent) = self.default_agent().map(|a| a.pubkey.clone()) else {
            self.selected_paths = vec![self.cwd.clone()];
            self.who_sel = 0;
            self.go(Step::Who);
            return Action::None;
        };
        Action::Launch(LaunchPlan {
            title: self.prompt_text(),
            projects: vec![self.cwd.clone()],
            agent_pubkey: agent,
            workspace: Workspace::InPlace,
            branch: None,
            thread_id: None,
            attachments: self.attachments.clone(),
        })
    }

    /// What Enter would do, for the line under the prompt: `(agent, place)`.
    pub fn quick_target(&self) -> Option<(&AgentRow, String)> {
        let agent = self.default_agent()?;
        let place = self.projects.first().map(|p| p.name.clone()).unwrap_or_else(|| display_path(&self.cwd));
        Some((agent, place))
    }

    fn key_title(&mut self, key: KeyEvent) -> Action {
        if self.title_focus == TitleFocus::Attachments {
            let n = self.attachments.len();
            match key.code {
                KeyCode::Left | KeyCode::Char('h') => self.attach_sel = self.attach_sel.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => self.attach_sel = (self.attach_sel + 1).min(n.saturating_sub(1)),
                KeyCode::Home => self.attach_sel = 0,
                KeyCode::End => self.attach_sel = n.saturating_sub(1),
                KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('x') => self.remove_attachment(self.attach_sel),
                KeyCode::Esc | KeyCode::BackTab => self.title_focus = TitleFocus::Text,
                KeyCode::Enter => return self.quick_launch(),
                KeyCode::Tab => self.title_next(),
                KeyCode::Char(_) => {
                    self.title_focus = TitleFocus::Text;
                    self.title.handle_key(key);
                    self.error = None;
                }
                _ => {}
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Tab if !self.attachments.is_empty() => {
                self.title_focus = TitleFocus::Attachments;
                self.attach_sel = self.attach_sel.min(self.attachments.len() - 1);
            }
            // Shift+Enter (Option+Enter): a line break, as in the thread composer.
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                self.title.insert_newline();
                self.error = None;
            }
            KeyCode::Enter => return self.quick_launch(),
            KeyCode::Tab => self.title_next(),
            KeyCode::Esc => return Action::Back,
            // ↑/↓ move between the prompt's lines; ↓ on the last goes to the recent threads.
            KeyCode::Up if !key.modifiers.contains(KeyModifiers::SUPER) => {
                self.title.line_up();
            }
            KeyCode::Down if !key.modifiers.contains(KeyModifiers::SUPER) => {
                if !self.title.line_down() {
                    return Action::FocusThreads;
                }
            }
            KeyCode::BackTab => {}
            // Backspace on an empty prompt removes the last attachment chip.
            KeyCode::Backspace if self.title.is_empty() && key.modifiers.is_empty() => {
                let last = self.attachments.len().saturating_sub(1);
                self.remove_attachment(last);
            }
            _ => {
                self.title.handle_key(key);
                self.error = None;
            }
        }
        Action::None
    }

    fn sync_branch(&mut self) {
        if !self.branch_dirty {
            self.branch.set_text(&format!("axel/{}", slugify(&self.title.text())));
        }
    }

    // ----- step 2 -------------------------------------------------------------

    fn key_where(&mut self, key: KeyEvent) -> Action {
        match self.where_mode {
            WhereMode::List => self.key_where_list(key),
            WhereMode::Path => self.key_where_path(key),
        }
    }

    fn toggle_path(&mut self, path: PathBuf) {
        if let Some(i) = self.selected_paths.iter().position(|p| *p == path) {
            self.selected_paths.remove(i);
        } else {
            self.selected_paths.push(path);
        }
        self.error = None;
    }

    fn key_where_list(&mut self, key: KeyEvent) -> Action {
        let rows = self.where_list();
        let n = rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => self.where_sel = self.where_sel.checked_sub(1).unwrap_or(n - 1),
            KeyCode::Down => self.where_sel = (self.where_sel + 1) % n,
            KeyCode::Char('p') if ctrl => self.where_sel = self.where_sel.checked_sub(1).unwrap_or(n - 1),
            KeyCode::Char('n') if ctrl => self.where_sel = (self.where_sel + 1) % n,
            KeyCode::Char(' ') => {
                if let WhereRow::Project(idx) = rows[self.where_sel.min(n - 1)] {
                    let path = self.projects[idx].path.clone();
                    self.toggle_path(path);
                }
            }
            KeyCode::Enter | KeyCode::Tab => match rows[self.where_sel.min(n - 1)] {
                WhereRow::Project(idx) => {
                    if self.selected_paths.is_empty() {
                        // Fast path: Enter on a project with nothing toggled picks just that one.
                        self.selected_paths.push(self.projects[idx].path.clone());
                    }
                    self.go(Step::Who);
                }
                WhereRow::OtherPath => {
                    let seed = if self.filter.is_empty() { display_path(&self.cwd) } else { self.filter.text() };
                    self.path_input = TextInput::with_text(&seed);
                    self.where_mode = WhereMode::Path;
                    self.error = None;
                }
                WhereRow::Scratch => {
                    self.selected_paths.clear();
                    self.go(Step::Who);
                }
            },
            KeyCode::Esc | KeyCode::BackTab => self.go(Step::Title),
            _ => {
                if self.filter.handle_key(key) {
                    self.where_sel = 0;
                    self.error = None;
                }
            }
        }
        Action::None
    }

    fn key_where_path(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter | KeyCode::Tab => {
                let text = self.path_input.text();
                let text = text.trim();
                if text.is_empty() {
                    self.error = Some("Type a directory path.".into());
                } else {
                    let path = expand_tilde(text);
                    let path = if path.is_absolute() { path } else { self.cwd.join(path) };
                    let idx = match self.projects.iter().position(|p| p.path == path) {
                        Some(i) => i,
                        None => {
                            self.projects.push(ProjectRow { name: project_name(&path), path: path.clone() });
                            self.projects.len() - 1
                        }
                    };
                    if !self.is_path_selected(&path) {
                        self.selected_paths.push(path);
                    }
                    // Back to the list with the new project under the cursor, so more can be added.
                    self.filter.clear();
                    self.where_mode = WhereMode::List;
                    self.where_sel = self.filtered_projects().iter().position(|i| *i == idx).unwrap_or(0);
                    self.error = None;
                }
            }
            KeyCode::Esc | KeyCode::BackTab => {
                self.where_mode = WhereMode::List;
                self.error = None;
            }
            _ => {
                self.path_input.handle_key(key);
                self.error = None;
            }
        }
        Action::None
    }

    // ----- step 3 -------------------------------------------------------------

    fn key_who(&mut self, key: KeyEvent) -> Action {
        match self.who_mode {
            WhoMode::List => self.key_who_list(key),
            WhoMode::NewAgent => self.key_who_form(key),
        }
    }

    fn key_who_list(&mut self, key: KeyEvent) -> Action {
        let rows = self.agents.len() + 1;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.who_sel = self.who_sel.checked_sub(1).unwrap_or(rows - 1),
            KeyCode::Down | KeyCode::Char('j') => self.who_sel = (self.who_sel + 1) % rows,
            KeyCode::Char('p') if ctrl => self.who_sel = self.who_sel.checked_sub(1).unwrap_or(rows - 1),
            KeyCode::Char('n') if ctrl => self.who_sel = (self.who_sel + 1) % rows,
            KeyCode::Char('n') => self.open_new_agent_form(),
            KeyCode::Char('i') => return Action::OpenInbox,
            KeyCode::Enter | KeyCode::Tab => {
                if self.who_sel == 0 {
                    self.open_new_agent_form();
                } else if let Some(agent) = self.agents.get(self.who_sel - 1) {
                    self.chosen_agent = Some(agent.pubkey.clone());
                    if self.asks_workspace() {
                        self.go(Step::Workspace);
                    } else {
                        self.go(Step::Summary);
                    }
                }
            }
            KeyCode::Esc | KeyCode::BackTab => {
                if self.spawn.is_some() {
                    return Action::Back;
                }
                self.go(Step::Where);
            }
            _ => {}
        }
        Action::None
    }

    fn open_new_agent_form(&mut self) {
        self.form = NewAgentForm::new();
        self.form.color_idx = self.agents.len() % AgentColor::ALL.len();
        self.who_mode = WhoMode::NewAgent;
        self.error = None;
    }

    fn key_who_form(&mut self, key: KeyEvent) -> Action {
        if self.form.busy {
            return Action::None;
        }
        let field = self.form.field;
        match key.code {
            KeyCode::Esc => {
                self.who_mode = WhoMode::List;
                self.form.error = None;
            }
            KeyCode::Tab | KeyCode::Down => self.form.field = field.next(),
            KeyCode::BackTab | KeyCode::Up => self.form.field = field.prev(),
            KeyCode::Enter => {
                if field == Field::Color {
                    return self.confirm_new_agent();
                }
                self.form.field = field.next();
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if !field.is_text() => {
                let back = key.code == KeyCode::Left;
                let cycle = |idx: usize, n: usize| if back { (idx + n - 1) % n } else { (idx + 1) % n };
                match field {
                    Field::Harness => self.form.harness_idx = cycle(self.form.harness_idx, self.harnesses.len()),
                    Field::Color => self.form.color_idx = cycle(self.form.color_idx, AgentColor::ALL.len()),
                    Field::Mode => self.form.mode_idx = cycle(self.form.mode_idx, RuntimeMode::ALL.len()),
                    _ => {}
                }
            }
            _ => {
                let consumed = match field {
                    Field::Name => self.form.name.handle_key(key),
                    Field::Model => self.form.model.handle_key(key),
                    Field::Skills => self.form.skills.handle_key(key),
                    Field::Harness | Field::Mode | Field::Color => false,
                };
                if consumed {
                    self.form.error = None;
                }
            }
        }
        Action::None
    }

    fn confirm_new_agent(&mut self) -> Action {
        let name = self.form.name.text().trim().to_string();
        if name.is_empty() {
            self.form.error = Some("Name is required.".into());
            self.form.field = Field::Name;
            return Action::None;
        }
        if name.contains(char::is_whitespace) {
            self.form.error = Some("Name cannot contain spaces (it is used as @name).".into());
            self.form.field = Field::Name;
            return Action::None;
        }
        if self.agents.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) {
            self.form.error = Some(format!("An agent named \"{name}\" already exists."));
            self.form.field = Field::Name;
            return Action::None;
        }
        let model = self.form.model.text();
        let model = model.trim();
        let mut skills: Vec<String> =
            self.form.skills.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if skills.is_empty() {
            skills.push("*".to_string());
        }
        self.form.busy = true;
        self.form.error = None;
        Action::CreateAgent(NewAgentSpec {
            name,
            harness: Harness::parse(self.harness()),
            model: if model.is_empty() { None } else { Some(model.to_string()) },
            skills,
            color: Some(self.form.color()),
            runtime_mode: self.form.runtime_mode(),
        })
    }

    // ----- step 4 -------------------------------------------------------------

    fn key_workspace(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
                self.workspace = self.workspace.toggle();
                self.error = None;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if self.workspace == Workspace::Worktree && self.branch.text().trim().is_empty() {
                    self.error = Some("Branch name is required for a worktree.".into());
                } else {
                    self.go(Step::Summary);
                }
            }
            KeyCode::Esc | KeyCode::BackTab => {
                self.who_mode = WhoMode::List;
                self.go(Step::Who);
            }
            KeyCode::Char('i') if self.workspace != Workspace::Worktree => return Action::OpenInbox,
            _ if self.workspace == Workspace::Worktree && self.branch.handle_key(key) => {
                self.branch_dirty = true;
                self.error = None;
            }
            _ => {}
        }
        Action::None
    }

    // ----- step 5 -------------------------------------------------------------

    fn key_summary(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => return Action::Launch(self.plan()),
            KeyCode::Esc | KeyCode::BackTab => {
                if self.asks_workspace() {
                    self.go(Step::Workspace);
                } else {
                    self.who_mode = WhoMode::List;
                    self.go(Step::Who);
                }
            }
            KeyCode::Char('i') => return Action::OpenInbox,
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    /// Effective workspace for the plan.
    pub fn effective_workspace(&self) -> Workspace {
        if self.spawn.is_some() {
            Workspace::InPlace
        } else if self.selected_paths.is_empty() {
            Workspace::Scratch
        } else {
            self.workspace
        }
    }

    fn plan(&self) -> LaunchPlan {
        let workspace = self.effective_workspace();
        let branch = match workspace {
            Workspace::Worktree => Some(self.branch.text().trim().to_string()),
            Workspace::InPlace | Workspace::Scratch => None,
        };
        LaunchPlan {
            title: self.title.text().trim().to_string(),
            projects: self.selected_paths.clone(),
            agent_pubkey: self.chosen_agent.clone().unwrap_or_default(),
            workspace,
            branch,
            thread_id: self.spawn.as_ref().map(|s| s.thread_id.clone()),
            attachments: self.attachments.clone(),
        }
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        self.error = None;
    }

    /// Human description of the workspace for breadcrumbs and the summary.
    pub fn workspace_summary(&self) -> String {
        match self.effective_workspace() {
            Workspace::InPlace => "in place".to_string(),
            Workspace::Worktree => format!("worktree on {}", self.branch.text().trim()),
            Workspace::Scratch => "scratch".to_string(),
        }
    }

    /// Steps already answered, as (label, value) pairs for the breadcrumb.
    pub fn breadcrumbs(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(sp) = &self.spawn {
            out.push(("Thread", sp.title.clone()));
        } else {
            if self.step > Step::Title {
                out.push(("Building", self.title.text().trim().to_string()));
            }
            if self.step > Step::Where {
                out.push(("Where", self.where_summary()));
            }
        }
        if self.step > Step::Who {
            let who = self.chosen_agent_row().map(|a| format!("{} · {}", a.name, a.harness)).unwrap_or_default();
            out.push(("Who", who));
        }
        if self.step > Step::Workspace && self.asks_workspace() {
            out.push(("Workspace", self.workspace_summary()));
        }
        out
    }

    /// Step-specific key hints for the footer.
    /// One word for the top line: which step we're on.
    pub fn step_title(&self) -> &'static str {
        match self.step {
            Step::Title => "new thread",
            Step::Where => "where",
            Step::Who => "who",
            Step::Workspace => "workspace",
            Step::Summary => "launch",
        }
    }

    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        match self.step {
            Step::Title => match self.title_focus {
                TitleFocus::Attachments => vec![("←→", "select"), ("⌫", "remove"), ("Esc", "back")],
                TitleFocus::Text if !self.attachments.is_empty() => {
                    vec![("Enter", "launch"), ("Tab", "attachments"), ("↓", "threads"), ("Esc", "close")]
                }
                TitleFocus::Text => vec![("Enter", "launch"), ("Tab", "options"), ("↓", "threads"), ("Esc", "close")],
            },
            Step::Where => match self.where_mode {
                WhereMode::List => vec![("↑↓", "move"), ("Space", "toggle"), ("Enter", "confirm"), ("type", "filter"), ("Esc", "back")],
                WhereMode::Path => vec![("Enter", "add path"), ("Esc", "back")],
            },
            Step::Who => match self.who_mode {
                WhoMode::List => {
                    let back = if self.spawn.is_some() { "dashboard" } else { "back" };
                    vec![("↑↓", "pick"), ("n", "new agent"), ("Enter", "next"), ("i", "inbox"), ("Esc", back)]
                }
                WhoMode::NewAgent => {
                    if self.form.field.is_text() {
                        vec![("Tab", "next field"), ("Enter", "next"), ("Esc", "cancel")]
                    } else if self.form.field == Field::Color {
                        vec![("←→", "cycle"), ("Enter", "create"), ("Esc", "cancel")]
                    } else {
                        vec![("←→", "cycle"), ("Tab", "next field"), ("Esc", "cancel")]
                    }
                }
            },
            Step::Workspace => {
                let mut v = vec![("↑↓", "toggle"), ("Enter", "next")];
                if self.workspace != Workspace::Worktree {
                    v.push(("i", "inbox"));
                }
                v.push(("Esc", "back"));
                v
            }
            Step::Summary => vec![("Enter", "launch"), ("i", "inbox"), ("Esc", "back"), ("q", "quit")],
        }
    }
}

fn project_name(path: &std::path::Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn type_str(l: &mut Launcher, s: &str) {
        for c in s.chars() {
            assert_eq!(l.handle_key(key(KeyCode::Char(c))), Action::None);
        }
    }
    fn enter(l: &mut Launcher) -> Action {
        l.handle_key(key(KeyCode::Enter))
    }
    fn esc(l: &mut Launcher) -> Action {
        l.handle_key(key(KeyCode::Esc))
    }
    fn space(l: &mut Launcher) -> Action {
        l.handle_key(key(KeyCode::Char(' ')))
    }
    fn down(l: &mut Launcher) {
        l.handle_key(key(KeyCode::Down));
    }
    fn up(l: &mut Launcher) {
        l.handle_key(key(KeyCode::Up));
    }

    fn agent(name: &str, pk: &str) -> AgentRow {
        AgentRow {
            pubkey: pk.into(),
            name: name.into(),
            harness: Harness::Claude,
            model: None,
            color: Some(AgentColor::Blue),
            runtime_mode: RuntimeMode::Autopilot,
            last_seen_at: None,
            unread: 0,
        }
    }

    fn launcher() -> Launcher {
        Launcher::new(
            PathBuf::from("/work/axel"),
            vec![
                ProjectRow { name: "other".into(), path: PathBuf::from("/work/other") },
                ProjectRow { name: "axel".into(), path: PathBuf::from("/work/axel") },
                ProjectRow { name: "zebra".into(), path: PathBuf::from("/tmp/zebra") },
            ],
            vec![agent("atlas", "PK_ATLAS"), agent("nova", "PK_NOVA")],
            vec!["claude".into(), "codex".into(), "opencode".into()],
        )
    }

    /// Title typed and confirmed; on the Where step.
    fn at_where(title: &str) -> Launcher {
        let mut l = launcher();
        type_str(&mut l, title);
        assert_eq!(l.handle_key(key(KeyCode::Tab)), Action::None);
        assert_eq!(l.step, Step::Where);
        l
    }

    #[test]
    fn enter_on_the_prompt_launches_with_the_defaults() {
        let mut l = launcher();
        type_str(&mut l, "Fix the login bug");
        assert_eq!(
            enter(&mut l),
            Action::Launch(LaunchPlan {
                title: "Fix the login bug".into(),
                projects: vec![PathBuf::from("/work/axel")],
                agent_pubkey: "PK_ATLAS".into(),
                workspace: Workspace::InPlace,
                branch: None,
                thread_id: None,
                attachments: vec![],
            }),
            "no agent used yet: the first one, in place in the cwd"
        );
        let mut l = launcher();
        l.agents[1].last_seen_at = Some(100);
        type_str(&mut l, "x");
        assert!(matches!(enter(&mut l), Action::Launch(p) if p.agent_pubkey == "PK_NOVA"), "the agent used last");
    }

    #[test]
    fn a_leading_mention_picks_the_agent_and_is_dropped_from_the_prompt() {
        let mut l = launcher();
        type_str(&mut l, "@Nova fix the tests");
        assert_eq!(l.quick_target().map(|(a, _)| a.name.as_str()), Some("nova"));
        match enter(&mut l) {
            Action::Launch(p) => {
                assert_eq!(p.agent_pubkey, "PK_NOVA");
                assert_eq!(p.title, "fix the tests");
            }
            other => panic!("expected launch, got {other:?}"),
        }
        let mut l = launcher();
        type_str(&mut l, "test @nova");
        match enter(&mut l) {
            Action::Launch(p) => assert_eq!((p.agent_pubkey.as_str(), p.title.as_str()), ("PK_NOVA", "test @nova"), "later in the sentence: picked, kept"),
            other => panic!("expected launch, got {other:?}"),
        }
        let mut l = launcher();
        type_str(&mut l, "@nobody fix it");
        assert!(matches!(enter(&mut l), Action::Launch(p) if p.agent_pubkey == "PK_ATLAS" && p.title == "@nobody fix it"));
    }

    #[test]
    fn with_no_agent_enter_asks_who_and_down_moves_to_the_threads() {
        let mut l = Launcher::new(PathBuf::from("/work/axel"), vec![], vec![], vec![]);
        assert_eq!(l.handle_key(key(KeyCode::Down)), Action::FocusThreads);
        type_str(&mut l, "Build it");
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.selected_paths, vec![PathBuf::from("/work/axel")]);
    }

    #[test]
    fn cwd_is_first_and_preselected() {
        let l = launcher();
        assert_eq!(l.projects.len(), 3, "cwd deduped, not duplicated");
        assert_eq!(l.projects[0].path, PathBuf::from("/work/axel"));
        assert_eq!(l.where_sel, 0);
        assert!(l.selected_paths.is_empty(), "nothing toggled until the user acts");
        assert_eq!(l.where_list().len(), 5, "3 projects + other + scratch");
    }

    #[test]
    fn cwd_inserted_when_absent() {
        let l = Launcher::new(PathBuf::from("/somewhere/new"), vec![], vec![], vec![]);
        assert_eq!(l.projects.len(), 1);
        assert_eq!(l.projects[0].name, "new");
        assert_eq!(l.harnesses, vec!["claude".to_string()], "empty harness list falls back to claude");
    }

    #[test]
    fn empty_title_blocks_advance_and_esc_goes_back_to_dashboard() {
        let mut l = launcher();
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Title);
        assert!(l.error.is_some());
        assert_eq!(esc(&mut l), Action::Back);
    }

    #[test]
    fn ctrl_c_quits_anywhere() {
        let mut l = at_where("x");
        assert_eq!(l.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::Quit);
    }

    #[test]
    fn full_happy_path_single_project_in_place() {
        let mut l = at_where("Fix the login bug");
        // Enter on the preselected cwd with nothing toggled picks just it.
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.selected_paths, vec![PathBuf::from("/work/axel")]);
        assert_eq!(l.who_sel, 1, "first agent preselected; row 0 is + New agent");
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Workspace);
        assert_eq!(l.chosen_agent.as_deref(), Some("PK_ATLAS"));
        assert_eq!(l.workspace, Workspace::InPlace);
        assert_eq!(l.branch.text(), "axel/fix-the-login-bug");
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Summary);
        assert_eq!(l.breadcrumbs().len(), 4);
        assert_eq!(
            enter(&mut l),
            Action::Launch(LaunchPlan {
                title: "Fix the login bug".into(),
                projects: vec![PathBuf::from("/work/axel")],
                agent_pubkey: "PK_ATLAS".into(),
                workspace: Workspace::InPlace,
                branch: None,
                thread_id: None,
                attachments: vec![],
            })
        );
    }

    #[test]
    fn a_pasted_directory_is_text_not_an_attachment() {
        let dir = std::env::temp_dir();
        let mut l = launcher();
        l.handle_paste(dir.to_str().unwrap());
        assert!(l.attachments.is_empty(), "no chip for a directory");
        assert_eq!(l.title.text(), dir.to_str().unwrap(), "the path is in the prompt, as pasted");
    }

    #[test]
    fn option_arrows_move_by_word_in_the_prompt() {
        let mut l = launcher();
        for c in "fix the login".chars() {
            l.handle_key(key(KeyCode::Char(c)));
        }
        l.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(l.title.cursor(), 8, "Option+← to the start of the word");
        l.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(l.title.cursor(), 4);
        l.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(l.title.cursor(), 7, "Option+→ to the end of the word");
    }

    #[test]
    fn the_prompt_takes_several_lines() {
        let mut l = launcher();
        for c in "fix it".chars() {
            l.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(l.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)), Action::None, "Shift+Enter doesn't launch");
        for c in "it fails".chars() {
            l.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(l.title.text(), "fix it\nit fails");
        assert_eq!(l.handle_key(key(KeyCode::Up)), Action::None, "↑ to the first line");
        assert_eq!(l.title.cursor(), 6);
        assert_eq!(l.handle_key(key(KeyCode::Down)), Action::None, "↓ back to the second");
        assert_eq!(l.handle_key(key(KeyCode::Down)), Action::FocusThreads, "↓ on the last line: the recent threads");
        l.title.clear();
        l.handle_paste("first\nsecond");
        assert_eq!(l.title.text(), "first\nsecond", "a pasted text keeps its lines");
        assert_eq!(crate::util::title_line(&l.title.text()), "first", "the thread is named after its first line");
    }

    #[test]
    fn attachments_chips_backspace_and_plan() {
        let dir = std::env::temp_dir().join(format!("axel-tui-launcher-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("spec.md");
        let f2 = dir.join("shot.png");
        std::fs::write(&f1, b"x").unwrap();
        std::fs::write(&f2, b"x").unwrap();
        let mut l = launcher();
        // a dropped path becomes a chip, not text
        l.handle_paste(&format!("{}\n{}\n", f1.display(), f2.display()));
        assert!(l.title.is_empty());
        assert_eq!(l.attachments.len(), 2);
        // dropping the same file again does not duplicate it
        l.handle_paste(f1.to_str().unwrap());
        assert_eq!(l.attachments.len(), 2);
        // ordinary pastes are still text
        l.handle_paste("fix it");
        assert_eq!(l.title.text(), "fix it");
        // Backspace with text edits text; Ctrl+U clears text only; Backspace on empty pops a chip
        l.handle_key(key(KeyCode::Backspace));
        assert_eq!(l.title.text(), "fix i");
        assert_eq!(l.attachments.len(), 2);
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(l.title.is_empty());
        assert_eq!(l.attachments.len(), 2, "Ctrl+U keeps chips");
        l.handle_key(key(KeyCode::Backspace));
        assert_eq!(l.attachments.len(), 1);
        assert!(l.attachments[0].ends_with("spec.md"));
        // Tab from text goes to the chips; Tab from the chips advances the step.
        l.handle_paste(f2.to_str().unwrap());
        assert_eq!(l.attachments.len(), 2);
        l.handle_key(key(KeyCode::Tab));
        assert_eq!(l.title_focus, TitleFocus::Attachments);
        assert_eq!(l.step, Step::Title);
        l.handle_key(key(KeyCode::Right));
        l.handle_key(key(KeyCode::Right));
        assert_eq!(l.attach_sel, 1, "clamped");
        l.handle_key(key(KeyCode::Left));
        l.handle_key(key(KeyCode::Char('x')));
        assert_eq!(l.attachments.len(), 1);
        assert!(l.attachments[0].ends_with("shot.png"));
        assert_eq!(l.attach_sel, 0);
        assert_eq!(l.title_focus, TitleFocus::Attachments);
        assert_eq!(esc(&mut l), Action::None, "Esc leaves the chips, does not go back");
        assert_eq!(l.title_focus, TitleFocus::Text);
        l.handle_key(key(KeyCode::Tab));
        assert_eq!(l.handle_key(key(KeyCode::Tab)), Action::None);
        assert_eq!(l.step, Step::Title, "empty title blocks the advance");
        assert!(l.error.is_some());
        assert_eq!(l.title_focus, TitleFocus::Text);
        type_str(&mut l, "R");
        l.handle_key(key(KeyCode::Tab));
        assert_eq!(l.title_focus, TitleFocus::Attachments);
        // typing from the chips returns to the text and types the char
        type_str(&mut l, "e");
        assert_eq!(l.title_focus, TitleFocus::Text);
        assert_eq!(l.title.text(), "Re");
        l.handle_key(key(KeyCode::Tab));
        l.handle_key(key(KeyCode::Tab));
        assert_eq!(l.step, Step::Where, "Tab from the chips advances");
        esc(&mut l);
        l.title.clear();
        // the plan carries the remaining attachment
        type_str(&mut l, "Review the spec");
        for _ in 0..4 {
            enter(&mut l);
        }
        match enter(&mut l) {
            Action::Launch(p) => assert_eq!(p.attachments, l.attachments),
            other => panic!("expected launch, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn where_multi_select_with_space() {
        let mut l = at_where("Cross-repo change");
        assert_eq!(space(&mut l), Action::None); // toggle cwd (/work/axel)
        down(&mut l);
        space(&mut l); // toggle /work/other
        down(&mut l);
        space(&mut l); // toggle /tmp/zebra
        space(&mut l); // untoggle /tmp/zebra
        assert_eq!(l.selected_paths, vec![PathBuf::from("/work/axel"), PathBuf::from("/work/other")]);
        assert!(l.is_path_selected(&PathBuf::from("/work/other")));
        assert!(!l.is_path_selected(&PathBuf::from("/tmp/zebra")));
        // Enter confirms the toggled set regardless of where the cursor is.
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.where_summary(), "axel, other");
        enter(&mut l);
        assert_eq!(l.step, Step::Workspace, "projects attached -> workspace asked");
        enter(&mut l);
        match enter(&mut l) {
            Action::Launch(p) => {
                assert_eq!(p.projects, vec![PathBuf::from("/work/axel"), PathBuf::from("/work/other")]);
                assert_eq!(p.workspace, Workspace::InPlace);
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn where_space_toggles_survive_filtering_and_space_does_not_filter() {
        let mut l = at_where("t");
        space(&mut l); // cwd toggled
        type_str(&mut l, "zeb");
        assert_eq!(l.filter.text(), "zeb", "space never reached the filter");
        assert_eq!(l.filtered_projects().len(), 1);
        space(&mut l); // zebra toggled while filtered
        assert_eq!(l.selected_paths.len(), 2);
        // Space on non-project rows does nothing.
        down(&mut l);
        assert_eq!(l.where_row(), WhereRow::OtherPath);
        space(&mut l);
        down(&mut l);
        assert_eq!(l.where_row(), WhereRow::Scratch);
        space(&mut l);
        assert_eq!(l.selected_paths.len(), 2);
    }

    #[test]
    fn scratch_thread_skips_workspace() {
        let mut l = at_where("Think about caching");
        up(&mut l); // wraps to the last row = scratch
        assert_eq!(l.where_row(), WhereRow::Scratch);
        space(&mut l); // toggle something first to prove scratch clears it
        assert_eq!(enter(&mut l), Action::None);
        assert!(l.selected_paths.is_empty());
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.where_summary(), "scratch (no project)");
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Summary, "no projects -> no workspace question");
        assert_eq!(l.workspace_summary(), "scratch");
        assert!(l.breadcrumbs().iter().any(|(k, v)| *k == "Where" && v == "scratch (no project)"));
        // Esc from summary goes back to Who, not Workspace.
        assert_eq!(esc(&mut l), Action::None);
        assert_eq!(l.step, Step::Who);
        enter(&mut l);
        match enter(&mut l) {
            Action::Launch(p) => {
                assert!(p.projects.is_empty());
                assert_eq!(p.workspace, Workspace::Scratch);
                assert_eq!(p.branch, None);
                assert_eq!(p.thread_id, None);
                assert!(p.attachments.is_empty());
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn worktree_requires_branch_and_keeps_user_edit() {
        let mut l = at_where("Add OAuth");
        enter(&mut l);
        enter(&mut l);
        assert_eq!(l.step, Step::Workspace);
        down(&mut l);
        assert_eq!(l.workspace, Workspace::Worktree);
        assert_eq!(l.branch.text(), "axel/add-oauth");
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(enter(&mut l), Action::None);
        assert!(l.error.is_some());
        type_str(&mut l, "feat/oauth");
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Summary);
        for _ in 0..4 {
            esc(&mut l);
        }
        assert_eq!(l.step, Step::Title);
        type_str(&mut l, " v2");
        l.handle_key(key(KeyCode::Tab));
        assert_eq!(l.branch.text(), "feat/oauth", "user-edited branch survives a title change");
        assert_eq!(l.selected_paths.len(), 1, "selection survives going back");
        enter(&mut l);
        enter(&mut l);
        enter(&mut l);
        match enter(&mut l) {
            Action::Launch(p) => {
                assert_eq!(p.workspace, Workspace::Worktree);
                assert_eq!(p.branch.as_deref(), Some("feat/oauth"));
                assert_eq!(p.title, "Add OAuth v2");
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn where_filter_and_other_path_adds_to_selection() {
        let mut l = at_where("t");
        type_str(&mut l, "zeb");
        let f = l.filtered_projects();
        assert_eq!(f.len(), 1);
        assert_eq!(l.projects[f[0]].name, "zebra");
        down(&mut l);
        assert_eq!(l.where_row(), WhereRow::OtherPath);
        enter(&mut l);
        assert_eq!(l.where_mode, WhereMode::Path);
        assert_eq!(l.path_input.text(), "zeb", "non-empty filter seeds the path");
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut l, "/srv/app");
        enter(&mut l);
        // Back on the list, filter cleared, new project present, selected, under the cursor.
        assert_eq!(l.step, Step::Where);
        assert_eq!(l.where_mode, WhereMode::List);
        assert!(l.filter.is_empty());
        assert_eq!(l.selected_paths, vec![PathBuf::from("/srv/app")]);
        assert_eq!(l.where_row(), WhereRow::Project(3));
        assert_eq!(l.projects[3].name, "app");
        // Toggle cwd too and confirm.
        up(&mut l);
        up(&mut l);
        up(&mut l);
        assert_eq!(l.where_row(), WhereRow::Project(0));
        space(&mut l);
        enter(&mut l);
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.project_names(), vec!["app".to_string(), "axel".to_string()]);
        assert!(l.breadcrumbs().iter().any(|(k, v)| *k == "Where" && v == "app, axel"));
    }

    #[test]
    fn where_relative_path_is_joined_to_cwd_and_esc_leaves_path_mode() {
        let mut l = at_where("t");
        up(&mut l);
        up(&mut l); // last-but-one row = Other path…
        assert_eq!(l.where_row(), WhereRow::OtherPath);
        enter(&mut l);
        assert_eq!(l.where_mode, WhereMode::Path);
        assert_eq!(esc(&mut l), Action::None);
        assert_eq!(l.where_mode, WhereMode::List);
        assert_eq!(l.step, Step::Where);
        assert_eq!(l.where_row(), WhereRow::OtherPath, "cursor stays on Other path… after cancelling");
        enter(&mut l);
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut l, "sub/dir");
        enter(&mut l);
        assert_eq!(l.selected_paths, vec![PathBuf::from("/work/axel/sub/dir")]);
        assert_eq!(l.where_row(), WhereRow::Project(3), "cursor on the newly added project");
        // Adding the same path twice does not duplicate it.
        down(&mut l);
        assert_eq!(l.where_row(), WhereRow::OtherPath);
        enter(&mut l);
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut l, "sub/dir");
        enter(&mut l);
        assert_eq!(l.selected_paths.len(), 1);
        assert_eq!(l.projects.len(), 4);
    }

    #[test]
    fn set_projects_keeps_typed_paths_that_are_selected() {
        let mut l = at_where("t");
        up(&mut l);
        up(&mut l);
        enter(&mut l);
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut l, "/srv/app");
        enter(&mut l);
        l.set_projects(vec![ProjectRow { name: "other".into(), path: PathBuf::from("/work/other") }]);
        assert!(l.projects.iter().any(|p| p.path.as_path() == std::path::Path::new("/srv/app")));
        assert_eq!(l.selected_paths, vec![PathBuf::from("/srv/app")]);
    }

    #[test]
    fn who_inbox_hotkey_and_back() {
        let mut l = at_where("t");
        enter(&mut l);
        assert_eq!(l.step, Step::Who);
        assert!(l.inbox_hotkey_active());
        assert_eq!(l.handle_key(key(KeyCode::Char('i'))), Action::OpenInbox);
        assert_eq!(l.handle_key(key(KeyCode::BackTab)), Action::None);
        assert_eq!(l.step, Step::Where);
    }

    #[test]
    fn spawn_mode_starts_at_who_and_skips_projects_and_workspace() {
        let agents = vec![agent("atlas", "PK_ATLAS"), agent("nova", "PK_NOVA")];
        let mut l = Launcher::for_spawn(PathBuf::from("/work/axel"), agents, vec!["claude".into()], "T_123", "Fix login");
        assert_eq!(l.step, Step::Who);
        assert_eq!(l.breadcrumbs(), vec![("Thread", "Fix login".to_string())]);
        assert!(l.help().iter().any(|(k, v)| *k == "Esc" && *v == "dashboard"));
        down(&mut l); // nova
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Summary, "workspace step skipped when spawning");
        assert_eq!(l.workspace_summary(), "in place");
        assert_eq!(
            enter(&mut l),
            Action::Launch(LaunchPlan {
                title: "Fix login".into(),
                projects: vec![],
                agent_pubkey: "PK_NOVA".into(),
                workspace: Workspace::InPlace,
                branch: None,
                thread_id: Some("T_123".into()),
                attachments: vec![],
            })
        );
        // Esc from Who in spawn mode returns to the dashboard, never to Where/Title.
        esc(&mut l);
        assert_eq!(l.step, Step::Who);
        assert_eq!(esc(&mut l), Action::Back);
        // New-agent form still works while spawning.
        l.handle_key(key(KeyCode::Char('n')));
        assert_eq!(l.who_mode, WhoMode::NewAgent);
    }

    #[test]
    fn new_agent_form_flow() {
        let mut l = at_where("t");
        enter(&mut l);
        up(&mut l); // from row 1 to row 0 = "+ New agent"
        assert_eq!(l.who_sel, 0);
        enter(&mut l);
        assert_eq!(l.who_mode, WhoMode::NewAgent);
        assert_eq!(l.form.field, Field::Name);
        assert_eq!(l.form.skills.text(), "*");

        type_str(&mut l, "Atlas");
        for _ in 0..5 {
            l.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(l.form.field, Field::Color);
        assert_eq!(enter(&mut l), Action::None);
        assert!(l.form.error.as_deref().unwrap().contains("already exists"));
        assert_eq!(l.form.field, Field::Name);

        l.form.name.clear();
        type_str(&mut l, "scout");
        enter(&mut l); // -> Harness
        assert_eq!(l.form.field, Field::Harness);
        l.handle_key(key(KeyCode::Right));
        assert_eq!(l.harness(), "codex");
        l.handle_key(key(KeyCode::Left));
        l.handle_key(key(KeyCode::Left));
        assert_eq!(l.harness(), "opencode", "cycling wraps");
        enter(&mut l); // -> Mode
        assert_eq!(l.form.field, Field::Mode);
        assert_eq!(l.form.runtime_mode(), RuntimeMode::Autopilot);
        l.handle_key(key(KeyCode::Left));
        assert_eq!(l.form.runtime_mode(), RuntimeMode::Plan, "mode cycling wraps");
        l.handle_key(key(KeyCode::Right));
        l.handle_key(key(KeyCode::Right));
        assert_eq!(l.form.runtime_mode(), RuntimeMode::Edits);
        enter(&mut l); // -> Model
        type_str(&mut l, "o3");
        enter(&mut l); // -> Skills
        l.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_str(&mut l, "rust, , review");
        enter(&mut l); // -> Color
        let before = l.form.color_idx;
        l.handle_key(key(KeyCode::Right));
        assert_eq!(l.form.color_idx, (before + 1) % AgentColor::ALL.len());
        let spec = match enter(&mut l) {
            Action::CreateAgent(spec) => spec,
            other => panic!("expected CreateAgent, got {other:?}"),
        };
        assert_eq!(spec.name, "scout");
        assert_eq!(spec.harness, Harness::Opencode);
        assert_eq!(spec.model.as_deref(), Some("o3"));
        assert_eq!(spec.skills, vec!["rust".to_string(), "review".to_string()]);
        assert_eq!(spec.runtime_mode, RuntimeMode::Edits);
        assert_eq!(spec.color, Some(AgentColor::ALL[(before + 1) % AgentColor::ALL.len()]));
        assert!(l.form.busy);
        assert_eq!(l.handle_key(key(KeyCode::Esc)), Action::None, "keys ignored while busy");
        assert_eq!(l.who_mode, WhoMode::NewAgent);

        let mut agents = l.agents.clone();
        agents.insert(0, agent("scout", "PK_SCOUT"));
        l.agent_created(agents, "PK_SCOUT");
        assert_eq!(l.who_mode, WhoMode::List);
        assert_eq!(l.selected_agent().map(|a| a.name.as_str()), Some("scout"));
        assert_eq!(enter(&mut l), Action::None);
        assert_eq!(l.step, Step::Workspace);
        assert_eq!(l.chosen_agent.as_deref(), Some("PK_SCOUT"));
    }

    #[test]
    fn new_agent_form_failure_and_cancel() {
        let mut l = at_where("t");
        enter(&mut l);
        l.handle_key(key(KeyCode::Char('n')));
        assert_eq!(l.who_mode, WhoMode::NewAgent);
        type_str(&mut l, "bad name");
        for _ in 0..5 {
            l.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(enter(&mut l), Action::None);
        assert!(l.form.error.as_deref().unwrap().contains("spaces"));
        l.form.name.clear();
        type_str(&mut l, "ok");
        for _ in 0..5 {
            l.handle_key(key(KeyCode::Tab));
        }
        assert!(matches!(enter(&mut l), Action::CreateAgent(_)));
        l.agent_create_failed("keystore locked");
        assert!(!l.form.busy);
        assert_eq!(l.form.error.as_deref(), Some("keystore locked"));
        assert_eq!(esc(&mut l), Action::None);
        assert_eq!(l.who_mode, WhoMode::List);
        assert_eq!(l.step, Step::Who);
    }

    #[test]
    fn summary_back_and_inbox() {
        let mut l = at_where("t");
        for _ in 0..3 {
            enter(&mut l);
        }
        assert_eq!(l.step, Step::Summary);
        assert_eq!(l.handle_key(key(KeyCode::Char('i'))), Action::OpenInbox);
        assert_eq!(esc(&mut l), Action::None);
        assert_eq!(l.step, Step::Workspace);
        assert_eq!(l.breadcrumbs().len(), 3);
    }

    #[test]
    fn paste_goes_to_focused_field() {
        let mut l = launcher();
        l.handle_paste("pasted\ntitle");
        assert_eq!(l.title.text(), "pasted\ntitle", "the prompt keeps lines");
        l.handle_key(key(KeyCode::Tab));
        l.handle_paste("zeb\nra");
        assert_eq!(l.filter.text(), "zeb ra", "one-line fields still collapse them");
    }

    #[test]
    fn set_agents_keeps_selection_by_pubkey() {
        let mut l = at_where("t");
        enter(&mut l);
        down(&mut l); // nova
        assert_eq!(l.selected_agent().unwrap().name, "nova");
        l.set_agents(vec![agent("nova", "PK_NOVA"), agent("atlas", "PK_ATLAS")]);
        assert_eq!(l.selected_agent().unwrap().name, "nova");
        l.set_agents(vec![]);
        assert_eq!(l.who_sel, 0);
        l.set_agents(vec![agent("atlas", "PK_ATLAS")]);
        assert_eq!(l.who_sel, 1, "an empty list gaining agents preselects the first");
    }
}
