//! Which threads the owner asked to inspect (ADR 0011).  Local-only: the
//! switch is this machine's, never in the shared log.  A thread that was
//! never turned on is off: the proxy carries credentials, so it runs only
//! where it was asked for.

use rusqlite::params;

use crate::{Error, Store, sql_limit};

/// Most inspected threads one read returns.
pub const INSPECTED_THREADS_MAX: usize = 1000;

impl Store {
    /// Whether `thread_id`'s new managed sessions run through the inspector.
    pub fn thread_inspected(&self, thread_id: &str) -> Result<bool, Error> {
        let found: i64 = self.conn.query_row("SELECT COUNT(*) FROM inspected_threads WHERE thread_id = ?1", [thread_id], |r| r.get(0))?;
        debug_assert!(found <= 1, "thread_id is the key");
        Ok(found == 1)
    }

    /// Turn inspection of `thread_id` on or off.
    pub fn set_thread_inspected(&self, thread_id: &str, on: bool) -> Result<(), Error> {
        if thread_id.is_empty() {
            return Err(Error::Invalid("inspect: empty thread id".into()));
        }
        if on {
            self.conn.execute(
                "INSERT INTO inspected_threads (thread_id, since_ms) VALUES (?1, ?2) ON CONFLICT(thread_id) DO NOTHING",
                params![thread_id, crate::now().saturating_mul(1000)],
            )?;
        } else {
            self.conn.execute("DELETE FROM inspected_threads WHERE thread_id = ?1", [thread_id])?;
        }
        debug_assert_eq!(self.thread_inspected(thread_id).ok(), Some(on));
        Ok(())
    }

    /// The inspected threads, most recently turned on first.
    pub fn inspected_threads(&self) -> Result<Vec<String>, Error> {
        let mut st = self.conn.prepare("SELECT thread_id FROM inspected_threads ORDER BY since_ms DESC, thread_id LIMIT ?1")?;
        let rows = st.query_map([sql_limit(INSPECTED_THREADS_MAX)], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_is_inspected_only_once_turned_on_and_until_turned_off() {
        let store = Store::open_memory().unwrap();
        assert!(!store.thread_inspected("T1").unwrap());
        store.set_thread_inspected("T1", true).unwrap();
        store.set_thread_inspected("T1", true).unwrap();
        assert!(store.thread_inspected("T1").unwrap());
        assert!(!store.thread_inspected("T2").unwrap(), "one thread's switch is its own");
        assert_eq!(store.inspected_threads().unwrap(), ["T1"]);
        store.set_thread_inspected("T1", false).unwrap();
        assert!(!store.thread_inspected("T1").unwrap());
        assert!(store.inspected_threads().unwrap().is_empty());
        assert!(store.set_thread_inspected("", true).is_err());
    }
}
