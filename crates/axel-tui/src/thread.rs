//! Thread view state machine. Pure: holds a [`ThreadData`] snapshot plus the
//! timeline cursor, fold state and composer; consumes [`KeyEvent`]s and
//! produces [`ThreadAction`]s. Rendering lives in `view::thread_view`.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use std::path::PathBuf;

use crate::attach::parse_dropped_paths;
use crate::input::TextInput;
use crate::model::{ItemBody, ItemKind, ItemStatus, RuntimeMode, ThreadData, ThreadItem};

/// Command sent to a managed session's host (mirrors `axel_acp::socket::Command`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendCommand {
    /// While a turn runs the host queues the prompt; `steer` puts it first and
    /// interrupts the running turn.
    Prompt { text: String, attachments: Vec<PathBuf>, steer: bool },
    /// Send a queued prompt now (interrupting the running turn).
    Steer { item_id: String },
    /// Withdraw a queued prompt.
    Dequeue { item_id: String },
    /// Switch the session's runtime mode.
    SetMode { mode: RuntimeMode },
    Cancel,
    Answer { item_id: String, option_id: String },
}

/// Reply from a managed session's host (mirrors `axel_acp::socket::Reply`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendReply {
    Ok,
    /// The prompt waits behind the running turn as this pending item.
    Queued { item_id: String },
    Status { state: String, turn: i64, pending: usize },
    Error { message: String },
}

/// A grouped timeline entry: one item, or a run of consecutive tool calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Item(usize),
    /// Indices into the items vec, in order.
    Fold(Vec<usize>),
}

/// Group items into timeline entries: `TurnEnd` is dropped and every run of
/// consecutive `ToolCall` items becomes one [`Entry::Fold`].
pub fn group_items(items: &[ThreadItem]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind == ItemKind::ToolCall {
            run.push(i);
            continue;
        }
        if !run.is_empty() {
            out.push(Entry::Fold(std::mem::take(&mut run)));
        }
        if it.kind != ItemKind::TurnEnd {
            out.push(Entry::Item(i));
        }
    }
    if !run.is_empty() {
        out.push(Entry::Fold(run));
    }
    out
}

/// What a visible timeline row is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// User/assistant/error/answer prose.
    Prose(usize),
    /// Permission/question card (pending or resolved).
    Choice(usize),
    /// Folded or expanded thought.
    Thought(usize),
    /// Collapsed run of tool calls.
    Fold(Vec<usize>),
    /// One tool call inside an expanded fold; `expanded` shows its output head.
    Tool { idx: usize, fold_first: usize },
}

impl Row {
    /// Stable identity used to keep the cursor across refreshes.
    pub fn key(&self, items: &[ThreadItem]) -> String {
        match self {
            Row::Prose(i) | Row::Choice(i) | Row::Thought(i) | Row::Tool { idx: i, .. } => items[*i].id.clone(),
            Row::Fold(v) => format!("fold:{}", items[v[0]].id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Timeline,
    Composer,
    /// Chip selection in the composer (only while attachments exist).
    Attachments,
    /// Selection in the queued-messages strip (only while messages are queued).
    Queue,
}

/// What the last [`ThreadAction::Send`] was, so its reply can be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
enum InFlight {
    Prompt { session_id: String, text: String, composer_text: String, attachments: Vec<PathBuf> },
    Dequeue { text: String },
    Other,
}

/// Whether `it` is a prompt waiting behind a running turn.
pub fn is_queued(it: &ThreadItem) -> bool {
    it.kind == ItemKind::UserMessage && it.status == Some(ItemStatus::Pending)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadAction {
    None,
    Quit,
    /// Esc with an empty composer: back to the dashboard.
    Back,
    OpenLog(String),
    Send { session_id: String, cmd: SendCommand },
    Summon { thread_id: String, agent: String, text: String, attachments: Vec<PathBuf> },
}

/// Where composer text goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dispatch {
    Prompt { session_id: String, text: String },
    Summon { agent: String, text: String },
    /// Nothing live and no agent has ever been on the thread.
    NoTarget,
}

/// Decide what `text` typed in the composer should do.
///
/// `@name …` goes to that agent's live **managed** session on this thread as a
/// prompt; when the agent has no such session it is summoned with it.  Either
/// way the text goes as typed, mention included, so the thread shows who was
/// asked (a bare `@name` summons it with nothing to do yet).  Without a
/// mention it goes to the live managed session whose agent replied last.
pub fn dispatch(text: &str, data: &ThreadData) -> Dispatch {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('@') {
        let (name, rest) = match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], rest[i..].trim()),
            None => (rest, ""),
        };
        if !name.is_empty() {
            // `@atlas@mini …` asks for atlas on host mini; a live atlas takes it either way.
            let agent = name.split_once('@').map_or(name, |(a, _)| a);
            if let Some(s) = data.live_sessions.iter().find(|s| s.managed && s.agent_name.eq_ignore_ascii_case(agent)) {
                return Dispatch::Prompt { session_id: s.id.clone(), text: text.to_string() };
            }
            let text = if rest.is_empty() { String::new() } else { text.to_string() };
            return Dispatch::Summon { agent: name.to_string(), text };
        }
    }
    if let Some(s) = reply_target(data) {
        return Dispatch::Prompt { session_id: s.id.clone(), text: text.to_string() };
    }
    match &data.last_agent_name {
        Some(agent) => Dispatch::Summon { agent: agent.clone(), text: text.to_string() },
        None => Dispatch::NoTarget,
    }
}

/// The live managed session a message without a mention goes to: the one
/// whose agent replied most recently, else the newest (the list is newest first).
pub fn reply_target(data: &ThreadData) -> Option<&crate::model::LiveSession> {
    data.live_sessions.iter().filter(|s| s.managed).fold(None, |best: Option<&crate::model::LiveSession>, s| match best {
        Some(b) if b.last_reply_at >= s.last_reply_at => Some(b),
        _ => Some(s),
    })
}

/// Who `text` would reach, as the composer shows it: the agent's name, with
/// `+` when it would be summoned onto the thread.
pub fn target_label(text: &str, data: &ThreadData) -> Option<String> {
    match dispatch(text, data) {
        Dispatch::Prompt { session_id, .. } => data.live_sessions.iter().find(|s| s.id == session_id).map(|s| s.agent_name.to_lowercase()),
        Dispatch::Summon { agent, .. } => Some(format!("+{}", agent.to_lowercase())),
        Dispatch::NoTarget => None,
    }
}

#[derive(Clone, Debug)]
pub struct ThreadView {
    pub thread_id: String,
    pub data: ThreadData,
    pub focus: Focus,
    /// Cursor over `rows()`.
    pub cursor: usize,
    /// Pin the cursor to the newest row while items arrive.
    pub follow: bool,
    /// Item ids of folds (first tool id) and thoughts that are expanded.
    pub expanded: HashSet<String>,
    /// Tool ids whose output head is shown.
    pub expanded_tools: HashSet<String>,
    /// Highlighted option per pending choice item id.
    pub choice_sel: HashMap<String, usize>,
    pub composer: TextInput,
    /// Files dropped onto the composer, sent with the next message.
    pub attachments: Vec<PathBuf>,
    /// Selected chip while `focus == Attachments`.
    pub attach_sel: usize,
    /// Dim status line under the composer (`sending…`, errors).
    pub status: Option<String>,
    /// Set while a send is in flight to block double submits.
    pub sending: bool,
    /// Selected row while `focus == Queue`.
    pub queue_sel: usize,
    /// Prompts sent from this view, oldest first (history before the store catches up).
    sent: Vec<String>,
    /// ↑/↓ history browsing: index into `history()` and the draft it replaced.
    hist_pos: Option<usize>,
    hist_draft: String,
    in_flight: Option<InFlight>,
    /// Composer-only pane (the timeline streams in the pane above): Tab skips
    /// the timeline and pending approvals are answered from the composer.
    pub compose_only: bool,
}

impl ThreadView {
    pub fn new(thread_id: &str, data: ThreadData) -> Self {
        let mut v = Self {
            thread_id: thread_id.to_string(),
            data,
            focus: Focus::Composer,
            cursor: 0,
            follow: true,
            expanded: HashSet::new(),
            expanded_tools: HashSet::new(),
            choice_sel: HashMap::new(),
            composer: TextInput::new(),
            attachments: Vec::new(),
            attach_sel: 0,
            status: None,
            sending: false,
            queue_sel: 0,
            sent: Vec::new(),
            hist_pos: None,
            hist_draft: String::new(),
            in_flight: None,
            compose_only: false,
        };
        v.cursor = v.rows().len().saturating_sub(1);
        v
    }

    /// Replace the snapshot. Keeps the cursor on the same row key unless following.
    pub fn set_data(&mut self, data: ThreadData) {
        let old_rows = self.rows();
        let key = old_rows.get(self.cursor).map(|r| r.key(&self.data.items));
        self.data = data;
        self.clamp_queue_sel();
        let rows = self.rows();
        if self.follow {
            self.cursor = rows.len().saturating_sub(1);
        } else {
            self.cursor = key
                .and_then(|k| rows.iter().position(|r| r.key(&self.data.items) == k))
                .unwrap_or_else(|| self.cursor.min(rows.len().saturating_sub(1)));
        }
    }

    /// Visible rows derived from the items and the fold state.
    pub fn rows(&self) -> Vec<Row> {
        let items = &self.data.items;
        let mut rows = Vec::new();
        for e in group_items(items) {
            match e {
                Entry::Item(i) => match items[i].kind {
                    ItemKind::Permission | ItemKind::Question => rows.push(Row::Choice(i)),
                    ItemKind::Thought => rows.push(Row::Thought(i)),
                    ItemKind::ToolCall | ItemKind::TurnEnd => {}
                    // Queued prompts live in the strip above the composer.
                    _ if is_queued(&items[i]) => {}
                    _ => rows.push(Row::Prose(i)),
                },
                Entry::Fold(v) => {
                    let first = v[0];
                    if self.expanded.contains(&items[first].id) {
                        for idx in v {
                            rows.push(Row::Tool { idx, fold_first: first });
                        }
                    } else {
                        rows.push(Row::Fold(v));
                    }
                }
            }
        }
        rows
    }

    /// Indices of queued prompts, oldest first.
    pub fn queued(&self) -> Vec<usize> {
        self.data.items.iter().enumerate().filter(|(_, it)| is_queued(it)).map(|(i, _)| i).collect()
    }

    fn clamp_queue_sel(&mut self) {
        let n = self.queued().len();
        self.queue_sel = self.queue_sel.min(n.saturating_sub(1));
        if n == 0 && self.focus == Focus::Queue {
            self.focus = Focus::Composer;
        }
    }

    /// Whether a permission/question is waiting for an answer.  Sending a message
    /// then declines it and redirects the agent (t3code's behaviour): the
    /// prompt goes out as a steer, and the host's cancel rejects the request.
    pub fn awaiting_answer(&self) -> bool {
        self.data.items.iter().any(|it| matches!(it.kind, ItemKind::Permission | ItemKind::Question) && it.status == Some(ItemStatus::Pending))
    }

    /// The first pending permission/question, as an item index.
    pub fn pending_choice(&self) -> Option<usize> {
        self.data
            .items
            .iter()
            .position(|it| matches!(it.kind, ItemKind::Permission | ItemKind::Question) && it.status == Some(ItemStatus::Pending))
    }

    /// Keys for the composer-only pane.  With an empty composer and a pending
    /// approval, ↑/↓ pick an option, Enter or a digit answers; typing still
    /// declines and redirects.  Everything else is the normal composer.
    pub fn handle_compose_key(&mut self, key: KeyEvent) -> ThreadAction {
        if self.focus == Focus::Timeline {
            self.focus = Focus::Composer;
        }
        if self.focus == Focus::Composer
            && self.composer.is_empty()
            && self.attachments.is_empty()
            && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
            && let Some(i) = self.pending_choice()
        {
            let n = match &self.data.items[i].body {
                ItemBody::Choice { options, .. } => options.len(),
                _ => 0,
            };
            let id = self.data.items[i].id.clone();
            let cur = self.choice_index(&id);
            match key.code {
                KeyCode::Up => {
                    self.choice_sel.insert(id, cur.saturating_sub(1));
                    return ThreadAction::None;
                }
                KeyCode::Down => {
                    self.choice_sel.insert(id, (cur + 1).min(n.saturating_sub(1)));
                    return ThreadAction::None;
                }
                KeyCode::Enter => return self.answer(i),
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let k = c as usize - '1' as usize;
                    if k < n {
                        self.choice_sel.insert(id, k);
                        return self.answer(i);
                    }
                    return ThreadAction::None;
                }
                _ => {}
            }
        }
        match key.code {
            // No timeline here: the pane above scrolls natively.
            KeyCode::PageUp | KeyCode::PageDown => ThreadAction::None,
            _ => self.handle_key(key),
        }
    }

    /// Whether an agent is actively working (not parked on a permission).
    pub fn is_working(&self) -> bool {
        self.data.agents.iter().any(|(_, _, st)| *st == crate::model::StatusState::Working)
    }

    /// Whether an agent on the thread is mid-turn (a send would be queued).
    pub fn is_busy(&self) -> bool {
        self.data.agents.iter().any(|(_, _, st)| matches!(st, crate::model::StatusState::Working | crate::model::StatusState::Blocked))
    }

    /// The owner's previous prompts on this thread, oldest first, without duplicates
    /// (the latest occurrence wins).
    pub fn history(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .data
            .items
            .iter()
            .filter(|it| it.kind == ItemKind::UserMessage && it.author.is_none() && it.agent.is_none())
            .filter_map(|it| match &it.body {
                ItemBody::Text(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
                _ => None,
            })
            .collect();
        all.extend(self.sent.iter().cloned());
        let mut out: Vec<String> = Vec::with_capacity(all.len());
        for t in all {
            out.retain(|o| *o != t);
            out.push(t);
        }
        out
    }

    /// ↑ (older) / ↓ (newer) through `history()`; past the newest restores the draft.
    fn history_step(&mut self, older: bool) {
        let hist = self.history();
        if hist.is_empty() {
            return;
        }
        let next = match (self.hist_pos, older) {
            (None, true) => {
                self.hist_draft = self.composer.text();
                Some(hist.len() - 1)
            }
            (None, false) => return,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) if p + 1 < hist.len() => Some(p + 1),
            (Some(_), false) => None,
        };
        self.hist_pos = next;
        match next {
            Some(p) => self.composer.set_text(&hist[p]),
            None => {
                let draft = std::mem::take(&mut self.hist_draft);
                self.composer.set_text(&draft);
            }
        }
    }

    pub fn current_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.cursor)
    }

    pub fn is_expanded(&self, item_id: &str) -> bool {
        self.expanded.contains(item_id)
    }

    pub fn is_tool_expanded(&self, item_id: &str) -> bool {
        self.expanded_tools.contains(item_id)
    }

    /// Highlighted option index of a choice item.
    pub fn choice_index(&self, item_id: &str) -> usize {
        self.choice_sel.get(item_id).copied().unwrap_or(0)
    }

    /// Whether any session of the thread is live (drives the poll rate).
    pub fn is_live(&self) -> bool {
        !self.data.live_sessions.is_empty()
    }

    /// Session to open the raw log of: the item under the cursor, else the latest live one.
    pub fn log_target(&self) -> Option<String> {
        let from_cursor = self.current_row().map(|r| match r {
            Row::Prose(i) | Row::Choice(i) | Row::Thought(i) | Row::Tool { idx: i, .. } => self.data.items[i].session_id.clone(),
            Row::Fold(v) => self.data.items[v[0]].session_id.clone(),
        });
        from_cursor.filter(|s| !s.is_empty()).or_else(|| self.data.live_sessions.first().map(|s| s.id.clone()))
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
    }

    /// Called by the app with the host's reply to the last send.
    pub fn on_reply(&mut self, reply: Result<SendReply, String>) {
        self.sending = false;
        let in_flight = self.in_flight.take();
        let failed = matches!(reply, Err(_) | Ok(SendReply::Error { .. }));
        match (&in_flight, &reply) {
            // Show the queued prompt right away; the next refresh brings the stored item.
            (Some(InFlight::Prompt { session_id, text, attachments, .. }), Ok(SendReply::Queued { item_id }))
                if !self.data.items.iter().any(|it| it.id == *item_id) => {
                    self.data.items.push(ThreadItem {
                        id: item_id.clone(),
                        session_id: session_id.clone(),
                        agent: None,
                        author: None,
                        seq: i64::MAX,
                        turn: 0,
                        at_ms: self.data.items.last().map(|l| l.at_ms).unwrap_or(0),
                        kind: ItemKind::UserMessage,
                        status: Some(ItemStatus::Pending),
                        body: ItemBody::Text(text.clone()),
                        attachments: attachments.clone(),
                        relay_to: None,
                    });
                }
            // A failed send gives the text back instead of losing it.
            (Some(InFlight::Prompt { composer_text, attachments, .. }), _) if failed => {
                if self.composer.is_empty() {
                    self.composer.set_text(composer_text);
                }
                if self.attachments.is_empty() {
                    self.attachments = attachments.clone();
                }
            }
            _ => {}
        }
        self.status = match reply {
            Ok(SendReply::Ok) => None,
            Ok(SendReply::Queued { .. }) => Some(if self.queued().len() > 1 { format!("queued ({})", self.queued().len()) } else { "queued · runs when the turn ends · ^S steer".into() }),
            Ok(SendReply::Status { state, turn, pending }) => Some(format!("{state} · turn {turn} · {pending} pending")),
            Ok(SendReply::Error { message }) => Some(match in_flight {
                Some(InFlight::Dequeue { .. }) => format!("error: {message} · text kept in the composer"),
                _ => format!("error: {message}"),
            }),
            Err(e) => Some(format!("error: {e}")),
        };
        self.clamp_queue_sel();
    }

    pub fn handle_paste(&mut self, text: &str) {
        match parse_dropped_paths(text) {
            Some(paths) => {
                for p in paths {
                    if !self.attachments.contains(&p) {
                        self.attachments.push(p);
                    }
                }
            }
            None => self.composer.insert_str(text),
        }
        self.focus = Focus::Composer;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThreadAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.composer.is_empty() {
                return ThreadAction::Quit;
            }
            self.composer.clear();
            return ThreadAction::None;
        }
        // Keys shared by both focuses.
        match key.code {
            // In the composer ↑/↓ browse prompt history; the timeline scrolls with PgUp/PgDn.
            KeyCode::Up if self.focus == Focus::Composer => {
                self.history_step(true);
                return ThreadAction::None;
            }
            KeyCode::Down if self.focus == Focus::Composer => {
                self.history_step(false);
                return ThreadAction::None;
            }
            KeyCode::Up if self.focus != Focus::Queue => return self.move_cursor(-1),
            KeyCode::Down if self.focus != Focus::Queue => return self.move_cursor(1),
            KeyCode::PageUp => return self.move_cursor(-10),
            KeyCode::PageDown => return self.move_cursor(10),
            // Shift+Tab cycles the live session's runtime mode (Claude Code's binding).
            KeyCode::BackTab => return self.cycle_mode(),
            KeyCode::Tab => {
                // Composer → attachments → queue → timeline, skipping empty stops.
                let order = [Focus::Composer, Focus::Attachments, Focus::Queue, Focus::Timeline];
                let has_queue = !self.queued().is_empty();
                let compose_only = self.compose_only;
                let available = |f: Focus| match f {
                    Focus::Attachments => !self.attachments.is_empty(),
                    Focus::Queue => has_queue,
                    Focus::Timeline => !compose_only,
                    Focus::Composer => true,
                };
                let cur = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let mut i = cur;
                loop {
                    i = (i + 1) % order.len();
                    if available(order[i]) {
                        break;
                    }
                }
                self.focus = order[i];
                self.attach_sel = self.attach_sel.min(self.attachments.len().saturating_sub(1));
                self.clamp_queue_sel();
                return ThreadAction::None;
            }
            KeyCode::Char(' ') if shift => {
                self.fold_all();
                return ThreadAction::None;
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => self.key_composer(key),
            Focus::Attachments => self.key_attachments(key),
            Focus::Queue => self.key_queue(key),
            Focus::Timeline => self.key_timeline(key),
        }
    }

    fn key_queue(&mut self, key: KeyEvent) -> ThreadAction {
        let q = self.queued();
        if q.is_empty() {
            self.focus = Focus::Composer;
            return ThreadAction::None;
        }
        let sel = self.queue_sel.min(q.len() - 1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.queue_sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.queue_sel = (sel + 1).min(q.len() - 1),
            KeyCode::Enter | KeyCode::Char('s') => return self.steer_queued(q[sel]),
            KeyCode::Char('e') => return self.edit_queued(q[sel]),
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('x') => return self.dequeue(q[sel], false),
            KeyCode::Esc => self.focus = Focus::Composer,
            KeyCode::Char(_) => {
                self.focus = Focus::Composer;
                self.composer.handle_key(key);
            }
            _ => {}
        }
        ThreadAction::None
    }

    /// The managed session whose mode the prompt shows and Shift+Tab changes
    /// (the newest live one).
    /// The session the composer talks to (and Shift+Tab switches the mode of).
    pub fn mode_session(&self) -> Option<&crate::model::LiveSession> {
        reply_target(&self.data)
    }

    /// Who the composer's text would reach (see [`target_label`]).
    pub fn target(&self) -> Option<String> {
        target_label(&self.composer.text(), &self.data)
    }

    /// Shift+Tab: next runtime mode, in Claude Code's order
    /// (ask → accept edits → plan → autopilot → ask).
    fn cycle_mode(&mut self) -> ThreadAction {
        if self.sending {
            return ThreadAction::None;
        }
        let Some(i) = self.data.live_sessions.iter().position(|s| s.managed) else {
            self.set_status("no managed session to switch modes on");
            return ThreadAction::None;
        };
        let s = &mut self.data.live_sessions[i];
        let next = match s.mode {
            RuntimeMode::Supervised => RuntimeMode::Edits,
            RuntimeMode::Edits => RuntimeMode::Plan,
            RuntimeMode::Plan => RuntimeMode::Autopilot,
            RuntimeMode::Autopilot => RuntimeMode::Supervised,
        };
        // Optimistic: the next refresh brings the stored mode.
        s.mode = next;
        let session_id = s.id.clone();
        self.sending = true;
        self.in_flight = Some(InFlight::Other);
        self.status = None;
        ThreadAction::Send { session_id, cmd: SendCommand::SetMode { mode: next } }
    }

    /// Mouse wheel: scroll the timeline by `delta` rows, whatever has focus.
    /// (Without mouse capture the terminal sends the wheel as ↑/↓, which the
    /// composer reads as history.)
    pub fn scroll(&mut self, delta: i32) {
        let n = self.rows().len();
        if n == 0 {
            return;
        }
        let last = n as i64 - 1;
        let next = (self.cursor as i64 + delta as i64).clamp(0, last);
        self.cursor = next as usize;
        self.follow = next == last;
    }

    /// Interrupt the running turn and send queued item `i` now.
    fn steer_queued(&mut self, i: usize) -> ThreadAction {
        if self.sending {
            return ThreadAction::None;
        }
        let it = &self.data.items[i];
        let (session_id, item_id) = (it.session_id.clone(), it.id.clone());
        self.sending = true;
        self.in_flight = Some(InFlight::Other);
        self.follow = true;
        self.set_status("steering…");
        ThreadAction::Send { session_id, cmd: SendCommand::Steer { item_id } }
    }

    /// Withdraw queued item `i`; with `edit`, its text (and files) go back to the composer.
    fn dequeue(&mut self, i: usize, edit: bool) -> ThreadAction {
        if self.sending {
            return ThreadAction::None;
        }
        let it = self.data.items.remove(i);
        let text = match &it.body {
            ItemBody::Text(t) => t.clone(),
            _ => String::new(),
        };
        if edit {
            self.composer.set_text(&text);
            for p in &it.attachments {
                if !self.attachments.contains(p) {
                    self.attachments.push(p.clone());
                }
            }
            self.focus = Focus::Composer;
        }
        self.clamp_queue_sel();
        self.sending = true;
        self.in_flight = Some(InFlight::Dequeue { text });
        self.set_status(if edit { "editing queued message…" } else { "removing…" });
        ThreadAction::Send { session_id: it.session_id, cmd: SendCommand::Dequeue { item_id: it.id } }
    }

    fn edit_queued(&mut self, i: usize) -> ThreadAction {
        if !self.composer.is_empty() {
            self.set_status("composer not empty · send or clear it first");
            return ThreadAction::None;
        }
        self.dequeue(i, true)
    }

    /// Remove chip `i`; the selection moves to its neighbour.
    pub fn remove_attachment(&mut self, i: usize) {
        if i < self.attachments.len() {
            self.attachments.remove(i);
        }
        if self.attachments.is_empty() {
            self.attach_sel = 0;
            if self.focus == Focus::Attachments {
                self.focus = Focus::Composer;
            }
        } else {
            self.attach_sel = self.attach_sel.min(self.attachments.len() - 1);
        }
    }

    fn key_attachments(&mut self, key: KeyEvent) -> ThreadAction {
        let n = self.attachments.len();
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => self.attach_sel = self.attach_sel.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.attach_sel = (self.attach_sel + 1).min(n.saturating_sub(1)),
            KeyCode::Home => self.attach_sel = 0,
            KeyCode::End => self.attach_sel = n.saturating_sub(1),
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('x') => self.remove_attachment(self.attach_sel),
            KeyCode::Esc => self.focus = Focus::Composer,
            KeyCode::Enter => return self.submit(false),
            KeyCode::Char(_) => {
                self.focus = Focus::Composer;
                self.composer.handle_key(key);
            }
            _ => {}
        }
        ThreadAction::None
    }

    fn key_composer(&mut self, key: KeyEvent) -> ThreadAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            // Editing a recalled prompt makes it the new draft.
            self.hist_pos = None;
        }
        match key.code {
            KeyCode::Enter => self.submit(false),
            // ^S: send now, interrupting the running turn; with an empty composer,
            // steer the oldest queued prompt.
            KeyCode::Char('s') if ctrl => {
                if self.composer.is_empty() && self.attachments.is_empty() {
                    match self.queued().first() {
                        Some(&i) => self.steer_queued(i),
                        None => ThreadAction::None,
                    }
                } else {
                    self.submit(true)
                }
            }
            KeyCode::Esc => {
                if self.composer.is_empty() {
                    ThreadAction::Back
                } else {
                    self.focus = Focus::Timeline;
                    ThreadAction::None
                }
            }
            // Space/→ on a fold/thought toggle it when nothing is being typed.
            KeyCode::Char(' ') | KeyCode::Right if self.composer.is_empty() && self.toggle_under_cursor() => ThreadAction::None,
            // Backspace on an empty composer removes the last attachment chip.
            KeyCode::Backspace if self.composer.is_empty() && key.modifiers.is_empty() => {
                let last = self.attachments.len().saturating_sub(1);
                self.remove_attachment(last);
                ThreadAction::None
            }
            _ => {
                self.composer.handle_key(key);
                ThreadAction::None
            }
        }
    }

    fn key_timeline(&mut self, key: KeyEvent) -> ThreadAction {
        match key.code {
            KeyCode::Esc => ThreadAction::Back,
            KeyCode::Char('q') => ThreadAction::Quit,
            KeyCode::Char('g') => {
                self.cursor = 0;
                self.follow = false;
                ThreadAction::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.cursor = self.rows().len().saturating_sub(1);
                self.follow = true;
                ThreadAction::None
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.follow = false;
                ThreadAction::None
            }
            KeyCode::Char('l') => match self.log_target() {
                Some(sid) => ThreadAction::OpenLog(sid),
                None => {
                    self.set_status("no session to show a log for");
                    ThreadAction::None
                }
            },
            KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Char(' ') | KeyCode::Right => {
                self.toggle_under_cursor();
                ThreadAction::None
            }
            KeyCode::Left => {
                self.collapse_under_cursor();
                ThreadAction::None
            }
            KeyCode::Enter => match self.current_row() {
                Some(Row::Choice(i)) => self.answer(i),
                _ => {
                    self.toggle_under_cursor();
                    ThreadAction::None
                }
            },
            KeyCode::Char(_) | KeyCode::Backspace => {
                // Any other printable key starts typing.
                self.focus = Focus::Composer;
                self.composer.handle_key(key);
                ThreadAction::None
            }
            _ => ThreadAction::None,
        }
    }

    /// ↑/↓: on a pending choice card, cycle its options first; at the edges move on.
    fn move_cursor(&mut self, delta: i32) -> ThreadAction {
        let rows = self.rows();
        if rows.is_empty() {
            return ThreadAction::None;
        }
        if let Some(Row::Choice(i)) = rows.get(self.cursor) {
            let it = &self.data.items[*i];
            if it.status == Some(ItemStatus::Pending)
                && let ItemBody::Choice { options, .. } = &it.body
                && !options.is_empty()
            {
                let cur = self.choice_index(&it.id);
                let n = options.len();
                if delta < 0 && cur > 0 {
                    self.choice_sel.insert(it.id.clone(), cur - 1);
                    return ThreadAction::None;
                }
                if delta > 0 && cur + 1 < n {
                    self.choice_sel.insert(it.id.clone(), cur + 1);
                    return ThreadAction::None;
                }
            }
        }
        let last = rows.len() as i64 - 1;
        let next = (self.cursor as i64 + delta as i64).clamp(0, last);
        self.cursor = next as usize;
        self.follow = next == last;
        ThreadAction::None
    }

    /// Returns `true` when the row under the cursor was a fold/thought/tool and toggled.
    fn toggle_under_cursor(&mut self) -> bool {
        match self.current_row() {
            Some(Row::Fold(v)) => {
                let id = self.data.items[v[0]].id.clone();
                self.expanded.insert(id);
                true
            }
            Some(Row::Thought(i)) => {
                let id = self.data.items[i].id.clone();
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                true
            }
            Some(Row::Tool { idx, .. }) => {
                let id = self.data.items[idx].id.clone();
                if !self.expanded_tools.remove(&id) {
                    self.expanded_tools.insert(id);
                }
                true
            }
            _ => false,
        }
    }

    /// ←: collapse the tool output, else the whole fold the cursor is in.
    fn collapse_under_cursor(&mut self) {
        match self.current_row() {
            Some(Row::Tool { idx, fold_first }) => {
                let tool_id = self.data.items[idx].id.clone();
                if !self.expanded_tools.remove(&tool_id) {
                    self.expanded.remove(&self.data.items[fold_first].id);
                    // Put the cursor on the collapsed fold row.
                    let rows = self.rows();
                    if let Some(p) = rows.iter().position(|r| matches!(r, Row::Fold(v) if v[0] == fold_first)) {
                        self.cursor = p;
                    }
                }
            }
            Some(Row::Thought(i)) => {
                self.expanded.remove(&self.data.items[i].id);
            }
            _ => {}
        }
    }

    /// Shift+Space: collapse everything, keeping the cursor on its fold.
    pub fn fold_all(&mut self) {
        let anchor = match self.current_row() {
            Some(Row::Tool { fold_first, .. }) => Some(fold_first),
            _ => None,
        };
        let key = anchor.map(|f| format!("fold:{}", self.data.items[f].id)).or_else(|| self.current_row().map(|r| r.key(&self.data.items)));
        self.expanded.clear();
        self.expanded_tools.clear();
        let rows = self.rows();
        if let Some(k) = key
            && let Some(p) = rows.iter().position(|r| r.key(&self.data.items) == k)
        {
            self.cursor = p;
        } else {
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        }
    }

    /// Enter on a pending choice: answer with the highlighted option.
    fn answer(&mut self, i: usize) -> ThreadAction {
        let it = &self.data.items[i];
        if it.status != Some(ItemStatus::Pending) || self.sending {
            return ThreadAction::None;
        }
        let ItemBody::Choice { options, .. } = &it.body else { return ThreadAction::None };
        let Some(opt) = options.get(self.choice_index(&it.id)).cloned() else { return ThreadAction::None };
        let (session_id, item_id) = (it.session_id.clone(), it.id.clone());
        self.sending = true;
        self.set_status(format!("answering {}…", opt.label));
        ThreadAction::Send { session_id, cmd: SendCommand::Answer { item_id, option_id: opt.id } }
    }

    /// Enter (or ^S with `steer`) in the composer.
    fn submit(&mut self, steer: bool) -> ThreadAction {
        let composer_text = self.composer.text();
        if (composer_text.trim().is_empty() && self.attachments.is_empty()) || self.sending {
            return ThreadAction::None;
        }
        let remember = |v: &mut Self| {
            let t = composer_text.trim().to_string();
            if !t.is_empty() {
                v.sent.retain(|s| *s != t);
                v.sent.push(t);
            }
            v.hist_pos = None;
            v.hist_draft.clear();
        };
        match dispatch(&composer_text, &self.data) {
            Dispatch::Prompt { session_id, text } => {
                let redirect = self.awaiting_answer();
                let steer = steer || redirect;
                remember(self);
                self.composer.clear();
                let attachments = std::mem::take(&mut self.attachments);
                self.sending = true;
                self.set_status(if redirect {
                    "declining · redirecting…"
                } else if steer {
                    "steering…"
                } else {
                    "sending…"
                });
                self.follow = true;
                self.in_flight = Some(InFlight::Prompt {
                    session_id: session_id.clone(),
                    text: text.clone(),
                    composer_text: composer_text.clone(),
                    attachments: attachments.clone(),
                });
                ThreadAction::Send { session_id, cmd: SendCommand::Prompt { text, attachments, steer } }
            }
            Dispatch::Summon { agent, text } => {
                remember(self);
                self.composer.clear();
                let attachments = std::mem::take(&mut self.attachments);
                self.in_flight = Some(InFlight::Prompt {
                    session_id: String::new(),
                    text: text.clone(),
                    composer_text: composer_text.clone(),
                    attachments: attachments.clone(),
                });
                self.sending = true;
                self.set_status(format!("summoning @{agent}…"));
                self.follow = true;
                ThreadAction::Summon { thread_id: self.thread_id.clone(), agent, text, attachments }
            }
            Dispatch::NoTarget => {
                self.set_status("no agent on this thread yet · start with @name");
                ThreadAction::None
            }
        }
    }

    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        match self.focus {
            Focus::Composer => {
                let queued = !self.queued().is_empty();
                let enter = if self.awaiting_answer() {
                    "decline & redirect"
                } else if self.is_busy() {
                    "queue"
                } else {
                    "send"
                };
                let mut v = vec![("Enter", enter)];
                if self.is_busy() || queued {
                    v.push(("^S", "steer"));
                }
                if self.compose_only && self.pending_choice().is_some() && self.composer.is_empty() {
                    v[0] = ("Enter", "answer");
                    v.insert(1, ("1-9", "pick"));
                    v.insert(2, ("↑↓", "choose"));
                } else {
                    v.push(("↑↓", "history"));
                }
                if !self.compose_only {
                    v.push(("PgUp", "scroll"));
                }
                if !self.attachments.is_empty() {
                    v.push(("Tab", "attachments"));
                } else if queued {
                    v.push(("Tab", "queue"));
                } else if !self.compose_only {
                    v.push(("Tab", "timeline keys"));
                }
                v.push(("Esc", if self.compose_only { "close" } else { "dashboard" }));
                v
            }
            Focus::Attachments => vec![("←→", "select"), ("⌫", "remove"), ("Esc", "back")],
            Focus::Queue => vec![("↑↓", "select"), ("Enter", "steer now"), ("e", "edit"), ("x", "remove"), ("Esc", "back")],
            Focus::Timeline => vec![
                ("Space", "expand"),
                ("⇧Space", "fold all"),
                ("Enter", "answer"),
                ("l", "log"),
                ("g/G", "top/bottom"),
                ("Esc", "dashboard"),
            ],
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::model::{ChoiceOption, LiveSession};
    use axel_store::AgentColor;

    pub fn item(id: &str, kind: ItemKind, body: ItemBody, at_ms: i64) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            session_id: "S1".into(),
            agent: Some(("Atlas".into(), Some(AgentColor::Purple))),
            author: None,
            seq: at_ms,
            turn: 1,
            at_ms,
            kind,
            status: Some(ItemStatus::Completed),
            body,
            attachments: Vec::new(),
            relay_to: None,
        }
    }

    pub fn tool(id: &str, title: &str, kind: &str, at_ms: i64) -> ThreadItem {
        item(
            id,
            ItemKind::ToolCall,
            ItemBody::Tool {
                title: title.into(),
                kind: Some(kind.into()),
                input: format!("{{\"path\":\"{title}\"}}"),
                output: (1..=20).map(|i| format!("line {i}")).collect(),
            },
            at_ms,
        )
    }

    /// Two prose items, five tool calls, one pending permission.
    pub fn items() -> Vec<ThreadItem> {
        let mut user = item("u1", ItemKind::UserMessage, ItemBody::Text("Fix the login bug".into()), 1_000);
        user.agent = None;
        let mut v = vec![
            user,
            item("a1", ItemKind::AssistantMessage, ItemBody::Text("\n\nLooking at the auth module first.\n\n\n\n".into()), 2_000),
            tool("t1", "Read src/auth.rs", "read", 3_000),
            tool("t2", "Read src/login.rs", "read", 3_400),
            tool("t3", "Grep session_token", "search", 3_900),
            tool("t4", "Edit src/login.rs", "edit", 4_800),
            tool("t5", "Run cargo test", "execute", 6_200),
            item(
                "p1",
                ItemKind::Permission,
                ItemBody::Choice {
                    prompt: "Run cargo test".into(),
                    options: vec![
                        ChoiceOption { id: "allow_once".into(), label: "Allow once".into() },
                        ChoiceOption { id: "allow_always".into(), label: "Always allow".into() },
                        ChoiceOption { id: "reject".into(), label: "Reject".into() },
                    ],
                    chosen: None,
                },
                6_300,
            ),
        ];
        v[7].status = Some(ItemStatus::Pending);
        v
    }

    pub fn data() -> ThreadData {
        ThreadData {
            thread_id: "T1".into(),
            title: "Fix login".into(),
            agents: vec![("Atlas".into(), Some(AgentColor::Purple), axel_store::StatusState::Blocked)],
            environments: vec!["macbook".into()],
            known_agents: Vec::new(),
            tokens: 12_400,
            cost_usd: 0.42,
            items: items(),
            live_sessions: vec![LiveSession { id: "S1".into(), agent_name: "Atlas".into(), managed: true, started_at: 1, mode: RuntimeMode::Autopilot, host: None, last_reply_at: None }],
            last_agent_name: Some("Atlas".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn shift_space() -> KeyEvent {
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::SHIFT)
    }
    fn type_str(v: &mut ThreadView, s: &str) {
        for c in s.chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn group_items_folds_consecutive_tool_calls_and_drops_turn_end() {
        let mut items = items();
        items.push(item("e1", ItemKind::TurnEnd, ItemBody::Empty, 7_000));
        items.push(tool("t6", "Read x", "read", 8_000));
        items.push(item("a2", ItemKind::AssistantMessage, ItemBody::Text("Done.".into()), 9_000));
        items.push(tool("t7", "Read y", "read", 9_500));
        let g = group_items(&items);
        assert_eq!(
            g,
            vec![
                Entry::Item(0),
                Entry::Item(1),
                Entry::Fold(vec![2, 3, 4, 5, 6]),
                Entry::Item(7),
                // TurnEnd dropped; t6 after it is its own fold because the permission broke the run
                Entry::Fold(vec![9]),
                Entry::Item(10),
                Entry::Fold(vec![11]),
            ]
        );
        assert!(group_items(&[]).is_empty());
    }

    #[test]
    fn rows_collapse_folds_by_default_and_follow_bottom() {
        let v = ThreadView::new("T1", data());
        let rows = v.rows();
        assert_eq!(rows.len(), 4, "user, assistant, fold, permission: {rows:?}");
        assert!(matches!(rows[2], Row::Fold(ref ids) if ids.len() == 5));
        assert!(matches!(rows[3], Row::Choice(7)));
        assert_eq!(v.cursor, 3, "starts on the newest row");
        assert!(v.follow);
        assert_eq!(v.focus, Focus::Composer);
    }

    #[test]
    fn expand_collapse_state_machine() {
        let mut v = ThreadView::new("T1", data());
        v.handle_key(key(KeyCode::Tab)); // timeline focus
        v.handle_key(key(KeyCode::Up)); // from permission (option 0, ↑ at first option moves) to fold
        assert!(matches!(v.current_row(), Some(Row::Fold(_))));
        assert!(!v.follow);
        v.handle_key(key(KeyCode::Char(' ')));
        let rows = v.rows();
        assert_eq!(rows.len(), 8, "fold expanded into 5 tool rows");
        assert!(matches!(v.current_row(), Some(Row::Tool { idx: 2, fold_first: 2 })));
        assert!(v.is_expanded("t1"));
        // Space on a tool shows its output; again hides it.
        v.handle_key(key(KeyCode::Char(' ')));
        assert!(v.is_tool_expanded("t1"));
        v.handle_key(key(KeyCode::Right));
        assert!(!v.is_tool_expanded("t1"));
        // ← inside the fold collapses it and lands on the fold row.
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Left));
        assert!(matches!(v.current_row(), Some(Row::Fold(_))));
        assert_eq!(v.rows().len(), 4);
        // Expand again, open an output, then Shift+Space folds everything.
        v.handle_key(key(KeyCode::Enter));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char(' ')));
        assert!(v.is_tool_expanded("t2"));
        v.handle_key(shift_space());
        assert!(v.expanded.is_empty() && v.expanded_tools.is_empty());
        assert!(matches!(v.current_row(), Some(Row::Fold(_))), "cursor back on the fold");
        // g / G
        v.handle_key(key(KeyCode::Char('G')));
        assert_eq!(v.cursor, 3);
        assert!(v.follow);
        v.handle_key(key(KeyCode::Char('g')));
        assert_eq!(v.cursor, 0);
        assert!(!v.follow);
    }

    #[test]
    fn thought_toggles_and_space_in_composer_only_when_empty() {
        let mut d = data();
        d.items.insert(2, item("th1", ItemKind::Thought, ItemBody::Text("hmm".into()), 2_500));
        let mut v = ThreadView::new("T1", d);
        v.handle_key(key(KeyCode::Tab)); // timeline focus
        v.handle_key(key(KeyCode::Up));
        v.handle_key(key(KeyCode::Up));
        assert!(matches!(v.current_row(), Some(Row::Thought(2))));
        v.handle_key(key(KeyCode::Tab)); // back to the composer
        // Space with an empty composer toggles the thought instead of typing.
        v.handle_key(key(KeyCode::Char(' ')));
        assert!(v.is_expanded("th1"));
        assert!(v.composer.is_empty());
        // With text in the composer, Space is just a space.
        type_str(&mut v, "hi");
        v.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(v.composer.text(), "hi ");
        assert!(v.is_expanded("th1"), "unchanged");
    }

    #[test]
    fn inline_answer_selection_and_send() {
        let mut v = ThreadView::new("T1", data());
        assert!(matches!(v.current_row(), Some(Row::Choice(7))));
        // ↓ cycles options on a pending card (timeline focus), clamped at the end.
        v.handle_key(key(KeyCode::Tab));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.choice_index("p1"), 2);
        assert_eq!(v.cursor, 3, "stays on the card at the last option");
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.choice_index("p1"), 1);
        // Enter answers from timeline focus.
        let a = v.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            ThreadAction::Send {
                session_id: "S1".into(),
                cmd: SendCommand::Answer { item_id: "p1".into(), option_id: "allow_always".into() }
            }
        );
        assert!(v.sending);
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ThreadAction::None, "blocked while sending");
        v.on_reply(Ok(SendReply::Ok));
        assert!(!v.sending && v.status.is_none());
        // Once answered, Enter does nothing.
        v.data.items[7].status = Some(ItemStatus::Allowed);
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ThreadAction::None);
        v.on_reply(Ok(SendReply::Error { message: "socket closed".into() }));
        assert_eq!(v.status.as_deref(), Some("error: socket closed"));
    }

    #[test]
    fn composer_dispatch_rules() {
        let d = data();
        // nova has no live session on this thread -> summon it with the text as typed
        assert_eq!(
            dispatch("@nova please review", &d),
            Dispatch::Summon { agent: "nova".into(), text: "@nova please review".into() },
            "the mention stays, so the thread shows who was asked"
        );
        assert_eq!(dispatch("@nova", &d), Dispatch::Summon { agent: "nova".into(), text: "".into() }, "a bare mention brings it in idle");
        assert_eq!(
            dispatch("@nova@mini fix the flaky test", &d),
            Dispatch::Summon { agent: "nova@mini".into(), text: "@nova@mini fix the flaky test".into() },
            "the host travels with the name"
        );
        // atlas has a live managed session -> prompt it with the full text as typed (case-insensitive name)
        assert_eq!(
            dispatch("@atlas please review", &d),
            Dispatch::Prompt { session_id: "S1".into(), text: "@atlas please review".into() }
        );
        // a live *pane* session for the mentioned agent cannot take prompts -> summon
        let mut pane = data();
        pane.live_sessions[0].managed = false;
        assert_eq!(
            dispatch("@Atlas go", &pane),
            Dispatch::Summon { agent: "Atlas".into(), text: "@Atlas go".into() }
        );
        assert_eq!(dispatch("  run the tests ", &d), Dispatch::Prompt { session_id: "S1".into(), text: "run the tests".into() });
        let mut no_live = data();
        no_live.live_sessions.clear();
        assert_eq!(dispatch("continue", &no_live), Dispatch::Summon { agent: "Atlas".into(), text: "continue".into() });
        let mut pane_only = data();
        pane_only.live_sessions[0].managed = false;
        assert_eq!(dispatch("hi", &pane_only), Dispatch::Summon { agent: "Atlas".into(), text: "hi".into() }, "pane sessions cannot take prompts");
        let mut empty = data();
        empty.live_sessions.clear();
        empty.last_agent_name = None;
        assert_eq!(dispatch("hi", &empty), Dispatch::NoTarget);
        assert_eq!(dispatch("@ hi", &empty), Dispatch::NoTarget, "bare @ is not a mention");
    }

    #[test]
    fn a_message_without_a_mention_goes_to_whoever_replied_last() {
        let live = |id: &str, name: &str, reply: Option<i64>| crate::model::LiveSession {
            id: id.into(),
            agent_name: name.into(),
            managed: true,
            started_at: 1,
            mode: RuntimeMode::Autopilot,
            host: None,
            last_reply_at: reply,
        };
        let mut d = data();
        // Newest first: nova joined after atlas, but atlas spoke last.
        d.live_sessions = vec![live("S2", "Nova", Some(100)), live("S1", "Atlas", Some(200))];
        assert_eq!(dispatch("what do you think?", &d), Dispatch::Prompt { session_id: "S1".into(), text: "what do you think?".into() });
        assert_eq!(target_label("what do you think?", &d).as_deref(), Some("atlas"));
        assert_eq!(target_label("@nova and you?", &d).as_deref(), Some("nova"), "a mention picks its agent");
        assert_eq!(target_label("@vega join us", &d).as_deref(), Some("+vega"), "+ means it would be summoned");
        // Nobody has replied yet: the newest session.
        d.live_sessions = vec![live("S2", "Nova", None), live("S1", "Atlas", None)];
        assert_eq!(dispatch("go", &d), Dispatch::Prompt { session_id: "S2".into(), text: "go".into() });
        let v = ThreadView::new("T1", d);
        assert_eq!(v.mode_session().map(|s| s.id.as_str()), Some("S2"), "Shift+Tab acts on the composer's target");
        assert_eq!(v.target().as_deref(), Some("nova"));
    }

    #[test]
    fn composer_submit_actions_and_escape() {
        let mut v = ThreadView::new("T1", data());
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ThreadAction::None, "empty composer sends nothing");
        type_str(&mut v, "run the tests");
        // The fixture's permission is pending: sending declines it and redirects (a steer).
        assert!(v.awaiting_answer());
        assert_eq!(v.help()[0], ("Enter", "decline & redirect"));
        assert_eq!(
            v.handle_key(key(KeyCode::Enter)),
            ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Prompt { text: "run the tests".into(), attachments: vec![], steer: true } }
        );
        assert!(v.composer.is_empty() && v.sending);
        assert_eq!(v.status.as_deref(), Some("declining · redirecting…"));
        v.on_reply(Ok(SendReply::Ok));
        type_str(&mut v, "@nova take over");
        assert_eq!(
            v.handle_key(key(KeyCode::Enter)),
            ThreadAction::Summon { thread_id: "T1".into(), agent: "nova".into(), text: "@nova take over".into(), attachments: vec![] }
        );
        v.on_reply(Err("spawn failed".into()));
        assert_eq!(v.status.as_deref(), Some("error: spawn failed"));
        // Ctrl+C with text clears; Esc with text goes to timeline focus; Esc empty → Back; Ctrl+C empty → Quit.
        type_str(&mut v, "draft");
        v.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(v.composer.is_empty());
        type_str(&mut v, "draft");
        assert_eq!(v.handle_key(key(KeyCode::Esc)), ThreadAction::None);
        assert_eq!(v.focus, Focus::Timeline);
        assert_eq!(v.composer.text(), "draft", "text kept");
        assert_eq!(v.handle_key(key(KeyCode::Esc)), ThreadAction::Back);
        v.composer.clear();
        v.focus = Focus::Composer;
        assert_eq!(v.handle_key(key(KeyCode::Esc)), ThreadAction::Back);
        assert_eq!(v.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), ThreadAction::Quit);
        // No target → status, no action.
        let mut empty = data();
        empty.live_sessions.clear();
        empty.last_agent_name = None;
        let mut v = ThreadView::new("T1", empty);
        type_str(&mut v, "hello");
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ThreadAction::None);
        assert!(v.status.as_deref().unwrap().contains("@name"));
    }

    #[test]
    fn attachments_chips_and_dispatch_carry_them() {
        let dir = std::env::temp_dir().join(format!("axel-tui-thread-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, b"x").unwrap();
        std::fs::write(&f2, b"x").unwrap();
        let mut v = ThreadView::new("T1", data());
        v.handle_paste(&format!("{} {}", f1.display(), f2.display()));
        assert!(v.composer.is_empty());
        assert_eq!(v.attachments.len(), 2);
        v.handle_paste("not a path");
        assert_eq!(v.composer.text(), "not a path");
        // Backspace with text edits text; on empty composer it pops a chip; Ctrl+U clears text only.
        v.handle_key(key(KeyCode::Backspace));
        assert_eq!(v.composer.text(), "not a pat");
        v.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(v.composer.is_empty() && v.attachments.len() == 2);
        v.handle_key(key(KeyCode::Backspace));
        assert_eq!(v.attachments.len(), 1);
        // A message with chips sends them and clears the chips.
        type_str(&mut v, "look at this");
        let a = v.handle_key(key(KeyCode::Enter));
        match a {
            ThreadAction::Send { cmd: SendCommand::Prompt { text, attachments, .. }, .. } => {
                assert_eq!(text, "look at this");
                assert_eq!(attachments.len(), 1);
                assert!(attachments[0].ends_with("a.txt"));
            }
            other => panic!("expected prompt, got {other:?}"),
        }
        assert!(v.attachments.is_empty());
        v.on_reply(Ok(SendReply::Ok));
        // Attachments alone (no text) can be sent; summon carries them too.
        v.handle_paste(f2.to_str().unwrap());
        type_str(&mut v, "@nova see");
        match v.handle_key(key(KeyCode::Enter)) {
            ThreadAction::Summon { agent, attachments, .. } => {
                assert_eq!(agent, "nova");
                assert_eq!(attachments.len(), 1);
            }
            other => panic!("expected summon, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attachment_focus_cycling_selection_and_removal() {
        let mut v = ThreadView::new("T1", data());
        // Without chips Tab goes straight to the timeline and back.
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Timeline);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Composer);
        v.attachments = vec![PathBuf::from("/x/a.png"), PathBuf::from("/x/b.txt"), PathBuf::from("/x/c.rs")];
        // composer → attachments → timeline → composer.
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Attachments);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Timeline);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Composer);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Attachments);
        // selection moves with ←/→ and h/l, clamped at the edges
        assert_eq!(v.attach_sel, 0);
        v.handle_key(key(KeyCode::Left));
        assert_eq!(v.attach_sel, 0);
        v.handle_key(key(KeyCode::Right));
        v.handle_key(key(KeyCode::Char('l')));
        v.handle_key(key(KeyCode::Right));
        assert_eq!(v.attach_sel, 2, "clamped at the last chip");
        v.handle_key(key(KeyCode::Char('h')));
        assert_eq!(v.attach_sel, 1);
        // removing the middle chip keeps the selection on its right neighbour
        v.handle_key(key(KeyCode::Char('x')));
        assert_eq!(v.attachments, vec![PathBuf::from("/x/a.png"), PathBuf::from("/x/c.rs")]);
        assert_eq!(v.attach_sel, 1);
        assert_eq!(v.focus, Focus::Attachments);
        // removing the last chip moves the selection left
        v.handle_key(key(KeyCode::Delete));
        assert_eq!(v.attachments, vec![PathBuf::from("/x/a.png")]);
        assert_eq!(v.attach_sel, 0);
        // a printable char returns to the composer and is typed
        v.handle_key(key(KeyCode::Char('o')));
        assert_eq!(v.focus, Focus::Composer);
        assert_eq!(v.composer.text(), "o");
        v.composer.clear();
        // Esc returns to the composer; removing the only chip also returns
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Attachments);
        v.handle_key(key(KeyCode::Esc));
        assert_eq!(v.focus, Focus::Composer);
        v.handle_key(key(KeyCode::Tab));
        v.handle_key(key(KeyCode::Backspace));
        assert!(v.attachments.is_empty());
        assert_eq!(v.focus, Focus::Composer);
        // Enter from the attachments focus sends as usual
        v.attachments = vec![PathBuf::from("/x/a.png")];
        type_str(&mut v, "see");
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Attachments);
        assert!(matches!(v.handle_key(key(KeyCode::Enter)), ThreadAction::Send { .. }));
        assert!(v.attachments.is_empty());
    }

    #[test]
    fn timeline_typing_switches_to_composer_and_hotkeys_work() {
        let mut v = ThreadView::new("T1", data());
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Timeline);
        assert_eq!(v.handle_key(key(KeyCode::Char('l'))), ThreadAction::OpenLog("S1".into()));
        v.handle_key(key(KeyCode::Char('x')));
        assert_eq!(v.focus, Focus::Composer);
        assert_eq!(v.composer.text(), "x");
        // l/g/G type in composer focus
        type_str(&mut v, "lgG");
        assert_eq!(v.composer.text(), "xlgG");
    }

    #[test]
    fn set_data_follows_or_keeps_cursor() {
        let mut v = ThreadView::new("T1", data());
        let mut d = data();
        d.items.push(item("a2", ItemKind::AssistantMessage, ItemBody::Text("Tests pass.".into()), 9_000));
        v.set_data(d.clone());
        assert_eq!(v.cursor, 4, "following: cursor moved to the new last row");
        // Scroll up, add more: cursor stays on the same row key.
        v.handle_key(key(KeyCode::Tab));
        v.handle_key(key(KeyCode::Up));
        v.handle_key(key(KeyCode::Up)); // on the fold (↑ from permission with option 0 moves up)
        assert!(matches!(v.current_row(), Some(Row::Fold(_))));
        d.items.insert(0, item("u0", ItemKind::UserMessage, ItemBody::Text("earlier".into()), 500));
        v.set_data(d);
        assert!(matches!(v.current_row(), Some(Row::Fold(_))));
        assert!(!v.follow);
        // log target without cursor session falls back to the live session
        let mut v2 = ThreadView::new("T1", ThreadData { live_sessions: data().live_sessions, ..Default::default() });
        assert_eq!(v2.log_target(), Some("S1".into()));
        v2.data.live_sessions.clear();
        assert_eq!(v2.log_target(), None);
    }

    fn queued_item(id: &str, text: &str) -> ThreadItem {
        let mut it = item(id, ItemKind::UserMessage, ItemBody::Text(text.into()), 7_000);
        it.agent = None;
        it.status = Some(ItemStatus::Pending);
        it
    }

    /// Mid-turn, with the fixture's permission already answered.
    fn busy_data() -> ThreadData {
        let mut d = data();
        d.agents[0].2 = axel_store::StatusState::Working;
        d.items[7].status = Some(ItemStatus::Allowed);
        d
    }

    #[test]
    fn arrows_in_the_composer_browse_prompt_history() {
        let mut d = data();
        let mut second = item("u2", ItemKind::UserMessage, ItemBody::Text("now run clippy".into()), 8_000);
        second.agent = None;
        let mut other = item("u3", ItemKind::UserMessage, ItemBody::Text("from a teammate".into()), 8_500);
        other.agent = None;
        other.author = Some("ana".into());
        d.items.push(second);
        d.items.push(other);
        let mut v = ThreadView::new("T1", d);
        assert_eq!(v.history(), vec!["Fix the login bug", "now run clippy"], "own prompts only");
        let cursor = v.cursor;
        type_str(&mut v, "draft");
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.composer.text(), "now run clippy");
        assert_eq!(v.cursor, cursor, "the timeline cursor does not move");
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.composer.text(), "Fix the login bug");
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.composer.text(), "Fix the login bug", "clamped at the oldest");
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.composer.text(), "now run clippy");
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.composer.text(), "draft", "past the newest restores the draft");
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.composer.text(), "draft");
        // Editing a recalled entry starts a fresh browse from the edited text.
        v.handle_key(key(KeyCode::Up));
        type_str(&mut v, "!");
        assert_eq!(v.composer.text(), "now run clippy!");
        v.handle_key(key(KeyCode::Up));
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.composer.text(), "now run clippy!");
        // Sending adds to the history right away, deduplicated.
        v.composer.clear();
        type_str(&mut v, "Fix the login bug");
        assert!(matches!(v.handle_key(key(KeyCode::Enter)), ThreadAction::Send { .. }));
        v.on_reply(Ok(SendReply::Ok));
        assert_eq!(v.history(), vec!["now run clippy", "Fix the login bug"]);
        // PgUp still scrolls the timeline from the composer.
        v.handle_key(key(KeyCode::PageUp));
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn sending_mid_turn_shows_the_message_as_queued() {
        let mut v = ThreadView::new("T1", busy_data());
        type_str(&mut v, "also check logout");
        assert_eq!(
            v.handle_key(key(KeyCode::Enter)),
            ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Prompt { text: "also check logout".into(), attachments: vec![], steer: false } }
        );
        v.on_reply(Ok(SendReply::Queued { item_id: "q1".into() }));
        let q = v.queued();
        assert_eq!(q.len(), 1, "shown before the store refresh");
        assert_eq!(v.data.items[q[0]].body, ItemBody::Text("also check logout".into()));
        assert!(v.rows().iter().all(|r| r.key(&v.data.items) != "q1"), "not a timeline row");
        assert!(v.status.as_deref().unwrap().contains("queued"));
        // The store's copy replaces the optimistic one.
        let mut d = busy_data();
        d.items.push(queued_item("q1", "also check logout"));
        v.set_data(d);
        assert_eq!(v.queued().len(), 1);
    }

    #[test]
    fn failed_send_gives_the_text_back() {
        let mut v = ThreadView::new("T1", data());
        type_str(&mut v, "important prompt");
        v.handle_key(key(KeyCode::Enter));
        assert!(v.composer.is_empty());
        v.on_reply(Ok(SendReply::Error { message: "host unreachable".into() }));
        assert_eq!(v.composer.text(), "important prompt");
        assert!(v.status.as_deref().unwrap().starts_with("error"));
    }

    #[test]
    fn ctrl_s_steers_typed_text_or_the_next_queued_message() {
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let mut d = busy_data();
        d.items.push(queued_item("q1", "first"));
        d.items.push(queued_item("q2", "second"));
        let mut v = ThreadView::new("T1", d);
        assert!(v.help().iter().any(|(k, _)| *k == "^S"));
        // With text: send it now as a steer.
        type_str(&mut v, "stop, do this instead");
        assert_eq!(
            v.handle_key(ctrl_s),
            ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Prompt { text: "stop, do this instead".into(), attachments: vec![], steer: true } }
        );
        v.on_reply(Ok(SendReply::Queued { item_id: "q3".into() }));
        // Empty composer: steer the oldest queued message.
        assert_eq!(v.handle_key(ctrl_s), ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Steer { item_id: "q1".into() } });
        v.on_reply(Ok(SendReply::Ok));
        // Idle thread with nothing queued: ^S does nothing.
        let mut idle_data = data();
        idle_data.agents[0].2 = axel_store::StatusState::Idle;
        let mut idle = ThreadView::new("T1", idle_data);
        assert_eq!(idle.handle_key(ctrl_s), ThreadAction::None);
        assert!(!idle.help().iter().any(|(k, _)| *k == "^S"));
    }

    #[test]
    fn queue_focus_selects_steers_edits_and_removes() {
        let mut d = busy_data();
        d.items.push(queued_item("q1", "first"));
        d.items.push(queued_item("q2", "second"));
        let mut v = ThreadView::new("T1", d.clone());
        // Tab: composer → queue (no chips) → timeline → composer.
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Queue);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Timeline);
        v.handle_key(key(KeyCode::Tab));
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Queue);
        // ↓ selects within the queue, not the timeline.
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.queue_sel, 1);
        assert_eq!(v.cursor, 3);
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Steer { item_id: "q2".into() } });
        v.on_reply(Ok(SendReply::Ok));
        // e: withdraw into the composer for editing.
        v.handle_key(key(KeyCode::Up));
        assert_eq!(v.handle_key(key(KeyCode::Char('e'))), ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Dequeue { item_id: "q1".into() } });
        assert_eq!(v.composer.text(), "first");
        assert_eq!(v.focus, Focus::Composer);
        assert_eq!(v.queued().len(), 1);
        v.on_reply(Ok(SendReply::Ok));
        // e with text in the composer is refused.
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Queue);
        assert_eq!(v.handle_key(key(KeyCode::Char('e'))), ThreadAction::None);
        // x removes; the last one gone returns focus to the composer.
        assert_eq!(v.handle_key(key(KeyCode::Char('x'))), ThreadAction::Send { session_id: "S1".into(), cmd: SendCommand::Dequeue { item_id: "q2".into() } });
        v.on_reply(Ok(SendReply::Ok));
        assert!(v.queued().is_empty());
        assert_eq!(v.focus, Focus::Composer);
        // A refresh that drops the queue while focused also returns to the composer.
        let mut v = ThreadView::new("T1", d);
        v.handle_key(key(KeyCode::Tab));
        v.set_data(busy_data());
        assert_eq!(v.focus, Focus::Composer);
    }

    #[test]
    fn shift_tab_cycles_the_session_mode_and_the_wheel_scrolls() {
        let mut v = ThreadView::new("T1", busy_data());
        assert_eq!(v.mode_session().unwrap().mode, RuntimeMode::Autopilot);
        let mut seen = vec![];
        for _ in 0..4 {
            match v.handle_key(key(KeyCode::BackTab)) {
                ThreadAction::Send { session_id, cmd: SendCommand::SetMode { mode } } => {
                    assert_eq!(session_id, "S1");
                    seen.push(mode);
                }
                other => panic!("expected SetMode, got {other:?}"),
            }
            v.on_reply(Ok(SendReply::Ok));
        }
        assert_eq!(seen, vec![RuntimeMode::Supervised, RuntimeMode::Edits, RuntimeMode::Plan, RuntimeMode::Autopilot]);
        assert_eq!(v.focus, Focus::Composer, "Shift+Tab no longer moves focus");
        // No managed session: nothing to switch.
        let mut d = busy_data();
        d.live_sessions[0].managed = false;
        let mut p = ThreadView::new("T1", d);
        assert_eq!(p.handle_key(key(KeyCode::BackTab)), ThreadAction::None);
        // The wheel scrolls the timeline and never touches the composer's history.
        type_str(&mut v, "draft");
        let last = v.cursor;
        v.scroll(-3);
        assert_eq!(v.cursor, last - 3);
        assert!(!v.follow);
        assert_eq!(v.composer.text(), "draft");
        v.scroll(100);
        assert_eq!(v.cursor, last);
        assert!(v.follow);
    }
}
