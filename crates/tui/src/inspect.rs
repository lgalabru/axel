//! Token inspector state machine. Pure: holds an [`InspectData`] snapshot
//! (the model requests the per-session proxies recorded) plus the scope,
//! search, grouping and selection; consumes [`KeyEvent`]s and produces
//! [`InspectAction`]s. Rendering lives in `view::inspect_view`.
//!
//! Also the arithmetic the pane shows: per-part token estimates scaled to
//! what the provider billed, requests grouped by turn, totals, and the
//! anomalies worth a flag (a rebuilt cache, a context that keeps climbing).

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub use axel_store::{ContextBreakdown, ContextKind, ContextPart, ModelProvider, ModelRequest, ModelRequestFilter, RequestOutcome};

use crate::model::InspectData;

/// How far back the inspector looks: the usage pane's day.
pub const WINDOW_MS: i64 = 24 * 3600 * 1000;
/// Most requests one refresh reads. A busy agent makes a few hundred an
/// hour; a day of several agents fits, and parsing them once a second stays
/// in the low milliseconds (the store caps any read at 5000).
pub const REQUESTS_MAX: usize = 2000;
/// Bytes per token when the provider reported no usage: the usual rule of
/// thumb for English text and JSON.
pub const BYTES_PER_TOKEN: u64 = 4;
/// Longest search, in characters: a model or an agent's name.
const SEARCH_CHARS_MAX: usize = 64;
/// Rows PageUp / PageDown move.
const PAGE_ROWS: usize = 10;
/// Most anomalies listed; the rest would only repeat the pattern.
pub const ANOMALIES_MAX: usize = 8;
/// A cache write is a rebuild when it is at least this share of the
/// request's context…
const REBUILD_SHARE_PCT: u64 = 50;
/// …and the context is at least this big (a first small request writes its
/// whole cache, and that's no news).
const REBUILD_TOKENS_MIN: u64 = 20_000;
/// A turn's context "climbs" when it grows by at least this many tokens…
const CLIMB_TOKENS_MIN: u64 = 30_000;
/// …to at least this many times where it started…
const CLIMB_FACTOR_PCT: u64 = 150;
/// …over at least this many requests.
const CLIMB_REQUESTS_MIN: usize = 3;

/// Which requests the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Thread(String),
    Agent(String),
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    List,
    Inspector,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectAction {
    None,
    Quit,
    /// The scope changed: read the store again.
    Reload,
    /// Turn recording of this thread on (`true`) or off, then read again.
    SetInspected(String, bool),
}

/// A turn of one session: what the list groups requests by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GroupKey {
    pub session_id: String,
    pub turn: Option<u32>,
}

impl GroupKey {
    pub fn of(r: &ModelRequest) -> GroupKey {
        GroupKey { session_id: r.session_id.clone(), turn: r.turn }
    }
}

/// Requests added up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub requests: u32,
    pub context_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub in_flight: u32,
    pub errors: u32,
}

impl Totals {
    pub fn add(&mut self, r: &ModelRequest) {
        self.requests = self.requests.saturating_add(1);
        self.context_tokens = self.context_tokens.saturating_add(r.context_tokens());
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(r.cache_read_tokens.unwrap_or(0));
        self.output_tokens = self.output_tokens.saturating_add(r.output_tokens.unwrap_or(0));
        self.cost_usd += r.cost_usd_est();
        match r.outcome {
            RequestOutcome::InFlight => self.in_flight = self.in_flight.saturating_add(1),
            RequestOutcome::HttpError | RequestOutcome::Failed => self.errors = self.errors.saturating_add(1),
            RequestOutcome::Completed | RequestOutcome::Cancelled => {}
        }
    }

    pub fn of<'a>(requests: impl IntoIterator<Item = &'a ModelRequest>) -> Totals {
        let mut t = Totals::default();
        for r in requests {
            t.add(r);
        }
        t
    }

    /// Share of the context read from the cache, 0–100; `None` with no context.
    pub fn cache_hit_pct(&self) -> Option<u64> {
        let pct = (self.context_tokens > 0).then(|| share_pct(self.cache_read_tokens, self.context_tokens));
        debug_assert!(pct.is_none_or(|p| p <= 100), "cache reads are part of the context");
        pct
    }
}

/// `part` of `whole`, in whole percent, rounded; 0 when `whole` is.
pub fn share_pct(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    let pct = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    u64::try_from(pct).unwrap_or(u64::MAX)
}

/// One turn's requests, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnGroup {
    pub key: GroupKey,
    pub agent_pubkey: String,
    /// Indices into the requests, newest first.
    pub requests: Vec<usize>,
    pub totals: Totals,
}

/// `indices` (into `requests`, newest first) grouped by session and turn;
/// groups in the order of their newest request, so the busiest turn now is on top.
pub fn group_by_turn(requests: &[ModelRequest], indices: &[usize]) -> Vec<TurnGroup> {
    let mut groups: Vec<TurnGroup> = Vec::new();
    let mut at: HashMap<GroupKey, usize> = HashMap::new();
    for &i in indices {
        let r = &requests[i];
        let key = GroupKey::of(r);
        let g = *at.entry(key.clone()).or_insert_with(|| {
            groups.push(TurnGroup { key, agent_pubkey: r.agent_pubkey.clone(), requests: Vec::new(), totals: Totals::default() });
            groups.len() - 1
        });
        groups[g].requests.push(i);
        groups[g].totals.add(r);
    }
    debug_assert_eq!(groups.iter().map(|g| g.requests.len()).sum::<usize>(), indices.len(), "every request lands in one group");
    groups
}

/// One part of a request's context with its estimated tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartEstimate {
    pub kind: ContextKind,
    pub label: String,
    pub bytes: u64,
    pub count: u32,
    pub tokens: u64,
}

/// A breakdown in tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// In the breakdown's order.
    pub parts: Vec<PartEstimate>,
    pub tokens: u64,
    /// Scaled to the provider's count; else bytes over [`BYTES_PER_TOKEN`].
    pub scaled: bool,
}

/// Each part's tokens: its bytes scaled so the parts add up to
/// `context_tokens` exactly (the largest remainders get the rounding), or
/// bytes over [`BYTES_PER_TOKEN`] when the provider didn't say (0).
pub fn estimate(breakdown: &ContextBreakdown, context_tokens: u64) -> Estimate {
    let bytes_sum: u64 = breakdown.parts.iter().map(|p| p.bytes).fold(0, u64::saturating_add);
    let part = |p: &ContextPart, tokens: u64| PartEstimate { kind: p.kind, label: p.label.clone(), bytes: p.bytes, count: p.count, tokens };
    if context_tokens == 0 || bytes_sum == 0 {
        let parts: Vec<PartEstimate> = breakdown.parts.iter().map(|p| part(p, p.bytes / BYTES_PER_TOKEN)).collect();
        let tokens = parts.iter().map(|p| p.tokens).fold(0, u64::saturating_add);
        return Estimate { parts, tokens, scaled: false };
    }
    let total = u128::from(context_tokens);
    let sum = u128::from(bytes_sum);
    let mut parts = Vec::with_capacity(breakdown.parts.len());
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(breakdown.parts.len());
    for (i, p) in breakdown.parts.iter().enumerate() {
        let exact = u128::from(p.bytes) * total;
        parts.push(part(p, u64::try_from(exact / sum).expect("a share of a u64 fits a u64")));
        remainders.push((exact % sum, i));
    }
    let floor_sum: u64 = parts.iter().map(|p| p.tokens).sum();
    assert!(floor_sum <= context_tokens, "rounding down never overshoots");
    let short = usize::try_from(context_tokens - floor_sum).expect("short by fewer tokens than parts");
    assert!(short <= parts.len(), "each part rounds down by less than one token");
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, i) in remainders.iter().take(short) {
        parts[i].tokens += 1;
    }
    debug_assert_eq!(parts.iter().map(|p| p.tokens).sum::<u64>(), context_tokens, "the parts add up to what was billed");
    Estimate { parts, tokens: context_tokens, scaled: true }
}

/// One bar of the breakdown: a kind added up (tool results stay one bar per
/// tool), or one tool's definition under the definitions when they're open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarRow {
    pub kind: ContextKind,
    /// The tool, for per-tool bars; empty for a kind's total.
    pub label: String,
    pub tokens: u64,
    pub count: u32,
    /// Under the tool definitions' bar.
    pub nested: bool,
}

/// The bars, largest first, since the question is what made a request big:
/// a bar per kind (system prompt, tool definitions, user and assistant
/// text, tool calls, images, reasoning, the rest) and one per tool for tool
/// results. Each tool's definition follows the definitions' bar when
/// `tools_open`, largest first too.
pub fn bar_rows(est: &Estimate, tools_open: bool) -> Vec<BarRow> {
    // Top-level bars, each with the nested ones that follow it.
    let mut out: Vec<(BarRow, Vec<BarRow>)> = Vec::new();
    for kind in ContextKind::ALL {
        let mut of_kind: Vec<&PartEstimate> = est.parts.iter().filter(|p| p.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        of_kind.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.label.cmp(&b.label)));
        let row = |p: &PartEstimate, nested: bool| BarRow { kind, label: p.label.clone(), tokens: p.tokens, count: p.count, nested };
        if kind == ContextKind::ToolResult {
            out.extend(of_kind.iter().map(|p| (row(p, false), Vec::new())));
            continue;
        }
        let tokens = of_kind.iter().map(|p| p.tokens).fold(0u64, u64::saturating_add);
        let count = of_kind.iter().map(|p| p.count).fold(0u32, u32::saturating_add);
        let nested = if kind == ContextKind::ToolDefinition && tools_open { of_kind.iter().map(|p| row(p, true)).collect() } else { Vec::new() };
        out.push((BarRow { kind, label: String::new(), tokens, count, nested: false }, nested));
    }
    // Stable: equal bars keep the kinds' order.
    out.sort_by_key(|a| std::cmp::Reverse(a.0.tokens));
    out.into_iter().flat_map(|(top, nested)| std::iter::once(top).chain(nested)).collect()
}

/// The biggest bar (tools folded) and its share of the context.
pub fn largest(est: &Estimate) -> Option<(BarRow, u64)> {
    let rows = bar_rows(est, false);
    // A tie goes to the kind listed first.
    let top = rows.into_iter().max_by(|a, b| a.tokens.cmp(&b.tokens).then(kind_rank(b.kind).cmp(&kind_rank(a.kind))))?;
    (top.tokens > 0).then(|| {
        let pct = share_pct(top.tokens, est.tokens);
        (top, pct)
    })
}

/// Where a kind sits in [`ContextKind::ALL`].
fn kind_rank(kind: ContextKind) -> usize {
    ContextKind::ALL.iter().position(|k| *k == kind).unwrap_or(ContextKind::ALL.len())
}

/// The request rebuilt its cache: most of its context was written, not read.
pub fn is_rebuild(r: &ModelRequest) -> bool {
    let context = r.context_tokens();
    let written = r.cache_write_tokens.unwrap_or(0);
    context >= REBUILD_TOKENS_MIN && share_pct(written, context) >= REBUILD_SHARE_PCT
}

/// Something worth a flag in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anomaly {
    /// `request` (an index) wrote `written` tokens to the cache.
    CacheRebuilt { request: usize, written: u64 },
    /// `group`'s context grew from `from` to `to` tokens over `requests` requests.
    ContextClimb { group: usize, from: u64, to: u64, requests: u32 },
    /// Requests that failed.
    Errors { count: u32 },
}

/// The anomalies among `groups`, the worst kinds first, at most [`ANOMALIES_MAX`].
pub fn anomalies(requests: &[ModelRequest], groups: &[TurnGroup]) -> Vec<Anomaly> {
    let mut out = Vec::new();
    let errors: u32 = groups.iter().map(|g| g.totals.errors).fold(0, u32::saturating_add);
    if errors > 0 {
        out.push(Anomaly::Errors { count: errors });
    }
    for g in groups {
        for &i in &g.requests {
            if is_rebuild(&requests[i]) {
                out.push(Anomaly::CacheRebuilt { request: i, written: requests[i].cache_write_tokens.unwrap_or(0) });
            }
        }
    }
    for (gi, g) in groups.iter().enumerate() {
        if let Some((from, to, n)) = climb(requests, &g.requests) {
            out.push(Anomaly::ContextClimb { group: gi, from, to, requests: n });
        }
    }
    out.truncate(ANOMALIES_MAX);
    out
}

/// A turn whose context grew most of the way, a lot: `(from, to, requests)`.
/// `newest_first` indexes `requests`; requests without usage are left out.
fn climb(requests: &[ModelRequest], newest_first: &[usize]) -> Option<(u64, u64, u32)> {
    let sizes: Vec<u64> = newest_first.iter().rev().map(|&i| requests[i].context_tokens()).filter(|t| *t > 0).collect();
    if sizes.len() < CLIMB_REQUESTS_MIN {
        return None;
    }
    let (from, to) = (sizes[0], sizes[sizes.len() - 1]);
    let steps = sizes.len() - 1;
    let rising = sizes.windows(2).filter(|w| w[1] >= w[0]).count();
    // Three steps in four rising: a compaction or a retry in between doesn't hide it.
    let mostly_rising = rising * 4 >= steps * 3;
    let grew = to >= from.saturating_add(CLIMB_TOKENS_MIN) && u128::from(to) * 100 >= u128::from(from) * u128::from(CLIMB_FACTOR_PCT);
    (mostly_rising && grew).then(|| (from, to, u32::try_from(sizes.len()).unwrap_or(u32::MAX)))
}

/// A line of the request list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A turn's heading; a folded one stands for its requests.
    Header { group: usize, folded: bool },
    /// A request, by index into the data's requests.
    Request { index: usize },
}

/// What's selected, kept across refreshes by identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Request(String),
    Group(GroupKey),
}

/// The list as shown: the scope's requests matching the search, grouped.
#[derive(Clone, Debug, PartialEq)]
pub struct ListView {
    /// Indices into the data's requests, newest first.
    pub shown: Vec<usize>,
    pub groups: Vec<TurnGroup>,
    pub entries: Vec<Entry>,
    pub totals: Totals,
}

#[derive(Clone, Debug)]
pub struct Inspector {
    pub data: InspectData,
    pub scope: Scope,
    /// The thread the inspector was opened for, if any: where `f` comes back to.
    pub home_thread: Option<String>,
    pub focus: Focus,
    pub search: String,
    /// Typing into the search.
    pub searching: bool,
    /// Requests grouped by turn (else one flat list).
    pub grouped: bool,
    pub folded: HashSet<GroupKey>,
    pub selected: Option<Selection>,
    /// Where the selection was, for when it goes away.
    sel_hint: usize,
    /// Each tool's definition under the definitions' bar.
    pub tools_open: bool,
    /// Lines the inspector is scrolled down.
    pub scroll: u16,
    pub help_open: bool,
}

impl Inspector {
    /// The current thread's requests when there is one, else every thread's.
    pub fn new(home_thread: Option<String>, data: InspectData) -> Inspector {
        let scope = home_thread.clone().map_or(Scope::All, Scope::Thread);
        let mut inspector = Inspector {
            data: InspectData::default(),
            scope,
            home_thread,
            focus: Focus::List,
            search: String::new(),
            searching: false,
            grouped: true,
            folded: HashSet::new(),
            selected: None,
            sel_hint: 0,
            tools_open: false,
            scroll: 0,
            help_open: false,
        };
        inspector.set_data(data);
        inspector
    }

    /// What to read from the store for the current scope.
    pub fn query(&self, now_ms: i64) -> ModelRequestFilter {
        let (thread_id, agent_pubkey) = match &self.scope {
            Scope::Thread(t) => (Some(t.clone()), None),
            Scope::Agent(a) => (None, Some(a.clone())),
            Scope::All => (None, None),
        };
        ModelRequestFilter { thread_id, session_id: None, agent_pubkey, since_ms: now_ms - WINDOW_MS, limit: REQUESTS_MAX }
    }

    /// A new snapshot; the selection stays on the same request or turn.
    pub fn set_data(&mut self, data: InspectData) {
        assert!(data.requests.len() <= axel_store::MODEL_REQUESTS_LIMIT_MAX, "the store caps a read");
        let before = self.selected_request().map(|r| r.id.clone());
        self.data = data;
        let view = self.list();
        if self.selected_entry(&view).is_none() {
            self.select_at(&view, self.sel_hint);
        }
        if before != self.selected_request().map(|r| r.id.clone()) {
            self.scroll = 0;
        }
    }

    /// The name and colour of an agent, by key: its first characters when unknown.
    pub fn agent(&self, pubkey: &str) -> (String, Option<crate::model::AgentColor>) {
        self.data.agents.get(pubkey).cloned().unwrap_or_else(|| (pubkey.chars().take(8).collect(), None))
    }

    /// Whether what the pane shows is being recorded: the scoped thread's
    /// switch, or, scoped wider, whether any thread's is on.
    pub fn is_recording(&self) -> bool {
        match &self.scope {
            Scope::Thread(thread) => self.data.inspected.contains(thread),
            Scope::Agent(_) | Scope::All => !self.data.inspected.is_empty(),
        }
    }

    pub fn is_live(&self) -> bool {
        self.data.requests.iter().any(|r| r.outcome == RequestOutcome::InFlight)
    }

    /// The scope's requests that match the search, grouped as asked.
    pub fn list(&self) -> ListView {
        let needle = self.search.to_lowercase();
        let shown: Vec<usize> = (0..self.data.requests.len()).filter(|&i| needle.is_empty() || self.matches(&self.data.requests[i], &needle)).collect();
        let totals = Totals::of(shown.iter().map(|&i| &self.data.requests[i]));
        let groups = group_by_turn(&self.data.requests, &shown);
        let mut entries = Vec::with_capacity(shown.len() + groups.len());
        if self.grouped {
            for (g, group) in groups.iter().enumerate() {
                let folded = self.folded.contains(&group.key);
                entries.push(Entry::Header { group: g, folded });
                if !folded {
                    entries.extend(group.requests.iter().map(|&index| Entry::Request { index }));
                }
            }
        } else {
            entries.extend(shown.iter().map(|&index| Entry::Request { index }));
        }
        ListView { shown, groups, entries, totals }
    }

    fn matches(&self, r: &ModelRequest, needle: &str) -> bool {
        let model = r.model.as_deref().unwrap_or("").to_lowercase();
        let agent = self.agent(&r.agent_pubkey).0.to_lowercase();
        model.contains(needle) || agent.contains(needle) || r.endpoint.to_lowercase().contains(needle)
    }

    /// The entry the selection is on, by position in `view.entries`.
    pub fn selected_entry(&self, view: &ListView) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        view.entries.iter().position(|e| match (e, selected) {
            (Entry::Request { index }, Selection::Request(id)) => self.data.requests[*index].id == *id,
            (Entry::Header { group, folded: true }, Selection::Group(key)) => view.groups[*group].key == *key,
            _ => false,
        })
    }

    /// The request the inspector shows: the selected one, or a folded turn's newest.
    pub fn selected_request(&self) -> Option<&ModelRequest> {
        match self.selected.as_ref()? {
            Selection::Request(id) => self.data.requests.iter().find(|r| r.id == *id),
            Selection::Group(key) => self.data.requests.iter().find(|r| GroupKey::of(r) == *key),
        }
    }

    /// Requests and folded turns can be selected; an open turn's heading can't.
    fn selectable(entry: Entry) -> bool {
        !matches!(entry, Entry::Header { folded: false, .. })
    }

    fn selection_of(&self, view: &ListView, at: usize) -> Selection {
        match view.entries[at] {
            Entry::Request { index } => Selection::Request(self.data.requests[index].id.clone()),
            Entry::Header { group, .. } => Selection::Group(view.groups[group].key.clone()),
        }
    }

    /// The selectable entry nearest `at` (at or after it, else before), or none.
    fn select_at(&mut self, view: &ListView, at: usize) {
        let n = view.entries.len();
        let at = at.min(n.saturating_sub(1));
        let found = (at..n).find(|&i| Self::selectable(view.entries[i])).or_else(|| (0..at).rev().find(|&i| Self::selectable(view.entries[i])));
        self.selected = found.map(|i| self.selection_of(view, i));
        self.sel_hint = found.unwrap_or(0);
    }

    /// Move the selection `delta` selectable entries.
    fn step(&mut self, delta: isize) {
        let view = self.list();
        let Some(mut at) = self.selected_entry(&view) else {
            self.select_at(&view, 0);
            return;
        };
        let mut left = delta.unsigned_abs();
        let n = view.entries.len();
        // Bounded by the list's length: each pass moves one entry or stops.
        for _ in 0..n {
            if left == 0 {
                break;
            }
            let next = if delta < 0 { at.checked_sub(1) } else { Some(at + 1).filter(|&i| i < n) };
            let Some(next) = next else { break };
            at = next;
            if Self::selectable(view.entries[at]) {
                left -= 1;
                self.selected = Some(self.selection_of(&view, at));
                self.sel_hint = at;
            }
        }
        self.scroll = 0;
    }

    /// Fold or unfold the selected request's turn (`Some(true)` folds).
    fn fold(&mut self, fold: Option<bool>) {
        if !self.grouped {
            return;
        }
        let Some(key) = self.selected_request().map(GroupKey::of) else { return };
        let folded = self.folded.contains(&key);
        let want = fold.unwrap_or(!folded);
        if want {
            self.folded.insert(key.clone());
            self.selected = Some(Selection::Group(key));
        } else {
            self.folded.remove(&key);
            if let Some(r) = self.selected_request() {
                self.selected = Some(Selection::Request(r.id.clone()));
            }
        }
        let view = self.list();
        self.sel_hint = self.selected_entry(&view).unwrap_or(self.sel_hint);
    }

    /// Thread, then the selected request's agent, then everything, then back.
    fn next_scope(&self) -> Scope {
        let selected = self.selected_request();
        let thread = self.home_thread.clone().or_else(|| selected.map(|r| r.thread_id.clone())).filter(|t| !t.is_empty());
        let agent = selected.map(|r| r.agent_pubkey.clone()).filter(|a| !a.is_empty());
        match &self.scope {
            Scope::Thread(_) => agent.map_or(Scope::All, Scope::Agent),
            Scope::Agent(_) => Scope::All,
            Scope::All => thread.map(Scope::Thread).or(agent.map(Scope::Agent)).unwrap_or(Scope::All),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InspectAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return InspectAction::Quit;
        }
        if self.help_open {
            self.help_open = false;
            return InspectAction::None;
        }
        if self.searching {
            self.search_key(key);
            return InspectAction::None;
        }
        match self.focus {
            Focus::List => self.list_key(key),
            Focus::Inspector => self.inspector_key(key),
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.searching = false,
            KeyCode::Esc => {
                self.searching = false;
                self.search.clear();
            }
            KeyCode::Backspace => {
                self.search.pop();
            }
            KeyCode::Char(c) if !c.is_control() && self.search.chars().count() < SEARCH_CHARS_MAX => self.search.push(c),
            _ => return,
        }
        let view = self.list();
        if self.selected_entry(&view).is_none() {
            self.select_at(&view, 0);
        }
    }

    /// Keys either side handles alike; `None` when it isn't one of them.
    fn common_key(&mut self, key: KeyEvent) -> Option<InspectAction> {
        match key.code {
            KeyCode::Char('f') => {
                self.scope = self.next_scope();
                self.folded.clear();
                Some(InspectAction::Reload)
            }
            KeyCode::Char('/') => {
                self.searching = true;
                self.focus = Focus::List;
                Some(InspectAction::None)
            }
            KeyCode::Char('g') => {
                self.grouped = !self.grouped;
                let view = self.list();
                if self.selected_entry(&view).is_none() {
                    // A folded turn's selection becomes its newest request.
                    self.selected = self.selected_request().map(|r| Selection::Request(r.id.clone()));
                }
                Some(InspectAction::None)
            }
            // Recording is a thread's switch: it needs a thread in scope.
            KeyCode::Char('r') => match &self.scope {
                Scope::Thread(thread) => Some(InspectAction::SetInspected(thread.clone(), !self.data.inspected.contains(thread))),
                Scope::Agent(_) | Scope::All => Some(InspectAction::None),
            },
            KeyCode::Char('t') => {
                self.tools_open = !self.tools_open;
                Some(InspectAction::None)
            }
            KeyCode::Char('?') => {
                self.help_open = true;
                Some(InspectAction::None)
            }
            _ => None,
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> InspectAction {
        if let Some(action) = self.common_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc if !self.search.is_empty() => self.search.clear(),
            KeyCode::Esc | KeyCode::Char('q') => return InspectAction::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-(PAGE_ROWS as isize)),
            KeyCode::PageDown => self.step(PAGE_ROWS as isize),
            KeyCode::Enter | KeyCode::Tab if self.selected_request().is_some() => self.focus = Focus::Inspector,
            KeyCode::Char(' ') => self.fold(None),
            KeyCode::Left | KeyCode::Char('h') => self.fold(Some(true)),
            KeyCode::Right | KeyCode::Char('l') => self.fold(Some(false)),
            _ => {}
        }
        InspectAction::None
    }

    fn inspector_key(&mut self, key: KeyEvent) -> InspectAction {
        if let Some(action) = self.common_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Tab | KeyCode::BackTab => self.focus = Focus::List,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(PAGE_ROWS as u16),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(PAGE_ROWS as u16),
            KeyCode::Enter => self.tools_open = !self.tools_open,
            _ => {}
        }
        InspectAction::None
    }

    /// The keys that work now, for the help line.
    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        use axel_i18n::t;
        if self.searching {
            return vec![("Enter", t!("done")), ("Esc", t!("clear"))];
        }
        match self.focus {
            Focus::List => vec![
                ("↑↓", t!("move")),
                ("Enter", t!("inspect")),
                ("f", t!("filter")),
                ("/", t!("search")),
                ("g", t!("group by turn")),
                ("space", t!("fold")),
                ("?", t!("help")),
                ("q", t!("quit")),
            ],
            Focus::Inspector => vec![
                ("↑↓", t!("scroll")),
                ("t", t!("each tool")),
                ("f", t!("filter")),
                ("?", t!("help")),
                ("Esc", t!("back to the list")),
            ],
        }
    }
}

/// Sample requests for tests, previews and seeding a throwaway database:
/// two agents (a Claude and a Codex) on two threads over a few turns, with
/// breakdowns, one still in flight, one error and one heavy cache write.
pub mod fixtures {
    use super::*;
    use crate::model::AgentColor;

    pub const TERRA: &str = "PK_terra";
    pub const ORION: &str = "PK_orion";
    pub const THREAD_WEBHOOKS: &str = "T_webhooks";
    pub const THREAD_REVIEW: &str = "T_review";

    fn part(kind: ContextKind, label: &str, bytes: u64, count: u32) -> ContextPart {
        ContextPart { kind, label: label.into(), bytes, count }
    }

    /// What Claude Code sends on a turn deep into a task: a big system
    /// prompt, two dozen tools, and file reads piling up.
    pub fn claude_breakdown(reads_kib: u64) -> ContextBreakdown {
        use ContextKind::*;
        let mut parts = vec![
            part(ToolResult, "Read", reads_kib * 1024, 14),
            part(System, "", 14_200, 1),
            part(ToolDefinition, "Bash", 9_800, 1),
            part(ToolDefinition, "Edit", 6_400, 1),
            part(ToolDefinition, "Task", 5_900, 1),
            part(ToolDefinition, "mcp__axel__send", 4_100, 1),
            part(ToolDefinition, "Read", 3_300, 1),
            part(ToolDefinition, "Grep", 3_100, 1),
            part(ToolResult, "Bash", 18_400, 6),
            part(AssistantText, "", 9_100, 11),
            part(ToolCall, "Read", 2_900, 14),
            part(ToolCall, "Bash", 1_800, 6),
            part(UserText, "", 3_700, 2),
            part(Reasoning, "", 6_200, 5),
            part(ToolResult, "Grep", 4_800, 3),
        ];
        parts.sort_by_key(|a| std::cmp::Reverse(a.bytes));
        ContextBreakdown { parts, messages: 41, tools: 6 }
    }

    /// Codex over the Responses API: instructions, a handful of tools, shell output.
    pub fn codex_breakdown() -> ContextBreakdown {
        use ContextKind::*;
        let mut parts = vec![
            part(System, "", 22_500, 1),
            part(ToolDefinition, "shell", 2_600, 1),
            part(ToolDefinition, "apply_patch", 3_900, 1),
            part(ToolDefinition, "update_plan", 1_200, 1),
            part(ToolResult, "shell", 31_000, 9),
            part(ToolCall, "shell", 2_400, 9),
            part(UserText, "", 1_900, 1),
            part(AssistantText, "", 2_800, 4),
            part(Reasoning, "", 7_700, 4),
            part(Image, "", 48_000, 1),
        ];
        parts.sort_by_key(|a| std::cmp::Reverse(a.bytes));
        ContextBreakdown { parts, messages: 28, tools: 3 }
    }

    #[allow(clippy::too_many_arguments)]
    fn request(id: &str, agent: &str, thread: &str, turn: u32, at_ms: i64, model: &str, provider: ModelProvider, usage: [u64; 4]) -> ModelRequest {
        let [input, cache_read, cache_write, output] = usage;
        let claude = provider == ModelProvider::Anthropic;
        ModelRequest {
            id: id.into(),
            session_id: format!("S_{agent}_{thread}"),
            thread_id: thread.into(),
            agent_pubkey: agent.into(),
            turn: Some(turn),
            provider,
            endpoint: if claude { "/v1/messages".into() } else { "/responses".into() },
            model: Some(model.into()),
            started_at_ms: at_ms,
            first_byte_ms: Some(640 + (at_ms % 7) as u32 * 90),
            duration_ms: Some(2_400 + (at_ms % 11) as u32 * 380),
            http_status: Some(200),
            streamed: true,
            outcome: RequestOutcome::Completed,
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            reasoning_tokens: if claude { None } else { Some(output / 3) },
            request_bytes: 0,
            response_bytes: 1_800 + output * 5,
            stop_reason: Some(if claude { "tool_use".into() } else { "completed".into() }),
            error: None,
            breakdown: Some(if claude { claude_breakdown(40 + (at_ms / 1000 % 60) as u64) } else { codex_breakdown() }),
        }
    }

    /// The requests, newest first, the newest started `now_ms`.
    pub fn requests(now_ms: i64) -> Vec<ModelRequest> {
        let sec = 1000;
        let a = ModelProvider::Anthropic;
        let o = ModelProvider::ChatGpt;
        let sonnet = "claude-sonnet-5-5";
        let codex = "gpt-5.2-codex";
        let mut out = Vec::new();
        // terra (Claude) on the webhooks thread, turn 6: a context that climbs.
        for (n, ctx) in [52_000u64, 71_000, 94_000, 118_000, 141_000].into_iter().enumerate() {
            let at = now_ms - 3_600 * sec + n as i64 * 40 * sec;
            out.push(request(&format!("R6_{n}"), TERRA, THREAD_WEBHOOKS, 6, at, sonnet, a, [1_100, ctx - 4_000, 2_900, 380 + n as u64 * 40]));
        }
        // terra, turn 7: the cache had expired, so the first request wrote it all again.
        out.push(request("R7_0", TERRA, THREAD_WEBHOOKS, 7, now_ms - 600 * sec, sonnet, a, [900, 0, 96_000, 420]));
        out.push(request("R7_1", TERRA, THREAD_WEBHOOKS, 7, now_ms - 560 * sec, sonnet, a, [1_300, 96_000, 2_100, 260]));
        // orion (Codex) on the review thread.
        out.push(request("C3_0", ORION, THREAD_REVIEW, 3, now_ms - 420 * sec, codex, o, [6_200, 31_000, 0, 1_900]));
        out.push(request("C3_1", ORION, THREAD_REVIEW, 3, now_ms - 380 * sec, codex, o, [2_100, 38_500, 0, 2_600]));
        let mut failed = request("C3_2", ORION, THREAD_REVIEW, 3, now_ms - 300 * sec, codex, o, [0, 0, 0, 0]);
        failed.outcome = RequestOutcome::HttpError;
        failed.http_status = Some(429);
        failed.duration_ms = Some(310);
        failed.first_byte_ms = Some(300);
        failed.streamed = false;
        failed.input_tokens = None;
        failed.output_tokens = None;
        failed.cache_read_tokens = None;
        failed.cache_write_tokens = None;
        failed.reasoning_tokens = None;
        failed.stop_reason = None;
        failed.error = Some("429 Too Many Requests: rate limit reached for gpt-5.2-codex".into());
        out.push(failed);
        out.push(request("R7_2", TERRA, THREAD_WEBHOOKS, 7, now_ms - 60 * sec, sonnet, a, [1_500, 98_100, 1_800, 310]));
        // terra, still waiting on its answer.
        let mut live = request("R7_3", TERRA, THREAD_WEBHOOKS, 7, now_ms, sonnet, a, [0, 0, 0, 0]);
        live.outcome = RequestOutcome::InFlight;
        live.http_status = None;
        live.first_byte_ms = None;
        live.duration_ms = None;
        live.input_tokens = None;
        live.output_tokens = None;
        live.cache_read_tokens = None;
        live.cache_write_tokens = None;
        live.stop_reason = None;
        live.response_bytes = 0;
        out.push(live);
        for r in &mut out {
            r.request_bytes = r.breakdown.as_ref().map_or(0, |b| b.parts.iter().map(|p| p.bytes).sum::<u64>() + 2_000);
        }
        out.sort_by(|x, y| y.started_at_ms.cmp(&x.started_at_ms).then(y.id.cmp(&x.id)));
        out
    }

    /// The fixture as the inspector reads it.
    pub fn data(now_ms: i64) -> InspectData {
        InspectData {
            requests: requests(now_ms),
            agents: HashMap::from([
                (TERRA.to_string(), ("terra".to_string(), Some(AgentColor::Purple))),
                (ORION.to_string(), ("orion".to_string(), Some(AgentColor::Green))),
            ]),
            threads: HashMap::from([
                (THREAD_WEBHOOKS.to_string(), "webhook retries".to_string()),
                (THREAD_REVIEW.to_string(), "review the pay-kit PR".to_string()),
            ]),
            now_ms,
            inspected: [THREAD_WEBHOOKS.to_string(), THREAD_REVIEW.to_string()].into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn breakdown(bytes: &[u64]) -> ContextBreakdown {
        ContextBreakdown { parts: bytes.iter().map(|&b| ContextPart { kind: ContextKind::UserText, label: String::new(), bytes: b, count: 1 }).collect(), messages: 1, tools: 0 }
    }

    #[test]
    fn estimates_scale_to_the_billed_context_exactly() {
        let e = estimate(&breakdown(&[1, 1, 1]), 10);
        assert!(e.scaled);
        assert_eq!(e.parts.iter().map(|p| p.tokens).collect::<Vec<_>>(), [4, 3, 3], "the rounding goes to the first of equal remainders");
        let e = estimate(&fixtures::claude_breakdown(40), 93_117);
        assert_eq!(e.parts.iter().map(|p| p.tokens).sum::<u64>(), 93_117);
        assert_eq!(e.tokens, 93_117);
        let big = estimate(&breakdown(&[u64::MAX / 2, u64::MAX / 2]), u64::MAX);
        assert_eq!(big.parts.iter().map(|p| p.tokens).fold(0u64, u64::saturating_add), u64::MAX, "no overflow at the edge");
    }

    #[test]
    fn without_usage_an_estimate_is_bytes_over_four() {
        let e = estimate(&breakdown(&[400, 41]), 0);
        assert!(!e.scaled);
        assert_eq!(e.parts.iter().map(|p| p.tokens).collect::<Vec<_>>(), [100, 10]);
        assert_eq!(e.tokens, 110);
        let empty = estimate(&breakdown(&[0, 0]), 5_000);
        assert!(!empty.scaled && empty.tokens == 0, "nothing to scale");
    }

    #[test]
    fn bars_fold_tools_and_keep_tool_results_apart() {
        let e = estimate(&fixtures::claude_breakdown(40), 100_000);
        let rows = bar_rows(&e, false);
        let defs: Vec<_> = rows.iter().filter(|r| r.kind == ContextKind::ToolDefinition).collect();
        assert_eq!(defs.len(), 1, "one bar for every definition");
        assert_eq!(defs[0].count, 6);
        let results: Vec<_> = rows.iter().filter(|r| r.kind == ContextKind::ToolResult).map(|r| r.label.as_str()).collect();
        assert_eq!(results, ["Read", "Bash", "Grep"], "by tool, largest first");
        assert_eq!((rows[0].kind, rows[0].label.as_str()), (ContextKind::ToolResult, "Read"), "largest first");
        assert!(rows.windows(2).all(|w| w[0].tokens >= w[1].tokens), "{rows:?}");
        let open = bar_rows(&e, true);
        assert_eq!(open.iter().filter(|r| r.nested).count(), 6, "each tool under the definitions");
        let (top, pct) = largest(&e).unwrap();
        assert_eq!((top.kind, top.label.as_str()), (ContextKind::ToolResult, "Read"));
        assert!(pct > 25 && pct < 50, "{pct}");
    }

    #[test]
    fn requests_group_by_session_and_turn_newest_first() {
        let rs = fixtures::requests(NOW);
        let all: Vec<usize> = (0..rs.len()).collect();
        let groups = group_by_turn(&rs, &all);
        let keys: Vec<(String, Option<u32>)> = groups.iter().map(|g| (g.key.session_id.clone(), g.key.turn)).collect();
        assert_eq!(keys.len(), 3, "{keys:?}");
        assert_eq!(keys[0].1, Some(7), "the turn with the newest request first, though orion's came in between");
        assert_eq!(groups[0].requests.len(), 4);
        assert_eq!(keys[1].1, Some(3));
        assert_eq!(keys[2].1, Some(6));
        assert_eq!(groups[2].totals.requests, 5);
        assert!(groups[0].requests.windows(2).all(|w| rs[w[0]].started_at_ms >= rs[w[1]].started_at_ms), "newest first inside a group");
        assert_eq!(groups[0].totals.in_flight, 1);
        assert_eq!(groups[1].totals.errors, 1);
    }

    #[test]
    fn anomalies_flag_the_rebuild_the_climb_and_the_error() {
        let rs = fixtures::requests(NOW);
        let all: Vec<usize> = (0..rs.len()).collect();
        let groups = group_by_turn(&rs, &all);
        let found = anomalies(&rs, &groups);
        assert!(matches!(found[0], Anomaly::Errors { count: 1 }), "{found:?}");
        assert!(found.iter().any(|a| matches!(a, Anomaly::CacheRebuilt { request, .. } if rs[*request].id == "R7_0")), "{found:?}");
        assert!(found.iter().any(|a| matches!(a, Anomaly::ContextClimb { group: 2, from: 52_000, to: 141_000, requests: 5 })), "{found:?}");
        assert!(!found.iter().any(|a| matches!(a, Anomaly::ContextClimb { group: 0, .. })), "turn 7 started big after a rebuild, then held: {found:?}");
    }

    #[test]
    fn r_turns_the_scoped_threads_recording_on_or_off() {
        let mut data = fixtures::data(NOW);
        data.inspected.clear();
        let mut i = Inspector::new(Some(fixtures::THREAD_WEBHOOKS.into()), data);
        assert!(!i.is_recording(), "a thread never turned on isn't recorded");
        assert_eq!(i.handle_key(key(KeyCode::Char('r'))), InspectAction::SetInspected(fixtures::THREAD_WEBHOOKS.into(), true));
        i.data.inspected.insert(fixtures::THREAD_WEBHOOKS.into());
        assert!(i.is_recording());
        assert_eq!(i.handle_key(key(KeyCode::Char('r'))), InspectAction::SetInspected(fixtures::THREAD_WEBHOOKS.into(), false));
        // Scoped wider, there's no one thread to switch.
        let mut all = Inspector::new(None, fixtures::data(NOW));
        assert_eq!(all.handle_key(key(KeyCode::Char('r'))), InspectAction::None);
        assert!(all.is_recording(), "some thread is recorded");
    }

    #[test]
    fn keys_move_fold_filter_search_and_leave_the_inspector_first() {
        let mut i = Inspector::new(Some(fixtures::THREAD_WEBHOOKS.into()), fixtures::data(NOW));
        assert_eq!(i.scope, Scope::Thread(fixtures::THREAD_WEBHOOKS.into()), "the current thread by default");
        assert_eq!(i.selected_request().unwrap().id, "R7_3", "the newest first");
        i.handle_key(key(KeyCode::Down));
        assert_eq!(i.selected_request().unwrap().id, "R7_2");
        i.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(i.selected, Some(Selection::Group(GroupKey::of(&i.data.requests[0]))), "folded: the turn is selected");
        assert_eq!(i.selected_request().unwrap().id, "R7_3", "and shows its newest request");
        i.handle_key(key(KeyCode::Down));
        assert_eq!(i.selected_request().unwrap().turn, Some(3), "past the folded turn in one step");
        i.handle_key(key(KeyCode::Enter));
        assert_eq!(i.focus, Focus::Inspector);
        assert_eq!(i.handle_key(key(KeyCode::Esc)), InspectAction::None, "Esc leaves the inspector first");
        assert_eq!(i.focus, Focus::List);
        assert_eq!(i.handle_key(key(KeyCode::Char('f'))), InspectAction::Reload);
        assert_eq!(i.scope, Scope::Agent(fixtures::ORION.into()), "then the selected request's agent");
        i.handle_key(key(KeyCode::Char('f')));
        assert_eq!(i.scope, Scope::All);
        i.handle_key(key(KeyCode::Char('f')));
        assert_eq!(i.scope, Scope::Thread(fixtures::THREAD_WEBHOOKS.into()), "and back home");
        i.handle_key(key(KeyCode::Char('/')));
        for c in "codex".chars() {
            i.handle_key(key(KeyCode::Char(c)));
        }
        i.handle_key(key(KeyCode::Enter));
        let view = i.list();
        assert_eq!(view.shown.len(), 3, "the model matches");
        assert!(i.selected_request().unwrap().model.as_deref().unwrap().contains("codex"), "the selection follows the search");
        assert_eq!(i.handle_key(key(KeyCode::Esc)), InspectAction::None, "Esc clears the search before it quits");
        assert_eq!(i.handle_key(key(KeyCode::Char('q'))), InspectAction::Quit);
    }

    #[test]
    fn a_refresh_keeps_the_selection_on_its_request() {
        let mut i = Inspector::new(None, fixtures::data(NOW));
        i.handle_key(key(KeyCode::Down));
        i.handle_key(key(KeyCode::Down));
        let id = i.selected_request().unwrap().id.clone();
        i.set_data(fixtures::data(NOW));
        assert_eq!(i.selected_request().unwrap().id, id);
        i.set_data(InspectData::default());
        assert_eq!(i.selected, None, "nothing left to select");
    }
}
