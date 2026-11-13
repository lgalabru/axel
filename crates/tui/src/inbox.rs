//! Inbox state machine. Pure: holds an [`InboxData`] snapshot plus selection,
//! consumes [`KeyEvent`]s and produces [`InboxAction`]s.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{InboxData, RequestKind, RequestRow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Requests,
    Messages,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxAction {
    None,
    Quit,
    OpenLauncher,
    Allow(RequestRow),
    Deny(RequestRow),
    MarkRead,
    Refresh,
}

#[derive(Clone, Debug)]
pub struct Inbox {
    pub data: InboxData,
    pub focus: Focus,
    pub request_sel: usize,
    pub message_sel: usize,
    /// Transient feedback line ("allowed", "marked read", errors) with the time it was set.
    pub flash: Option<(String, i64)>,
}

impl Inbox {
    pub fn new(data: InboxData) -> Self {
        Self { data, focus: Focus::Requests, request_sel: 0, message_sel: 0, flash: None }
    }

    /// Replace the snapshot, keeping the selection on the same request id when possible.
    pub fn set_data(&mut self, data: InboxData) {
        let selected_id = self.selected_request().map(|r| r.id.clone());
        let selected_msg = self.data.messages.get(self.message_sel).map(|m| m.id.clone());
        self.data = data;
        self.request_sel = selected_id
            .and_then(|id| self.data.requests.iter().position(|r| r.id == id))
            .unwrap_or_else(|| self.request_sel.min(self.data.requests.len().saturating_sub(1)));
        self.message_sel = selected_msg
            .and_then(|id| self.data.messages.iter().position(|m| m.id == id))
            .unwrap_or_else(|| self.message_sel.min(self.data.messages.len().saturating_sub(1)));
    }

    pub fn selected_request(&self) -> Option<&RequestRow> {
        self.data.requests.get(self.request_sel)
    }

    pub fn set_flash(&mut self, msg: impl Into<String>, now: i64) {
        self.flash = Some((msg.into(), now));
    }

    /// Drop the flash after a few seconds.
    pub fn tick(&mut self, now: i64) {
        if let Some((_, at)) = self.flash
            && now - at > 4
        {
            self.flash = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InboxAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return InboxAction::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => InboxAction::Quit,
            KeyCode::Char('n') if !ctrl => InboxAction::OpenLauncher,
            KeyCode::Char('r') if !ctrl => InboxAction::MarkRead,
            KeyCode::Char('R') => InboxAction::Refresh,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Requests => Focus::Messages,
                    Focus::Messages => Focus::Requests,
                };
                InboxAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_sel(-1);
                InboxAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_sel(1);
                InboxAction::None
            }
            KeyCode::Char('p') if ctrl => {
                self.move_sel(-1);
                InboxAction::None
            }
            KeyCode::Char('n') if ctrl => {
                self.move_sel(1);
                InboxAction::None
            }
            KeyCode::Home => {
                self.set_sel(0);
                InboxAction::None
            }
            KeyCode::End => {
                self.set_sel(usize::MAX);
                InboxAction::None
            }
            KeyCode::Char('a') => self.reply(true),
            KeyCode::Char('d') => self.reply(false),
            _ => InboxAction::None,
        }
    }

    fn reply(&mut self, allow: bool) -> InboxAction {
        match self.selected_request() {
            Some(req) if req.kind == RequestKind::Permission => {
                let req = req.clone();
                if allow { InboxAction::Allow(req) } else { InboxAction::Deny(req) }
            }
            Some(_) => {
                self.flash = Some(("Questions need a typed answer; reply from the agent's pane.".into(), i64::MAX));
                InboxAction::None
            }
            None => InboxAction::None,
        }
    }

    fn move_sel(&mut self, delta: i32) {
        let (sel, len) = match self.focus {
            Focus::Requests => (&mut self.request_sel, self.data.requests.len()),
            Focus::Messages => (&mut self.message_sel, self.data.messages.len()),
        };
        if len == 0 {
            *sel = 0;
            return;
        }
        let next = (*sel as i64 + delta as i64).clamp(0, len as i64 - 1);
        *sel = next as usize;
    }

    fn set_sel(&mut self, to: usize) {
        let (sel, len) = match self.focus {
            Focus::Requests => (&mut self.request_sel, self.data.requests.len()),
            Focus::Messages => (&mut self.message_sel, self.data.messages.len()),
        };
        *sel = to.min(len.saturating_sub(1));
    }

    pub fn help(&self) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![("↑↓", "pick"), ("Tab", "section")];
        if matches!(self.selected_request(), Some(r) if r.kind == RequestKind::Permission) && self.focus == Focus::Requests {
            v.push(("a", "allow"));
            v.push(("d", "deny"));
        }
        v.push(("r", "mark read"));
        v.push(("n", "new thread"));
        v.push(("q", "quit"));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRow, StatusRow, StatusState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn req(id: &str, kind: RequestKind) -> RequestRow {
        RequestRow {
            id: id.into(),
            from_pubkey: "PK".into(),
            agent_name: "atlas".into(),
            kind,
            content: "run cargo test?".into(),
            thread_id: Some("T1".into()),
            session_id: Some("S1".into()),
            thread_title: Some("Fix bug".into()),
            created_at: 0,
        }
    }

    fn data() -> InboxData {
        InboxData {
            requests: vec![req("R1", RequestKind::Permission), req("R2", RequestKind::Question), req("R3", RequestKind::Permission)],
            statuses: vec![StatusRow {
                session_id: "S1".into(),
                thread_id: "T1".into(),
                state: StatusState::Working,
                agent_name: "atlas".into(),
                thread_title: "Fix bug".into(),
                msg: None,
                updated_at: 0,
            }],
            messages: vec![
                MessageRow {
                    id: "M1".into(),
                    from_pubkey: "PK".into(),
                    from_name: "atlas".into(),
                    kind: 1,
                    kind_label: "note".into(),
                    content: "hi".into(),
                    thread_id: None,
                    created_at: 10,
                },
                MessageRow {
                    id: "M2".into(),
                    from_pubkey: "PK".into(),
                    from_name: "atlas".into(),
                    kind: 1,
                    kind_label: "note".into(),
                    content: "there".into(),
                    thread_id: None,
                    created_at: 5,
                },
            ],
            unread: 2,
        }
    }

    #[test]
    fn quit_and_navigation_actions() {
        let mut ib = Inbox::new(data());
        assert_eq!(ib.handle_key(key(KeyCode::Char('q'))), InboxAction::Quit);
        assert_eq!(ib.handle_key(key(KeyCode::Esc)), InboxAction::Quit);
        assert_eq!(ib.handle_key(key(KeyCode::Char('n'))), InboxAction::OpenLauncher);
        assert_eq!(ib.handle_key(key(KeyCode::Char('r'))), InboxAction::MarkRead);
        assert_eq!(ib.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), InboxAction::Quit);
    }

    #[test]
    fn allow_deny_only_for_permissions() {
        let mut ib = Inbox::new(data());
        assert_eq!(ib.handle_key(key(KeyCode::Char('a'))), InboxAction::Allow(req("R1", RequestKind::Permission)));
        ib.handle_key(key(KeyCode::Down));
        assert_eq!(ib.request_sel, 1);
        assert_eq!(ib.handle_key(key(KeyCode::Char('d'))), InboxAction::None, "question: no deny");
        assert!(ib.flash.is_some());
        ib.handle_key(key(KeyCode::Down));
        assert_eq!(ib.handle_key(key(KeyCode::Char('d'))), InboxAction::Deny(req("R3", RequestKind::Permission)));
        // clamps at the end
        ib.handle_key(key(KeyCode::Down));
        assert_eq!(ib.request_sel, 2);
    }

    #[test]
    fn selection_survives_refresh_by_id() {
        let mut ib = Inbox::new(data());
        ib.handle_key(key(KeyCode::Down));
        ib.handle_key(key(KeyCode::Down));
        assert_eq!(ib.selected_request().unwrap().id, "R3");
        // R1 was answered and disappears
        let mut d = data();
        d.requests.remove(0);
        ib.set_data(d);
        assert_eq!(ib.selected_request().unwrap().id, "R3");
        // everything answered
        let mut d = data();
        d.requests.clear();
        ib.set_data(d);
        assert_eq!(ib.request_sel, 0);
        assert!(ib.selected_request().is_none());
        assert_eq!(ib.handle_key(key(KeyCode::Char('a'))), InboxAction::None);
    }

    #[test]
    fn tab_switches_focus_to_messages() {
        let mut ib = Inbox::new(data());
        ib.handle_key(key(KeyCode::Tab));
        assert_eq!(ib.focus, Focus::Messages);
        ib.handle_key(key(KeyCode::Char('j')));
        assert_eq!(ib.message_sel, 1);
        assert_eq!(ib.request_sel, 0);
        ib.handle_key(key(KeyCode::End));
        assert_eq!(ib.message_sel, 1);
        ib.handle_key(key(KeyCode::Home));
        assert_eq!(ib.message_sel, 0);
    }

    #[test]
    fn flash_expires() {
        let mut ib = Inbox::new(data());
        ib.set_flash("allowed", 100);
        ib.tick(102);
        assert!(ib.flash.is_some());
        ib.tick(105);
        assert!(ib.flash.is_none());
    }
}
