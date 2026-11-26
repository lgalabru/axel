//! `axel schedule …`: scheduled work (RFD 0006).
//!
//! A schedule is a signed envelope; a run is an ordinary note from the
//! schedule's author to its recipient, sent through the same path as
//! `axel send` (reach, chatter, sign, post, live note or summon).  `tick`
//! sends what is due on this device, and is safe to run every minute from
//! cron: a run is recorded when its note is posted, so a second tick finds
//! nothing new.

use anyhow::{Context, Result, bail};
use axel_mcp::{MessageKind, Outcome, SendReport};
use axel_store::{
    FinishReason, NewThreadSpec, RunCheck, RunThread, SCHEDULES_LIST_MAX, Schedule, ScheduleSpec, ScheduleWhen,
};

use super::Ctx;
use super::inbox::Origin;
use crate::cli::{ScheduleAdd, ScheduleCommand};

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
/// Digits accepted in a duration: nine is past any interval allowed
/// (`INTERVAL_SECS_MAX` is a year, eight digits of seconds).
const DURATION_DIGITS_MAX: usize = 9;
/// Characters of a schedule id shown where a short one is enough to find it.
const SCHEDULE_ID_SHORT_CHARS: usize = 12;

pub fn run(cmd: ScheduleCommand) -> Result<()> {
    let ctx = Ctx::open()?;
    let now = chrono::Utc::now().timestamp();
    match cmd {
        ScheduleCommand::Add(args) => {
            refuse_agent_caller()?;
            let schedule = add(&ctx, &args, now)?;
            println!("{}", schedule.spec.schedule_id);
            println!("{}", describe(&ctx, &schedule, &ctx.store.check_schedule(&schedule, now)?));
            Ok(())
        }
        ScheduleCommand::Ls { all } => list(&ctx, all, now),
        ScheduleCommand::Rm { schedule } => {
            refuse_agent_caller()?;
            let id = resolve(&ctx, &schedule)?;
            ctx.store.cancel_schedule(&ctx.owner()?, &id, now)?;
            println!("cancelled {id}");
            Ok(())
        }
        ScheduleCommand::Tick => {
            refuse_agent_caller()?;
            print_tick(&ctx, &tick(&ctx, now)?)
        }
    }
}

/// Schedules are the owner's: `add` and `rm` sign with the owner key, and
/// `tick` sends as the owner.  Whether agents may schedule is an open
/// question of RFD 0006, so an agent's process is refused here.
fn refuse_agent_caller() -> Result<()> {
    if let Some(agent) = super::policy::caller_agent() {
        bail!("agents can't arm or fire schedules yet (running as {agent}); ask the owner");
    }
    Ok(())
}

fn add(ctx: &Ctx, args: &ScheduleAdd, now: i64) -> Result<Schedule> {
    let name = args.agent.trim().trim_start_matches('@');
    let to = ctx.store.agent_by_name(name)?.with_context(|| format!("no agent named {name:?}"))?.pubkey;
    let thread = match (&args.thread, &args.new) {
        (Some(id), None) => RunThread::Existing(super::thread::resolve(ctx, id)?.id),
        (None, Some(title)) => RunThread::New(NewThreadSpec { title: title.clone(), projects: project_paths(&args.projects)? }),
        _ => bail!("give --thread ID or --new TITLE"),
    };
    let when = parse_when(args, local_offset_minutes())?;
    if let ScheduleWhen::Once { at } = when
        && at <= now
    {
        bail!("--at is in the past");
    }
    let until = args.until.as_deref().map(parse_rfc3339).transpose()?;
    if until.is_some_and(|u| u <= now) {
        bail!("--until is in the past");
    }
    let spec = ScheduleSpec {
        schedule_id: axel_store::new_id(),
        to,
        thread,
        prompt: args.prompt.join(" "),
        when,
        runs_max: args.runs_max.unwrap_or(axel_store::RUNS_MAX_DEFAULT),
        until,
        device: ctx.store.local_environment()?.id,
    };
    Ok(ctx.store.add_schedule(&ctx.owner()?, &spec, now)?)
}

/// The new thread's projects as absolute paths (the run may fire from a cron
/// job in another directory); the current directory when none are given.
fn project_paths(projects: &[std::path::PathBuf]) -> Result<Vec<String>> {
    let projects = if projects.is_empty() { vec![std::env::current_dir()?] } else { projects.to_vec() };
    // A path that can't be canonicalized is still the user's choice; keep it as given.
    Ok(projects.into_iter().map(|p| std::fs::canonicalize(&p).unwrap_or(p).display().to_string()).collect())
}

/// This machine's UTC offset now, in minutes: the default zone of `--daily`.
fn local_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

/// The one of `--at`, `--every` and `--daily` that was given (clap requires
/// exactly one).  `default_offset_minutes` is the zone of a `--daily` without `--tz`.
fn parse_when(args: &ScheduleAdd, default_offset_minutes: i32) -> Result<ScheduleWhen> {
    let when = match (&args.at, &args.every, &args.daily) {
        (Some(at), None, None) => ScheduleWhen::Once { at: parse_rfc3339(at)? },
        (None, Some(every), None) => ScheduleWhen::Every { interval_secs: parse_duration_secs(every)? },
        (None, None, Some(daily)) => {
            let (hh, mm) = parse_time_of_day(daily)?;
            let tz_offset_minutes = match &args.tz {
                Some(tz) => parse_utc_offset_minutes(tz)?,
                None => default_offset_minutes,
            };
            ScheduleWhen::Daily { hh, mm, tz_offset_minutes }
        }
        _ => bail!("give one of --at, --every or --daily"),
    };
    when.validate()?;
    Ok(when)
}

/// RFC 3339 (`2026-11-26T09:00:00+01:00`) as Unix seconds.
fn parse_rfc3339(text: &str) -> Result<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text.trim())
        .with_context(|| format!("{text:?} is not an RFC 3339 time like 2026-11-26T09:00:00+01:00"))?;
    Ok(parsed.timestamp())
}

/// `90s`, `15m`, `2h`, `7d` as seconds.
fn parse_duration_secs(text: &str) -> Result<u64> {
    let text = text.trim();
    let digits_len = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (digits, unit) = text.split_at(digits_len);
    if digits.is_empty() || digits.len() > DURATION_DIGITS_MAX {
        bail!("{text:?} is not a duration like 90s, 15m, 2h or 7d");
    }
    let count: u64 = digits.parse().with_context(|| format!("{text:?} is not a number of units"))?;
    let unit_secs = match unit {
        "s" => 1,
        "m" => SECONDS_PER_MINUTE,
        "h" => SECONDS_PER_HOUR,
        "d" => SECONDS_PER_DAY,
        _ => bail!("{text:?} needs a unit: s, m, h or d"),
    };
    // Nine digits of days fit easily in a u64; checked anyway.
    count.checked_mul(unit_secs).with_context(|| format!("{text:?} is too long"))
}

/// `HH:MM` (24-hour) as hours and minutes.
fn parse_time_of_day(text: &str) -> Result<(u8, u8)> {
    let bad = || format!("{text:?} is not a time like 09:00");
    let (hh, mm) = text.trim().split_once(':').with_context(bad)?;
    if hh.is_empty() || hh.len() > 2 || mm.len() != 2 {
        bail!(bad());
    }
    let (hh, mm): (u8, u8) = (hh.parse().with_context(bad)?, mm.parse().with_context(bad)?);
    if hh >= 24 || mm >= 60 {
        bail!(bad());
    }
    Ok((hh, mm))
}

/// `+01:00`, `-05:30`, `+09` or `Z` as minutes east of UTC.
fn parse_utc_offset_minutes(text: &str) -> Result<i32> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("z") || text.eq_ignore_ascii_case("utc") {
        return Ok(0);
    }
    let bad = || format!("{text:?} is not a UTC offset like +01:00");
    let (sign, rest) = match text.split_at_checked(1) {
        Some(("+", rest)) => (1, rest),
        Some(("-", rest)) => (-1, rest),
        _ => bail!(bad()),
    };
    let (hh, mm) = rest.split_once(':').unwrap_or((rest, "00"));
    if hh.len() != 2 || mm.len() != 2 {
        bail!(bad());
    }
    let (hh, mm): (i32, i32) = (hh.parse().with_context(bad)?, mm.parse().with_context(bad)?);
    if mm >= 60 || hh * 60 + mm > axel_store::TZ_OFFSET_MINUTES_MAX {
        bail!(bad());
    }
    Ok(sign * (hh * 60 + mm))
}

/// A full schedule id, from itself or a unique prefix.
fn resolve(ctx: &Ctx, id_or_prefix: &str) -> Result<String> {
    let all = ctx.store.schedules(SCHEDULES_LIST_MAX)?;
    let matches: Vec<&Schedule> = all.iter().filter(|s| s.spec.schedule_id.starts_with(id_or_prefix.trim())).collect();
    match matches.as_slice() {
        [one] => Ok(one.spec.schedule_id.clone()),
        [] => bail!("no schedule matches {id_or_prefix:?}; see axel schedule ls"),
        many => bail!("{} schedules match {id_or_prefix:?}; be more specific", many.len()),
    }
}

fn list(ctx: &Ctx, all: bool, now: i64) -> Result<()> {
    let schedules = ctx.store.schedules(SCHEDULES_LIST_MAX)?;
    let mut shown = 0usize;
    for schedule in &schedules {
        let check = ctx.store.check_schedule(schedule, now)?;
        if !all && matches!(check, RunCheck::Finished(_)) {
            continue;
        }
        println!("{}", describe(ctx, schedule, &check));
        shown += 1;
    }
    if shown == 0 {
        println!("no schedules armed{}", if all { "" } else { " (--all shows finished ones)" });
    }
    Ok(())
}

/// One line about a schedule: id, recipient, when, where, runs, and what's next.
fn describe(ctx: &Ctx, schedule: &Schedule, check: &RunCheck) -> String {
    let spec = &schedule.spec;
    let id: String = spec.schedule_id.chars().take(SCHEDULE_ID_SHORT_CHARS).collect();
    let place = match &spec.thread {
        RunThread::Existing(thread) => match ctx.store.get_thread(thread) {
            Ok(Some(t)) => format!("on {:?}", t.title),
            // Gone, or not synced here yet: the id still says which.
            _ => format!("on thread {}", thread.chars().take(SCHEDULE_ID_SHORT_CHARS).collect::<String>()),
        },
        RunThread::New(new) => format!("new thread {:?} each run", new.title),
    };
    let runs = ctx.store.last_schedule_run(&spec.schedule_id).ok().flatten().map_or(0, |r| r.run_number);
    let state = match check {
        RunCheck::Due { .. } => "due now".to_string(),
        RunCheck::Busy { .. } => "due, waiting for the last run to finish".to_string(),
        RunCheck::NotDue { next_at } => format!("next {}", local_time(*next_at)),
        RunCheck::Finished(reason) => finished_label(*reason).to_string(),
    };
    let to = super::policy::name_of(ctx, &spec.to);
    format!("{id}  @{to}  {}  {place}  {runs}/{} runs  {state}", describe_when(spec.when), spec.runs_max)
}

fn finished_label(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Cancelled => "cancelled",
        FinishReason::RunsMax => "finished: sent runs_max runs",
        FinishReason::Until => "finished: past --until",
        FinishReason::Done => "done",
    }
}

fn describe_when(when: ScheduleWhen) -> String {
    match when {
        ScheduleWhen::Once { at } => format!("once at {}", local_time(at)),
        ScheduleWhen::Every { interval_secs } => format!("every {}", describe_duration(interval_secs)),
        ScheduleWhen::Daily { hh, mm, tz_offset_minutes } => {
            let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
            let offset = tz_offset_minutes.unsigned_abs();
            format!("daily {hh:02}:{mm:02} {sign}{:02}:{:02}", offset / 60, offset % 60)
        }
    }
}

/// The largest whole unit: `7d`, `2h`, `15m`, else seconds.
fn describe_duration(secs: u64) -> String {
    for (unit, unit_secs) in [("d", SECONDS_PER_DAY), ("h", SECONDS_PER_HOUR), ("m", SECONDS_PER_MINUTE)] {
        if secs.is_multiple_of(unit_secs) {
            return format!("{}{unit}", secs / unit_secs);
        }
    }
    format!("{secs}s")
}

fn local_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// What one schedule did on a tick.
#[derive(Debug)]
pub enum Fired {
    /// Run `run_number` was posted (and delivered as `report` says).
    Sent { run_number: u32, missed_runs: u64, report: SendReport },
    /// Due, but the previous run's session is still working.
    Busy { session_id: String },
    /// Due, but refused before it was posted: it stays due.
    Refused { error: String },
}

/// Send every run that is due on this device at `now`.  Schedules that are
/// not due, finished, or named for another device are left alone.
pub fn tick(ctx: &Ctx, now: i64) -> Result<Vec<(Schedule, Fired)>> {
    let owner = ctx.owner()?.public().to_base58();
    let device = ctx.store.local_environment()?.id;
    let mut fired = Vec::new();
    // `schedules` is capped at SCHEDULES_LIST_MAX, which bounds this loop.
    for schedule in ctx.store.schedules(SCHEDULES_LIST_MAX)? {
        if schedule.spec.device != device {
            continue;
        }
        let outcome = match ctx.store.check_schedule(&schedule, now)? {
            RunCheck::Due { run_number, missed_runs } => match fire(ctx, &owner, &schedule, run_number, missed_runs, now) {
                Ok(report) => Fired::Sent { run_number, missed_runs, report },
                Err(e) => Fired::Refused { error: format!("{e:#}") },
            },
            RunCheck::Busy { session_id } => Fired::Busy { session_id },
            RunCheck::NotDue { .. } | RunCheck::Finished(_) => continue,
        };
        fired.push((schedule, outcome));
    }
    assert!(fired.len() <= SCHEDULES_LIST_MAX);
    Ok(fired)
}

/// Send one run as the schedule's author would send it with `axel send`.
fn fire(ctx: &Ctx, owner: &str, schedule: &Schedule, run_number: u32, missed_runs: u64, now: i64) -> Result<SendReport> {
    assert!(run_number >= 1);
    let spec = &schedule.spec;
    let _acting = act_as_author(ctx, owner, schedule)?;
    let recipients = [spec.to.clone()];
    // Checked before a new thread is opened, so a refusal leaves nothing behind.
    super::policy::check_message(ctx, &recipients)?;
    let thread_id = match &spec.thread {
        RunThread::Existing(id) => id.clone(),
        RunThread::New(new) => {
            let projects: Vec<std::path::PathBuf> = new.projects.iter().map(std::path::PathBuf::from).collect();
            super::thread::record_thread(ctx, &new.title, &projects)?.id
        }
    };
    let origin = Origin {
        thread_id: Some(thread_id),
        tags: vec![axel_store::run_tag(&spec.schedule_id, run_number)],
        created_at: Some(u64::try_from(now).context("now is before 1970")?),
    };
    let text = axel_store::run_text(&spec.prompt, missed_runs);
    super::inbox::send_from(ctx, &origin, &recipients, &[], &text, MessageKind::Request)
}

/// Policy checks for a run treat its author as the caller: the owner (who
/// may message anyone), or one of this machine's agents (whose reach
/// applies, as for any message it sends).  Anyone else's schedule doesn't
/// fire here.
fn act_as_author(ctx: &Ctx, owner: &str, schedule: &Schedule) -> Result<super::policy::ActingAs> {
    if schedule.author == owner {
        return Ok(super::policy::act_as_owner());
    }
    if ctx.store.agent_by_pubkey(&schedule.author)?.is_none() {
        bail!("not scheduled by the owner or one of this machine's agents");
    }
    if matches!(schedule.spec.thread, RunThread::New(_)) {
        // A new thread is the owner's; an agent's run could not post there.
        bail!("only the owner's schedules open new threads");
    }
    Ok(super::policy::act_as(&schedule.author, None))
}

fn print_tick(ctx: &Ctx, fired: &[(Schedule, Fired)]) -> Result<()> {
    let mut problems = 0usize;
    for (schedule, outcome) in fired {
        let id: String = schedule.spec.schedule_id.chars().take(SCHEDULE_ID_SHORT_CHARS).collect();
        let to = super::policy::name_of(ctx, &schedule.spec.to);
        match outcome {
            Fired::Sent { run_number, missed_runs, report } => {
                let missed = if *missed_runs > 0 { format!(" ({missed_runs} missed, coalesced)") } else { String::new() };
                for delivery in &report.deliveries {
                    if let Outcome::Failed { .. } = delivery.outcome {
                        problems += 1;
                        eprintln!("{id}: run {run_number}{missed} to @{to}: {}", delivery.outcome.describe());
                    } else {
                        println!("{id}: run {run_number}{missed} to @{to}: {}", delivery.outcome.describe());
                    }
                }
            }
            Fired::Busy { session_id } => println!("{id}: @{to} is still on the last run (session {session_id}); waiting"),
            Fired::Refused { error } => {
                problems += 1;
                eprintln!("{id}: run to @{to} refused: {error}");
            }
        }
    }
    if problems > 0 {
        bail!("{problems} scheduled run(s) were refused or not delivered");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
