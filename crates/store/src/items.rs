//! Conversation items of managed sessions.  Local-only: they are not
//! envelopes, never leave the machine, and `rebuild_projections` leaves them
//! alone (like `agents` and `projects`).

use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;

use crate::{ConversationItem, Error, ItemKind, ItemStatus, NewItem, Store, new_id};

pub(crate) const ITEM_COLS: &str =
    "id, session_id, thread_id, seq, turn, at, kind, status, ref_id, content_json";

pub(crate) fn row_item(r: &Row<'_>) -> rusqlite::Result<ConversationItem> {
    let conv = |e: Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let content_json: String = r.get("content_json")?;
    Ok(ConversationItem {
        id: r.get("id")?,
        session_id: r.get("session_id")?,
        thread_id: r.get("thread_id")?,
        seq: r.get("seq")?,
        turn: r.get("turn")?,
        at: r.get("at")?,
        kind: r
            .get::<_, String>("kind")?
            .parse::<ItemKind>()
            .map_err(conv)?,
        status: r
            .get::<_, Option<String>>("status")?
            .map(|s| s.parse::<ItemStatus>())
            .transpose()
            .map_err(conv)?,
        ref_id: r.get("ref_id")?,
        content: serde_json::from_str(&content_json).map_err(|e| conv(Error::Json(e)))?,
    })
}

/// Current Unix time in milliseconds.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Store {
    /// Append an item to a session's conversation.  `seq` is the next number
    /// for that session (from 1), assigned inside a transaction.
    pub fn push_item(&self, item: &NewItem) -> Result<ConversationItem, Error> {
        let content_json = serde_json::to_string(&item.content)?;
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM conversation_items WHERE session_id = ?1",
            [&item.session_id],
            |r| r.get(0),
        )?;
        let out = ConversationItem {
            id: new_id(),
            session_id: item.session_id.clone(),
            thread_id: item.thread_id.clone(),
            seq,
            turn: item.turn,
            at: now_ms(),
            kind: item.kind,
            status: item.status,
            ref_id: item.ref_id.clone(),
            content: item.content.clone(),
        };
        tx.execute(
            "INSERT INTO conversation_items (id, session_id, thread_id, seq, turn, at, kind, status, ref_id, content_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                out.id,
                out.session_id,
                out.thread_id,
                out.seq,
                out.turn,
                out.at,
                out.kind.as_str(),
                out.status.map(ItemStatus::as_str),
                out.ref_id,
                content_json
            ],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// Change an item's status and/or shallow-merge `content_patch` into its
    /// content (top-level keys replace; a `null` value removes the key; a
    /// non-object patch replaces the content).
    pub fn update_item(
        &self,
        id: &str,
        status: Option<ItemStatus>,
        content_patch: Option<Value>,
    ) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT content_json FROM conversation_items WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(Error::NotFound(format!("conversation item {id}")));
        };
        if let Some(patch) = content_patch {
            let mut content: Value = serde_json::from_str(&current)?;
            match (&mut content, patch) {
                (Value::Object(base), Value::Object(patch)) => {
                    for (k, v) in patch {
                        if v.is_null() {
                            base.remove(&k);
                        } else {
                            base.insert(k, v);
                        }
                    }
                }
                (slot, patch) => *slot = patch,
            }
            tx.execute(
                "UPDATE conversation_items SET content_json = ?2 WHERE id = ?1",
                params![id, serde_json::to_string(&content)?],
            )?;
        }
        if let Some(status) = status {
            tx.execute(
                "UPDATE conversation_items SET status = ?2 WHERE id = ?1",
                params![id, status.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove an item (a queued prompt that was withdrawn, or re-pushed when its
    /// turn started).  Missing ids are not an error.
    pub fn delete_item(&self, id: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM conversation_items WHERE id = ?1", [id])?;
        Ok(())
    }

    /// One item by id.
    pub fn item(&self, id: &str) -> Result<Option<ConversationItem>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ITEM_COLS} FROM conversation_items WHERE id = ?1"),
                [id],
                row_item,
            )
            .optional()?)
    }

    /// The latest item of a session carrying `ref_id` (an ACP tool-call or permission id).
    pub fn item_by_ref(
        &self,
        session_id: &str,
        ref_id: &str,
    ) -> Result<Option<ConversationItem>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {ITEM_COLS} FROM conversation_items WHERE session_id = ?1 AND ref_id = ?2
                     ORDER BY seq DESC LIMIT 1"
                ),
                params![session_id, ref_id],
                row_item,
            )
            .optional()?)
    }

    /// Items of a session with `seq > after_seq`, in order, at most `limit`.
    pub fn items(
        &self,
        session_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<ConversationItem>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM conversation_items WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![session_id, after_seq, limit as i64], row_item)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The last `limit` items across every session of a thread, ordered by
    /// `(at, seq)` ascending (newest last).
    pub fn thread_items(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<ConversationItem>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT * FROM (
                 SELECT {ITEM_COLS} FROM conversation_items WHERE thread_id = ?1
                 ORDER BY at DESC, seq DESC, session_id DESC LIMIT ?2)
             ORDER BY at ASC, seq ASC, session_id ASC"
        ))?;
        let rows = st.query_map(params![thread_id, limit as i64], row_item)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A session's tool calls in `turn` still running or pending, oldest first:
    /// the ones that never got a result, when the turn ends.
    pub fn open_tool_calls(&self, session_id: &str, turn: i64) -> Result<Vec<ConversationItem>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM conversation_items
             WHERE session_id = ?1 AND turn = ?2 AND kind = ?3 AND status IN (?4, ?5)
             ORDER BY seq ASC"
        ))?;
        let rows = st.query_map(
            params![session_id, turn, ItemKind::ToolCall.as_str(), ItemStatus::Running.as_str(), ItemStatus::Pending.as_str()],
            row_item,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Permissions and questions still waiting for an answer, oldest first;
    /// `thread_id` `None` means every thread.
    pub fn pending_items(&self, thread_id: Option<&str>) -> Result<Vec<ConversationItem>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM conversation_items
             WHERE status = ?1 AND kind IN (?2, ?3) AND (?4 IS NULL OR thread_id = ?4)
             ORDER BY at ASC, seq ASC"
        ))?;
        let rows = st.query_map(
            params![
                ItemStatus::Pending.as_str(),
                ItemKind::Permission.as_str(),
                ItemKind::Question.as_str(),
                thread_id
            ],
            row_item,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
