//! Dashboard state machine. Pure: a [`DashboardData`] snapshot plus focus and
//! selection over the threads and agents widgets; consumes [`KeyEvent`]s and
//! produces [`DashAction`]s.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{DashAgentRow, DashboardData, ThreadRow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Widget {
    Threads,
    Agents,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DashAction {
    None,
    Quit,
    OpenLauncher,
    OpenInbox,
    /// Focus the Rex window hosting this session (`o`).
    FocusSession(String),
    /// Open the thread view (Enter).
    OpenThread(String),
    /// Open the launcher pre-filled to add another agent to this thread.
    Spawn { thread_id: String, title: String },
    Refresh,
}

#[derive(Clone, Debug)]
pub struct Dashboard {
    pub data: DashboardData,
    pub focus: Widget,
    /// Set by the app when the area is too small for a threads widget: keys act
    /// on the agents list regardless of `focus` and Tab does nothing.
    pub compact: bool,
    pub thread_sel: usize,
    pub agent_sel: usize,
    pub flash: Option<(String, i64)>,
}

impl Dashboard {
    pub fn new(data: DashboardData) -> Self {
        Self { data, focus: Widget::Threads, compact: false, thread_sel: 0, agent_sel: 0, flash: None }
    }

    /// Replace the snapshot, keeping selections on the same thread/agent when possible.
    pub fn set_data(&mut self, data: DashboardData) {
        let tid = self.selected_thread().map(|t| t.id.clone());
        let pk = self.selected_agent().map(|a| a.pubkey.clone());
        self.data = data;
        self.thread_sel = tid
            .and_then(|id| self.data.threads.iter().position(|t| t.id == id))
            .unwrap_or_else(|| self.thread_sel.min(self.data.threads.len().saturating_sub(1)));
        self.agent_sel = pk
            .and_then(|pk| self.data.agents.iter().position(|a| a.pubkey == pk))
            .unwrap_or_else(|| self.agent_sel.min(self.data.agents.len().saturating_sub(1)));
    }

    pub fn selected_thread(&self) -> Option<&ThreadRow> {
        self.data.threads.get(self.thread_sel)
    }

    pub fn selected_agent(&self) -> Option<&DashAgentRow> {
        self.data.agents.get(self.agent_sel)
    }

    pub fn set_flash(&mut self, msg: impl Into<String>, now: i64) {
        self.flash = Some((msg.into(), now));
    }

    pub fn tick(&mut self, now: i64) {
        if let Some((_, at)) = self.flash
            && now - at > 4
        {
            self.flash = None;
        }
    }

    /// The widget keys act on: agents when compact, else the focused one.
    pub fn active_widget(&self) -> Widget {
        if self.compact { Widget::Agents } else { self.focus }
    }

    fn move_sel(&mut self, delta: i64) {
        let (sel, len) = match self.active_widget() {
            Widget::Threads => (&mut self.thread_sel, self.data.threads.len()),
            Widget::Agents => (&mut self.agent_sel, self.data.agents.len()),
        };
        if len == 0 {
            *sel = 0;
        } else {
            *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: i64) -> DashAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return DashAction::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => DashAction::Quit,
            KeyCode::Char('n') if !ctrl => DashAction::OpenLauncher,
            KeyCode::Char('i') => DashAction::OpenInbox,
            KeyCode::Char('R') => DashAction::Refresh,
            KeyCode::Tab | KeyCode::BackTab => {
                if !self.compact {
                    self.focus = match self.focus {
                        Widget::Threads => Widget::Agents,
                        Widget::Agents => Widget::Threads,
                    };
                }
                DashAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_sel(-1);
                DashAction::None
            }
            KeyCode::Char('p') if ctrl => {
                self.move_sel(-1);
                DashAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_sel(1);
                DashAction::None
            }
            KeyCode::Char('n') if ctrl => {
                self.move_sel(1);
                DashAction::None
            }
            KeyCode::Home => {
                self.move_sel(i64::MIN / 2);
                DashAction::None
            }
            KeyCode::End => {
                self.move_sel(i64::MAX / 2);
                DashAction::None
            }
            KeyCode::Char('s') if self.active_widget() == Widget::Threads => match self.selected_thread() {
                Some(t) => DashAction::Spawn { thread_id: t.id.clone(), title: t.title.clone() },
                None => {
                    self.set_flash("No thread selected. Press n to start one.", now);
                    DashAction::None
                }
            },
            KeyCode::Enter => self.open_thread(now),
            KeyCode::Char('o') => self.focus_session(now),
            _ => DashAction::None,
        }
    }

    /// Enter: open the selected thread, or the thread of the selected agent's latest session.
    fn open_thread(&mut self, now: i64) -> DashAction {
        match self.active_widget() {
            Widget::Threads => match self.selected_thread() {
                Some(t) => DashAction::OpenThread(t.id.clone()),
                None => {
                    self.set_flash("No open threads. Press n to start one.", now);
                    DashAction::None
                }
            },
            Widget::Agents => match self.selected_agent() {
                Some(a) => match &a.thread_id {
                    Some(tid) => DashAction::OpenThread(tid.clone()),
                    None => {
                        let msg = format!("{} has no thread yet. Press n to start one.", a.name);
                        self.set_flash(msg, now);
                        DashAction::None
                    }
                },
                None => {
                    self.set_flash("No agents yet. Press n to create one.", now);
                    DashAction::None
                }
            },
        }
    }

    /// `o`: focus the Rex pane of the most urgent live session.
    fn focus_session(&mut self, now: i64) -> DashAction {
        match self.active_widget() {
            Widget::Threads => match self.selected_thread() {
                Some(t) => match &t.focus_session {
                    Some(sid) => DashAction::FocusSession(sid.clone()),
                    None => {
                        let msg = format!("\u{201c}{}\u{201d} has no live session. Press s to spawn an agent onto it.", t.title);
                        self.set_flash(msg, now);
                        DashAction::None
                    }
                },
                None => {
                    self.set_flash("No open threads. Press n to start one.", now);
                    DashAction::None
                }
            },
            Widget::Agents => match self.selected_agent() {
                Some(a) => match &a.active_session_id {
                    Some(sid) => DashAction::FocusSession(sid.clone()),
                    None => {
                        let msg = format!("{} has no live session. Press n to start a thread.", a.name);
                        self.set_flash(msg, now);
                        DashAction::None
                    }
                },
                None => {
                    self.set_flash("No agents yet. Press n to create one.", now);
                    DashAction::None
                }
            },
        }
    }

    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![("n", "new thread"), ("enter", "open thread"), ("o", "pane")];
        if self.active_widget() == Widget::Threads {
            v.push(("s", "spawn agent"));
        }
        v.extend([("tab", "widget"), ("i", "inbox"), ("q", "quit")]);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn thread(title: &str, focus: Option<&str>) -> ThreadRow {
        #[allow(clippy::needless_update)]
        ThreadRow {
            id: format!("T_{title}"),
            title: title.into(),
            focus_session: focus.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    fn agent(name: &str, session: Option<&str>) -> DashAgentRow {
        DashAgentRow {
            pubkey: format!("PK_{name}"),
            name: name.into(),
            active_session_id: session.map(|s| s.to_string()),
            thread_id: session.map(|_| "T_Fix login".to_string()),
            ..Default::default()
        }
    }

    fn data() -> DashboardData {
        DashboardData {
            threads: vec![thread("Fix login", Some("S1")), thread("Docs", None)],
            agents: vec![agent("atlas", Some("S1")), agent("nova", None)],
            ..Default::default()
        }
    }

    #[test]
    fn global_keys() {
        let mut d = Dashboard::new(data());
        assert_eq!(d.handle_key(key(KeyCode::Char('n')), 0), DashAction::OpenLauncher);
        assert_eq!(d.handle_key(key(KeyCode::Char('i')), 0), DashAction::OpenInbox);
        assert_eq!(d.handle_key(key(KeyCode::Char('q')), 0), DashAction::Quit);
        assert_eq!(d.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 0), DashAction::Quit);
    }

    #[test]
    fn threads_widget_enter_and_spawn() {
        let mut d = Dashboard::new(data());
        assert_eq!(d.focus, Widget::Threads);
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::OpenThread("T_Fix login".into()));
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::FocusSession("S1".into()));
        assert_eq!(
            d.handle_key(key(KeyCode::Char('s')), 0),
            DashAction::Spawn { thread_id: "T_Fix login".into(), title: "Fix login".into() }
        );
        d.handle_key(key(KeyCode::Down), 0);
        assert_eq!(d.thread_sel, 1);
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::OpenThread("T_Docs".into()), "threads open even without a live session");
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::None);
        assert!(d.flash.as_ref().unwrap().0.contains("Docs"));
        d.handle_key(key(KeyCode::Down), 0);
        assert_eq!(d.thread_sel, 1, "clamped");
        d.handle_key(key(KeyCode::Home), 0);
        assert_eq!(d.thread_sel, 0);
        d.handle_key(key(KeyCode::End), 0);
        assert_eq!(d.thread_sel, 1);
    }

    #[test]
    fn tab_moves_focus_to_agents() {
        let mut d = Dashboard::new(data());
        d.handle_key(key(KeyCode::Tab), 0);
        assert_eq!(d.focus, Widget::Agents);
        assert_eq!(d.handle_key(key(KeyCode::Char('s')), 0), DashAction::None, "spawn only on threads");
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::OpenThread("T_Fix login".into()));
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::FocusSession("S1".into()));
        d.handle_key(key(KeyCode::Down), 0);
        assert_eq!(d.agent_sel, 1);
        assert_eq!(d.thread_sel, 0, "selections are independent");
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::None);
        assert!(d.flash.as_ref().unwrap().0.contains("nova"));
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::None);
        d.handle_key(key(KeyCode::BackTab), 0);
        assert_eq!(d.focus, Widget::Threads);
        assert!(d.help().iter().any(|(k, _)| *k == "s"));
    }

    #[test]
    fn compact_mode_acts_on_agents_and_ignores_tab() {
        let mut d = Dashboard::new(data());
        d.compact = true;
        assert_eq!(d.focus, Widget::Threads, "focus untouched");
        assert_eq!(d.active_widget(), Widget::Agents);
        // Enter opens the selected agent's thread; `o` focuses its session.
        d.data.agents[0].active_session_id = Some("S_AGENT".into());
        d.data.agents[0].thread_id = Some("T_AGENT".into());
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::OpenThread("T_AGENT".into()));
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::FocusSession("S_AGENT".into()));
        // Arrows move the agent selection.
        d.handle_key(key(KeyCode::Down), 0);
        assert_eq!((d.agent_sel, d.thread_sel), (1, 0));
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::None);
        assert!(d.flash.as_ref().unwrap().0.contains("nova"));
        // Tab is a no-op and `s` (threads-only) does nothing.
        d.handle_key(key(KeyCode::Tab), 0);
        assert_eq!(d.focus, Widget::Threads);
        assert_eq!(d.handle_key(key(KeyCode::Char('s')), 0), DashAction::None);
        assert!(!d.help().iter().any(|(k, _)| *k == "s"));
        // Global keys still work.
        assert_eq!(d.handle_key(key(KeyCode::Char('n')), 0), DashAction::OpenLauncher);
        assert_eq!(d.handle_key(key(KeyCode::Char('i')), 0), DashAction::OpenInbox);
        assert_eq!(d.handle_key(key(KeyCode::Char('q')), 0), DashAction::Quit);
        // Leaving compact mode restores thread focus behaviour.
        d.compact = false;
        assert_eq!(d.handle_key(key(KeyCode::Enter), 0), DashAction::OpenThread("T_Fix login".into()));
        assert_eq!(d.handle_key(key(KeyCode::Char('o')), 0), DashAction::FocusSession("S1".into()));
    }

    #[test]
    fn empty_dashboard_hints_and_flash_expiry() {
        let mut d = Dashboard::new(DashboardData::default());
        assert_eq!(d.handle_key(key(KeyCode::Enter), 5), DashAction::None);
        assert!(d.flash.is_some());
        assert_eq!(d.handle_key(key(KeyCode::Char('s')), 5), DashAction::None);
        d.tick(10);
        assert!(d.flash.is_none());
    }

    #[test]
    fn refresh_keeps_selections_by_id() {
        let mut d = Dashboard::new(data());
        d.handle_key(key(KeyCode::Down), 0);
        d.handle_key(key(KeyCode::Tab), 0);
        d.handle_key(key(KeyCode::Down), 0);
        let mut nd = data();
        nd.threads.reverse();
        nd.agents.reverse();
        d.set_data(nd);
        assert_eq!(d.selected_thread().unwrap().title, "Docs");
        assert_eq!(d.selected_agent().unwrap().name, "nova");
        d.set_data(DashboardData::default());
        assert_eq!((d.thread_sel, d.agent_sel), (0, 0));
    }
}
