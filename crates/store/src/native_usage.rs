//! Local, replaceable native-log readings. Signed usage reports stay untouched.
use rusqlite::params;
use crate::{Error, NativeUsage, Store};

impl Store {
    /// Replace a session's native readings atomically. Call only after a complete,
    /// successful scan; unavailable files must retain the previous readings.
    pub fn replace_native_usage(&self, session_id: &str, records: &[NativeUsage]) -> Result<(), Error> {
        if self.get_session(session_id)?.is_none() { return Err(Error::NotFound(format!("session {session_id}"))) }
        for r in records {
            let d = &r.delta;
            if r.id.is_empty() || !d.cost_usd_est.is_finite() || d.cost_usd_est < 0.0
                || [d.input_tokens,d.output_tokens,d.cache_read_tokens,d.cache_write_tokens,d.reasoning_tokens].iter().any(|v| *v < 0)
            { return Err(Error::Invalid("invalid native usage reading".into())) }
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM native_usage WHERE session_id = ?1", [session_id])?;
        for r in records {
            let d = &r.delta;
            tx.execute("INSERT INTO native_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![session_id, r.id, r.at, d.model, d.input_tokens, d.output_tokens,
                    d.cache_read_tokens, d.cache_write_tokens, d.reasoning_tokens, d.cost_usd_est])?;
        }
        tx.execute("INSERT INTO native_usage_sources VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET checked_at=excluded.checked_at",
            params![session_id, crate::now()])?;
        tx.commit()?;
        Ok(())
    }
}
