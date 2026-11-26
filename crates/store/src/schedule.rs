//! Scheduled work (RFD 0006): messages that arrive in an inbox at a set time.
//!
//! A schedule is a signed `kind 124` envelope ([`ScheduleWire`]), and the
//! `schedules` table is its projection.  A run is an ordinary note from the
//! schedule's author to its recipient, tagged `["sched", schedule_id,
//! run_number]`.  `schedule_runs` records each run as it is ingested, and a
//! second run with the same number is dropped there, so two devices that both
//! fire a schedule deliver each run once.
//!
//! The time math is hand-rolled on Unix seconds: the three cases (`once`,
//! `every`, `daily` at a fixed UTC offset) need no calendar library.
//! [`plan_run`] is pure, and every caller passes "now" in.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::log::{EVENT_COLS, row_envelope};
use crate::types::str_enum;
use crate::wire::kind;
use crate::{
    Envelope, EnvelopeBuilder, Error, Keypair, SECONDS_PER_DAY, SECONDS_PER_HOUR, Store, get_integer, sql_integer,
    sql_limit,
};

/// The tag a scheduled run carries: `["sched", schedule_id, run_number]`.
pub const SCHEDULE_RUN_TAG: &str = "sched";
/// Runs a schedule makes when its author sets no `runs_max` (RFD 0006).
pub const RUNS_MAX_DEFAULT: u32 = 1000;
/// The largest `runs_max` accepted: a run a minute for about ten weeks, so a
/// typo can't arm a schedule that spends for years.
pub const RUNS_MAX_LIMIT: u32 = 100_000;
/// The shortest `every` interval: one minute, the granularity of the cron
/// job that calls `axel schedule tick`.
pub const INTERVAL_SECS_MIN: u64 = 60;
/// The longest `every` interval: a leap year.  Rarer work reads better as a
/// `once`, and the bound keeps the occurrence math far from overflow.
pub const INTERVAL_SECS_MAX: u64 = 366 * SECONDS_PER_DAY as u64;
/// The widest UTC offset in use: UTC+14 (Line Islands).  UTC−12, the other
/// end, is inside the same bound.
pub const TZ_OFFSET_MINUTES_MAX: i32 = 14 * 60;
/// The longest prompt a schedule carries, in bytes: generous for an
/// instruction; a document belongs in the repository, named by the prompt.
pub const PROMPT_BYTES_MAX: usize = 64 * 1024;
/// The longest schedule id: a ULID is 26 characters; the slack admits other
/// generators without admitting a paragraph.
pub const SCHEDULE_ID_CHARS_MAX: usize = 64;
/// Projects a `new_thread` run may attach: more than any thread here holds.
pub const NEW_THREAD_PROJECTS_MAX: usize = 16;
/// The latest instant a schedule may name: 9999-12-31T23:59:59Z, the last
/// second RFC 3339's four-digit years can write.
pub const TIMESTAMP_MAX: i64 = 253_402_300_799;
/// Most schedules [`Store::schedules`] returns: far more than anyone arms by
/// hand, and a bound on what `axel schedule tick` walks each minute.
pub const SCHEDULES_LIST_MAX: usize = 1000;
/// Schedule envelopes read to re-project one schedule: an add and a cancel,
/// with room for replicas that signed either a few more times.
const SCHEDULE_OPS_MAX: usize = 64;
/// Run notes re-recorded by one rebuild: [`RUNS_MAX_LIMIT`] runs for each of
/// [`SCHEDULES_LIST_MAX`] schedules.
const RUNS_REBUILT_MAX: usize = RUNS_MAX_LIMIT as usize * SCHEDULES_LIST_MAX;

str_enum! {
    /// What a `kind 124` envelope does to its schedule.
    ScheduleAction { Add => "add", Cancel => "cancel" }
}

str_enum! {
    /// Why a schedule will send no more runs.
    FinishReason {
        /// Its author cancelled it (`axel schedule rm`).
        Cancelled => "cancelled",
        /// It has sent `runs_max` runs.
        RunsMax => "runs_max",
        /// Its `until` has passed, or its next run would fall after it.
        Until => "until",
        /// A `once` that has run.
        Done => "done"
    }
}

/// When a schedule's runs fall due.  A closed set on purpose: no cron syntax,
/// three cases that cover the examples and parse with no dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleWhen {
    /// One run at `at` (Unix seconds).
    Once { at: i64 },
    /// A run every `interval_secs`, counted from when the schedule was made.
    Every {
        #[serde(rename = "interval_s")]
        interval_secs: u64,
    },
    /// A run each day at `hh:mm` in the zone `tz_offset_minutes` east of UTC.
    /// The offset is fixed: the schedule doesn't follow daylight saving.
    Daily { hh: u8, mm: u8, tz_offset_minutes: i32 },
}

/// The thread a `new_thread` schedule opens for each run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewThreadSpec {
    pub title: String,
    /// Project paths as the author's device wrote them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
}

/// Where a schedule's runs land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunThread {
    /// Every run on this thread.
    Existing(String),
    /// A new thread for every run.
    New(NewThreadSpec),
}

/// `kind 124` content, as on the wire.  An `add` carries every field; a
/// `cancel` only `schedule_id` and `op`.  [`ScheduleSpec::from_wire`] turns
/// an `add` into the checked, typed form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleWire {
    pub schedule_id: String,
    pub op: ScheduleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_thread: Option<NewThreadSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<ScheduleWhen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl ScheduleWire {
    /// The content of a cancel.
    pub fn cancel(schedule_id: &str) -> ScheduleWire {
        ScheduleWire {
            schedule_id: schedule_id.to_string(),
            op: ScheduleAction::Cancel,
            to: None,
            thread: None,
            new_thread: None,
            prompt: None,
            when: None,
            runs_max: None,
            until: None,
            device: None,
        }
    }
}

/// A schedule as its author asked for it, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSpec {
    pub schedule_id: String,
    /// The recipient agent's pubkey.
    pub to: String,
    pub thread: RunThread,
    pub prompt: String,
    pub when: ScheduleWhen,
    pub runs_max: u32,
    /// No run fires after this instant (Unix seconds).
    pub until: Option<i64>,
    /// The environment id of the one device that fires it.
    pub device: String,
}

/// A schedule as projected from the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub spec: ScheduleSpec,
    /// The `add` envelope's id.
    pub event_id: String,
    /// Who signed the `add`: the sender of every run.
    pub author: String,
    /// The `add` envelope's `created_at`: the origin of `every`.
    pub created_at: i64,
    pub cancelled_at: Option<i64>,
}

/// One run a schedule sent, recorded when its note was ingested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleRun {
    pub schedule_id: String,
    /// 1 for the first run; a coalesced run counts once.
    pub run_number: u32,
    /// The run note's id.
    pub event_id: String,
    /// The run note's `created_at`.
    pub fired_at: i64,
}

/// What a schedule should do on this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunCheck {
    /// Send run `run_number` now; `missed_runs` earlier occurrences are
    /// folded into it.
    Due { run_number: u32, missed_runs: u64 },
    /// Due, but the previous run's session is still working: wait for a
    /// later tick, which will coalesce what fell due meanwhile.
    Busy { session_id: String },
    /// Nothing due; the next run falls at `next_at`.
    NotDue { next_at: i64 },
    Finished(FinishReason),
}

fn invalid(what: impl Into<String>) -> Error {
    Error::Invalid(what.into())
}

fn check_timestamp(value: i64, what: &str) -> Result<(), Error> {
    if !(0..=TIMESTAMP_MAX).contains(&value) {
        return Err(invalid(format!("{what} {value} is outside 1970..9999")));
    }
    Ok(())
}

impl ScheduleWhen {
    /// Refuse values the occurrence math is not defined for.
    pub fn validate(self) -> Result<(), Error> {
        match self {
            ScheduleWhen::Once { at } => check_timestamp(at, "once at"),
            ScheduleWhen::Every { interval_secs } => {
                if !(INTERVAL_SECS_MIN..=INTERVAL_SECS_MAX).contains(&interval_secs) {
                    return Err(invalid(format!(
                        "an interval of {interval_secs}s is outside {INTERVAL_SECS_MIN}s..{INTERVAL_SECS_MAX}s"
                    )));
                }
                Ok(())
            }
            ScheduleWhen::Daily { hh, mm, tz_offset_minutes } => {
                if hh >= 24 || mm >= 60 {
                    return Err(invalid(format!("{hh:02}:{mm:02} is not a time of day")));
                }
                if tz_offset_minutes.abs() > TZ_OFFSET_MINUTES_MAX {
                    return Err(invalid(format!("a UTC offset of {tz_offset_minutes} minutes is past ±14:00")));
                }
                Ok(())
            }
        }
    }

    /// For `daily`: the second of the UTC day its local `hh:mm` falls on.
    /// May lie outside `0..86400` (09:00 at +14:00 is the previous UTC day);
    /// the callers' Euclidean division folds it back.
    fn daily_slot_secs(hh: u8, mm: u8, tz_offset_minutes: i32) -> i64 {
        i64::from(hh) * SECONDS_PER_HOUR + i64::from(mm) * 60 - i64::from(tz_offset_minutes) * 60
    }

    /// How many occurrences fall in `(after, through]`.  `anchor` is when the
    /// schedule was made: `every` counts its intervals from there, and the
    /// callers never ask about an `every` before it.
    pub fn occurrences_between(self, anchor: i64, after: i64, through: i64) -> u64 {
        assert!(after >= 0 && through <= TIMESTAMP_MAX, "times are checked before planning");
        if through <= after {
            return 0;
        }
        let count = match self {
            ScheduleWhen::Once { at } => i64::from(after < at && at <= through),
            ScheduleWhen::Every { interval_secs } => {
                debug_assert!(after >= anchor, "an every counts from its anchor");
                let interval = i64::try_from(interval_secs).unwrap_or(i64::MAX);
                (through - anchor).div_euclid(interval) - (after - anchor).div_euclid(interval)
            }
            ScheduleWhen::Daily { hh, mm, tz_offset_minutes } => {
                let slot = Self::daily_slot_secs(hh, mm, tz_offset_minutes);
                (through - slot).div_euclid(SECONDS_PER_DAY) - (after - slot).div_euclid(SECONDS_PER_DAY)
            }
        };
        assert!(count >= 0, "a later bound never has fewer occurrences");
        u64::try_from(count).unwrap_or(0)
    }

    /// The first occurrence strictly after `after`, if there is one.
    pub fn next_after(self, anchor: i64, after: i64) -> Option<i64> {
        let next = match self {
            ScheduleWhen::Once { at } => return (at > after).then_some(at),
            ScheduleWhen::Every { interval_secs } => {
                let interval = i64::try_from(interval_secs).unwrap_or(i64::MAX);
                let from = after.max(anchor);
                anchor + ((from - anchor).div_euclid(interval) + 1) * interval
            }
            ScheduleWhen::Daily { hh, mm, tz_offset_minutes } => {
                let slot = Self::daily_slot_secs(hh, mm, tz_offset_minutes);
                slot + ((after - slot).div_euclid(SECONDS_PER_DAY) + 1) * SECONDS_PER_DAY
            }
        };
        debug_assert!(next > after);
        Some(next)
    }
}

impl ScheduleSpec {
    /// Refuse a schedule no replica could fire as asked.
    pub fn validate(&self) -> Result<(), Error> {
        if self.schedule_id.trim().is_empty() || self.schedule_id.chars().count() > SCHEDULE_ID_CHARS_MAX {
            return Err(invalid(format!("schedule id {:?} is empty or too long", self.schedule_id)));
        }
        if self.to.trim().is_empty() || self.device.trim().is_empty() {
            return Err(invalid("a schedule names its recipient and its device"));
        }
        if self.prompt.trim().is_empty() || self.prompt.len() > PROMPT_BYTES_MAX {
            return Err(invalid(format!("a schedule's prompt is 1 to {PROMPT_BYTES_MAX} bytes")));
        }
        if !(1..=RUNS_MAX_LIMIT).contains(&self.runs_max) {
            return Err(invalid(format!("runs_max {} is outside 1..={RUNS_MAX_LIMIT}", self.runs_max)));
        }
        if let Some(until) = self.until {
            check_timestamp(until, "until")?;
        }
        match &self.thread {
            RunThread::Existing(id) if id.trim().is_empty() => return Err(invalid("a schedule's thread id is empty")),
            RunThread::New(spec) if spec.title.trim().is_empty() => return Err(invalid("a new thread needs a title")),
            RunThread::New(spec) if spec.projects.len() > NEW_THREAD_PROJECTS_MAX => {
                return Err(invalid(format!("a new thread takes at most {NEW_THREAD_PROJECTS_MAX} projects")));
            }
            _ => {}
        }
        self.when.validate()
    }

    /// The wire content of this schedule's `add`.
    pub fn to_wire(&self) -> ScheduleWire {
        let (thread, new_thread) = match &self.thread {
            RunThread::Existing(id) => (Some(id.clone()), None),
            RunThread::New(spec) => (None, Some(spec.clone())),
        };
        ScheduleWire {
            to: Some(self.to.clone()),
            thread,
            new_thread,
            prompt: Some(self.prompt.clone()),
            when: Some(self.when),
            runs_max: Some(self.runs_max),
            until: self.until,
            device: Some(self.device.clone()),
            ..ScheduleWire::cancel(&self.schedule_id)
        }
        .with_op(ScheduleAction::Add)
    }

    /// The checked form of an `add`'s content; `runs_max` defaults to
    /// [`RUNS_MAX_DEFAULT`].
    pub fn from_wire(wire: ScheduleWire) -> Result<ScheduleSpec, Error> {
        if wire.op != ScheduleAction::Add {
            return Err(invalid("only an add describes a schedule"));
        }
        let thread = match (wire.thread, wire.new_thread) {
            (Some(id), None) => RunThread::Existing(id),
            (None, Some(spec)) => RunThread::New(spec),
            _ => return Err(invalid("a schedule names a thread or a new thread, not both or neither")),
        };
        let missing = |field: &str| invalid(format!("a schedule add without {field}"));
        let spec = ScheduleSpec {
            schedule_id: wire.schedule_id,
            to: wire.to.ok_or_else(|| missing("to"))?,
            thread,
            prompt: wire.prompt.ok_or_else(|| missing("prompt"))?,
            when: wire.when.ok_or_else(|| missing("when"))?,
            runs_max: wire.runs_max.unwrap_or(RUNS_MAX_DEFAULT),
            until: wire.until,
            device: wire.device.ok_or_else(|| missing("device"))?,
        };
        spec.validate()?;
        Ok(spec)
    }
}

impl ScheduleWire {
    fn with_op(mut self, op: ScheduleAction) -> ScheduleWire {
        self.op = op;
        self
    }
}

/// What `schedule` should do at `now`, given its newest run.  Pure: the
/// busy check needs the store, see [`Store::check_schedule`].
///
/// Occurrences are counted after the newest run (or the schedule's
/// creation), so a device that slept through several sends one run that says
/// how many it stands for.  Nothing fires once `until` has passed, even an
/// occurrence that fell before it while no device was ticking.
pub fn plan_run(schedule: &Schedule, last_run: Option<&ScheduleRun>, now: i64) -> RunCheck {
    assert!((0..=TIMESTAMP_MAX).contains(&now), "now {now} is outside 1970..9999");
    debug_assert!(last_run.is_none_or(|r| r.schedule_id == schedule.spec.schedule_id));
    let spec = &schedule.spec;
    if schedule.cancelled_at.is_some() {
        return RunCheck::Finished(FinishReason::Cancelled);
    }
    let runs_done = last_run.map_or(0, |r| r.run_number);
    if runs_done >= spec.runs_max {
        return RunCheck::Finished(FinishReason::RunsMax);
    }
    if matches!(spec.when, ScheduleWhen::Once { .. }) && runs_done >= 1 {
        return RunCheck::Finished(FinishReason::Done);
    }
    if spec.until.is_some_and(|until| now > until) {
        return RunCheck::Finished(FinishReason::Until);
    }
    // A clock that went back never lets a run count twice.
    let since = last_run.map_or(schedule.created_at, |r| r.fired_at).max(schedule.created_at);
    let due = match spec.when {
        // A `once` set in the past still runs once.
        ScheduleWhen::Once { at } => u64::from(at <= now),
        when => when.occurrences_between(schedule.created_at, since, now),
    };
    if due > 0 {
        return RunCheck::Due { run_number: runs_done + 1, missed_runs: due - 1 };
    }
    match spec.when.next_after(schedule.created_at, since.max(now)) {
        Some(next_at) if spec.until.is_none_or(|until| next_at <= until) => RunCheck::NotDue { next_at },
        _ => RunCheck::Finished(FinishReason::Until),
    }
}

/// The text of a run: the prompt, and how many runs it stands for when
/// some were missed.
pub fn run_text(prompt: &str, missed_runs: u64) -> String {
    match missed_runs {
        0 => prompt.to_string(),
        1 => format!("{prompt}\n\n(Scheduled run: 1 earlier run was missed while no device was ticking; this run stands for both.)"),
        n => format!(
            "{prompt}\n\n(Scheduled run: {n} earlier runs were missed while no device was ticking; this run stands for all {}.)",
            n.saturating_add(1)
        ),
    }
}

/// The tag of run `run_number` of `schedule_id`.
pub fn run_tag(schedule_id: &str, run_number: u32) -> Vec<String> {
    assert!(run_number >= 1, "runs are numbered from 1");
    vec![SCHEDULE_RUN_TAG.to_string(), schedule_id.to_string(), run_number.to_string()]
}

/// `(schedule_id, run_number)` of a note that is a scheduled run.  Other
/// kinds, and tags that don't parse, are not runs.
pub fn parse_run_tag(env: &Envelope) -> Option<(String, u32)> {
    if env.kind != kind::NOTE {
        return None;
    }
    let tag = env.tags.iter().find(|t| t.first().map(String::as_str) == Some(SCHEDULE_RUN_TAG))?;
    let schedule_id = tag.get(1).filter(|id| !id.is_empty())?;
    let run_number: u32 = tag.get(2)?.parse().ok().filter(|n| *n >= 1)?;
    Some((schedule_id.clone(), run_number))
}

const SCHEDULE_COLS: &str = "schedule_id, event_id, author, created_at, cancelled_at, content";

/// `event_id, author, created_at, cancelled_at, content` of a `schedules` row.
type ScheduleRow = (String, String, i64, Option<i64>, String);

fn row_schedule(r: &Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok((r.get("event_id")?, r.get("author")?, r.get("created_at")?, r.get("cancelled_at")?, r.get("content")?))
}

/// A `schedules` row as a [`Schedule`]; content that no longer parses is
/// corruption, since only valid adds are projected.
fn to_schedule(row: ScheduleRow) -> Result<Schedule, Error> {
    let (event_id, author, created_at, cancelled_at, content) = row;
    let wire: ScheduleWire = serde_json::from_str(&content)?;
    let spec = ScheduleSpec::from_wire(wire).map_err(|e| Error::Corrupt(format!("schedule {event_id}: {e}")))?;
    Ok(Schedule { spec, event_id, author, created_at, cancelled_at })
}

fn row_run(r: &Row<'_>) -> rusqlite::Result<ScheduleRun> {
    Ok(ScheduleRun {
        schedule_id: r.get("schedule_id")?,
        run_number: get_integer(r, "run_number")?,
        event_id: r.get("event_id")?,
        fired_at: r.get("fired_at")?,
    })
}

impl Store {
    /// Sign and project a schedule.  `now` (Unix seconds) stamps the envelope
    /// and is the origin of an `every`.
    pub fn add_schedule(&self, signer: &Keypair, spec: &ScheduleSpec, now: i64) -> Result<Schedule, Error> {
        spec.validate()?;
        check_timestamp(now, "now")?;
        let content = serde_json::to_string(&spec.to_wire())?;
        let created_at = u64::try_from(now).map_err(|_| invalid("now is negative"))?;
        let env = EnvelopeBuilder::new(kind::SCHEDULE, content).created_at(created_at).sign(signer)?;
        self.post(&env)?;
        let schedule = self
            .schedule(&spec.schedule_id)?
            .ok_or_else(|| Error::Corrupt(format!("schedule {} was posted but not projected", spec.schedule_id)))?;
        assert_eq!(schedule.spec.schedule_id, spec.schedule_id);
        Ok(schedule)
    }

    /// Sign a cancel.  Only the schedule's author may cancel it; anyone
    /// else's cancel would be stored and ignored, so it is refused here.
    pub fn cancel_schedule(&self, signer: &Keypair, schedule_id: &str, now: i64) -> Result<(), Error> {
        check_timestamp(now, "now")?;
        let schedule = self.schedule(schedule_id)?.ok_or_else(|| Error::NotFound(format!("schedule {schedule_id}")))?;
        if schedule.author != signer.public().to_base58() {
            return Err(Error::Unauthorized(format!("only the author of schedule {schedule_id} may cancel it")));
        }
        let content = serde_json::to_string(&ScheduleWire::cancel(schedule_id))?;
        let created_at = u64::try_from(now).map_err(|_| invalid("now is negative"))?;
        let env = EnvelopeBuilder::new(kind::SCHEDULE, content).created_at(created_at).sign(signer)?;
        self.post(&env)
    }

    /// One schedule by id, cancelled or not.
    pub fn schedule(&self, schedule_id: &str) -> Result<Option<Schedule>, Error> {
        let row = self
            .conn
            .query_row(&format!("SELECT {SCHEDULE_COLS} FROM schedules WHERE schedule_id = ?1"), [schedule_id], row_schedule)
            .optional()?;
        row.map(to_schedule).transpose()
    }

    /// Schedules, oldest first, at most `limit` (capped at [`SCHEDULES_LIST_MAX`]).
    pub fn schedules(&self, limit: usize) -> Result<Vec<Schedule>, Error> {
        let limit = limit.min(SCHEDULES_LIST_MAX);
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {SCHEDULE_COLS} FROM schedules ORDER BY created_at ASC, schedule_id ASC LIMIT ?1"))?;
        let rows: Vec<_> = statement.query_map([sql_limit(limit)], row_schedule)?.collect::<Result<_, _>>()?;
        debug_assert!(rows.len() <= limit);
        rows.into_iter().map(to_schedule).collect()
    }

    /// A schedule's runs, newest first, at most `limit`.
    pub fn schedule_runs(&self, schedule_id: &str, limit: usize) -> Result<Vec<ScheduleRun>, Error> {
        let mut statement = self.conn.prepare(
            "SELECT schedule_id, run_number, event_id, fired_at FROM schedule_runs
             WHERE schedule_id = ?1 ORDER BY run_number DESC LIMIT ?2",
        )?;
        let runs: Vec<ScheduleRun> =
            statement.query_map(params![schedule_id, sql_limit(limit)], row_run)?.collect::<Result<_, _>>()?;
        debug_assert!(runs.windows(2).all(|w| w[0].run_number > w[1].run_number));
        Ok(runs)
    }

    /// A schedule's newest run, if it has sent one.
    pub fn last_schedule_run(&self, schedule_id: &str) -> Result<Option<ScheduleRun>, Error> {
        Ok(self.schedule_runs(schedule_id, 1)?.into_iter().next())
    }

    /// [`plan_run`], and then whether the previous run's session is still
    /// working: the recipient's live session on that run's thread reports
    /// `working`, or is mid-turn (`running`, which the hooks and the session
    /// host set while a turn runs and clear when it ends).  A session stuck
    /// `starting` doesn't count, or one failed launch would hold the
    /// schedule forever.
    pub fn check_schedule(&self, schedule: &Schedule, now: i64) -> Result<RunCheck, Error> {
        let last = self.last_schedule_run(&schedule.spec.schedule_id)?;
        let check = plan_run(schedule, last.as_ref(), now);
        let (RunCheck::Due { .. }, Some(last)) = (&check, &last) else {
            return Ok(check);
        };
        let busy: Option<String> = self
            .conn
            .query_row(
                "SELECT s.id FROM events e
                 JOIN sessions s ON s.thread_id = e.thread_id AND s.agent_pubkey = ?2 AND s.ended_at IS NULL
                 LEFT JOIN status st ON st.session_id = s.id
                 WHERE e.id = ?1 AND (st.state = 'working' OR s.state = 'running')
                 ORDER BY s.started_at DESC LIMIT 1",
                params![last.event_id, schedule.spec.to],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match busy {
            Some(session_id) => RunCheck::Busy { session_id },
            None => check,
        })
    }
}

/// Project a `kind 124` envelope.  Returns why it is rejected, if it is:
/// content that is not a schedule op, or an add that could never fire.
pub(crate) fn project_schedule(conn: &Connection, env: &Envelope) -> Result<Option<String>, Error> {
    debug_assert_eq!(env.kind, kind::SCHEDULE);
    let wire: ScheduleWire = match serde_json::from_str(&env.content) {
        Ok(wire) => wire,
        Err(e) => return Ok(Some(format!("malformed content for kind {}: {e}", env.kind))),
    };
    let schedule_id = wire.schedule_id.clone();
    if wire.op == ScheduleAction::Add
        && let Err(e) = ScheduleSpec::from_wire(wire)
    {
        return Ok(Some(format!("invalid schedule: {e}")));
    }
    if schedule_id.trim().is_empty() {
        return Ok(Some("a schedule op without a schedule_id".into()));
    }
    reproject_schedule(conn, &schedule_id)?;
    Ok(None)
}

/// Rebuild one `schedules` row from every op on it, whatever order they
/// arrived in: the first valid add in `(created_at, id)` order defines it,
/// and a cancel by that add's author cancels it, even one stamped earlier
/// (a cancel is never undone by clock skew).
fn reproject_schedule(conn: &Connection, schedule_id: &str) -> Result<(), Error> {
    conn.execute("DELETE FROM schedules WHERE schedule_id = ?1", [schedule_id])?;
    let ops: Vec<Envelope> = {
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE kind = ?1 AND instr(content, ?2) > 0
             ORDER BY created_at ASC, id ASC LIMIT ?3"
        ))?;
        let rows = statement.query_map(params![kind::SCHEDULE, schedule_id, sql_limit(SCHEDULE_OPS_MAX)], row_envelope)?;
        rows.collect::<Result<_, _>>()?
    };
    let parsed: Vec<(&Envelope, ScheduleWire)> = ops
        .iter()
        .filter_map(|e| serde_json::from_str::<ScheduleWire>(&e.content).ok().map(|w| (e, w)))
        .filter(|(_, w)| w.schedule_id == schedule_id)
        .collect();
    let Some((add, add_wire)) = parsed
        .iter()
        .find(|(_, w)| w.op == ScheduleAction::Add && ScheduleSpec::from_wire(w.clone()).is_ok())
    else {
        return Ok(());
    };
    let cancelled_at = parsed
        .iter()
        .filter(|(e, w)| w.op == ScheduleAction::Cancel && e.from == add.from)
        .map(|(e, _)| e.created_at)
        .min();
    let created_at = sql_integer(add.created_at, "schedule created_at")?;
    let cancelled_at = cancelled_at.map(|at| sql_integer(at, "schedule cancelled_at")).transpose()?;
    conn.execute(
        "INSERT INTO schedules (schedule_id, event_id, author, to_pubkey, device, created_at, cancelled_at, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            schedule_id,
            add.id,
            add.from,
            add_wire.to.as_deref().unwrap_or_default(),
            add_wire.device.as_deref().unwrap_or_default(),
            created_at,
            cancelled_at,
            serde_json::to_string(add_wire)?,
        ],
    )?;
    Ok(())
}

/// Why `env` is dropped at ingest, if it is a scheduled run whose number
/// another note already took.  Re-appending the same note is not a duplicate.
pub(crate) fn duplicate_run(conn: &Connection, env: &Envelope) -> Result<Option<String>, Error> {
    let Some((schedule_id, run_number)) = parse_run_tag(env) else {
        return Ok(None);
    };
    let taken: Option<String> = conn
        .query_row(
            "SELECT event_id FROM schedule_runs WHERE schedule_id = ?1 AND run_number = ?2",
            params![schedule_id, run_number],
            |r| r.get(0),
        )
        .optional()?;
    Ok(taken
        .filter(|id| *id != env.id)
        .map(|id| format!("run {run_number} of schedule {schedule_id} was already sent as {id}")))
}

/// Record a scheduled run that was just stored.
pub(crate) fn record_run(conn: &Connection, env: &Envelope) -> Result<(), Error> {
    let Some((schedule_id, run_number)) = parse_run_tag(env) else {
        return Ok(());
    };
    let fired_at = sql_integer(env.created_at, "run created_at")?;
    conn.execute(
        "INSERT OR IGNORE INTO schedule_runs (schedule_id, run_number, event_id, fired_at) VALUES (?1, ?2, ?3, ?4)",
        params![schedule_id, run_number, env.id, fired_at],
    )?;
    Ok(())
}

/// Re-record every run in the log after `schedule_runs` was emptied, in
/// arrival order (duplicates were never stored, so order only breaks ties
/// that cannot occur).
pub(crate) fn rebuild_runs(conn: &Connection) -> Result<(), Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE kind = ?1 AND instr(tags_json, ?2) > 0 ORDER BY seq ASC LIMIT ?3"
    ))?;
    let needle = format!("\"{SCHEDULE_RUN_TAG}\"");
    let rows = statement.query_map(params![kind::NOTE, needle, sql_limit(RUNS_REBUILT_MAX)], row_envelope)?;
    for row in rows {
        record_run(conn, &row?)?;
    }
    Ok(())
}
