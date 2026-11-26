//! Scheduled work: the time math with a fake clock, and the projections.

use axel_identity::Keypair;
use axel_inbox::EnvelopeBuilder;

use crate::schedule::*;
use crate::{
    Harness, NewSession, RuntimeMode, SECONDS_PER_DAY, SECONDS_PER_HOUR, SessionRuntime, StatusReport, StatusState,
    Store, kind,
};

/// A UTC midnight (2024-10-04T00:00:00Z): every test clock starts here.
const DAY: i64 = 20_000 * SECONDS_PER_DAY;
const HOUR: i64 = SECONDS_PER_HOUR;
const MINUTE: i64 = 60;

fn spec(when: ScheduleWhen) -> ScheduleSpec {
    ScheduleSpec {
        schedule_id: "01SCHED".into(),
        to: "pk-reviewer".into(),
        thread: RunThread::Existing("thread-1".into()),
        prompt: "review yesterday's merges".into(),
        when,
        runs_max: RUNS_MAX_DEFAULT,
        until: None,
        device: "env-home".into(),
    }
}

fn schedule(when: ScheduleWhen, created_at: i64) -> Schedule {
    Schedule { spec: spec(when), event_id: "ev".into(), author: "pk-owner".into(), created_at, cancelled_at: None }
}

fn run(run_number: u32, fired_at: i64) -> ScheduleRun {
    ScheduleRun { schedule_id: "01SCHED".into(), run_number, event_id: format!("run-{run_number}"), fired_at }
}

const HOURLY: ScheduleWhen = ScheduleWhen::Every { interval_secs: 3600 };

#[test]
fn an_every_is_not_due_before_its_interval_and_due_on_it() {
    let s = schedule(HOURLY, DAY);
    assert_eq!(plan_run(&s, None, DAY), RunCheck::NotDue { next_at: DAY + HOUR });
    assert_eq!(plan_run(&s, None, DAY + HOUR - 1), RunCheck::NotDue { next_at: DAY + HOUR });
    assert_eq!(plan_run(&s, None, DAY + HOUR), RunCheck::Due { run_number: 1, missed_runs: 0 });
    // Fired on time: the next falls an interval later, counted from creation.
    let first = run(1, DAY + HOUR + 20);
    assert_eq!(plan_run(&s, Some(&first), DAY + HOUR + 30), RunCheck::NotDue { next_at: DAY + 2 * HOUR });
    assert_eq!(plan_run(&s, Some(&first), DAY + 2 * HOUR), RunCheck::Due { run_number: 2, missed_runs: 0 });
}

#[test]
fn runs_missed_during_a_long_sleep_coalesce_into_one() {
    let s = schedule(HOURLY, DAY);
    let first = run(1, DAY + HOUR);
    // Asleep for ten hours and a bit: nine more fell due, one run is sent.
    let check = plan_run(&s, Some(&first), DAY + 11 * HOUR + 5);
    assert_eq!(check, RunCheck::Due { run_number: 2, missed_runs: 9 });
    let text = run_text("summarise", 9);
    assert!(text.starts_with("summarise\n\n"));
    assert!(text.contains("9 earlier runs were missed"), "{text}");
    assert!(text.contains("all 10"), "{text}");
    assert!(run_text("summarise", 1).contains("1 earlier run was missed"));
    assert_eq!(run_text("summarise", 0), "summarise");
}

#[test]
fn a_daily_run_crosses_midnight_with_its_offset() {
    // 00:15 at +02:00 is 22:15 UTC the day before.
    let early = ScheduleWhen::Daily { hh: 0, mm: 15, tz_offset_minutes: 120 };
    let s = schedule(early, DAY + 22 * HOUR);
    let slot = DAY + 22 * HOUR + 15 * MINUTE;
    assert_eq!(plan_run(&s, None, DAY + 22 * HOUR + 14 * MINUTE), RunCheck::NotDue { next_at: slot });
    assert_eq!(plan_run(&s, None, slot), RunCheck::Due { run_number: 1, missed_runs: 0 });
    let fired = run(1, slot + 30);
    assert_eq!(plan_run(&s, Some(&fired), slot + HOUR), RunCheck::NotDue { next_at: slot + SECONDS_PER_DAY });
    // Three days later: two whole days were missed.
    assert_eq!(plan_run(&s, Some(&fired), slot + 3 * SECONDS_PER_DAY), RunCheck::Due { run_number: 2, missed_runs: 2 });

    // 23:45 at -05:00 is 04:45 UTC the day after.
    let late = ScheduleWhen::Daily { hh: 23, mm: 45, tz_offset_minutes: -300 };
    let s = schedule(late, DAY + HOUR);
    assert_eq!(plan_run(&s, None, DAY + HOUR), RunCheck::NotDue { next_at: DAY + 4 * HOUR + 45 * MINUTE });

    // Made after today's slot: the first run is tomorrow's, not a late one today.
    let nine = ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: 0 };
    let s = schedule(nine, DAY + 10 * HOUR);
    assert_eq!(plan_run(&s, None, DAY + 23 * HOUR), RunCheck::NotDue { next_at: DAY + SECONDS_PER_DAY + 9 * HOUR });
}

#[test]
fn runs_max_until_and_once_finish_a_schedule() {
    let mut s = schedule(HOURLY, DAY);
    s.spec.runs_max = 2;
    assert_eq!(plan_run(&s, Some(&run(1, DAY + HOUR)), DAY + 5 * HOUR), RunCheck::Due { run_number: 2, missed_runs: 3 });
    assert_eq!(plan_run(&s, Some(&run(2, DAY + 5 * HOUR)), DAY + 9 * HOUR), RunCheck::Finished(FinishReason::RunsMax));

    let mut s = schedule(HOURLY, DAY);
    s.spec.until = Some(DAY + 90 * MINUTE);
    assert_eq!(plan_run(&s, None, DAY + HOUR), RunCheck::Due { run_number: 1, missed_runs: 0 });
    // The next run would fall after `until`: nothing more will come.
    assert_eq!(plan_run(&s, Some(&run(1, DAY + HOUR)), DAY + 80 * MINUTE), RunCheck::Finished(FinishReason::Until));
    // Past `until`, even a run that fell due before it is not sent.
    assert_eq!(plan_run(&s, None, DAY + 2 * HOUR), RunCheck::Finished(FinishReason::Until));

    let once = schedule(ScheduleWhen::Once { at: DAY + HOUR }, DAY);
    assert_eq!(plan_run(&once, None, DAY), RunCheck::NotDue { next_at: DAY + HOUR });
    assert_eq!(plan_run(&once, None, DAY + 3 * HOUR), RunCheck::Due { run_number: 1, missed_runs: 0 });
    assert_eq!(plan_run(&once, Some(&run(1, DAY + 3 * HOUR)), DAY + 4 * HOUR), RunCheck::Finished(FinishReason::Done));

    let mut cancelled = schedule(HOURLY, DAY);
    cancelled.cancelled_at = Some(DAY + 1);
    assert_eq!(plan_run(&cancelled, None, DAY + 5 * HOUR), RunCheck::Finished(FinishReason::Cancelled));
}

#[test]
fn a_clock_that_went_back_never_counts_a_run_twice() {
    let s = schedule(HOURLY, DAY);
    // The run was stamped before the schedule (another device's clock).
    assert_eq!(plan_run(&s, Some(&run(1, DAY - HOUR)), DAY + 30 * MINUTE), RunCheck::NotDue { next_at: DAY + HOUR });
}

#[test]
fn values_the_math_is_not_defined_for_are_refused() {
    assert!(ScheduleWhen::Every { interval_secs: INTERVAL_SECS_MIN - 1 }.validate().is_err());
    assert!(ScheduleWhen::Every { interval_secs: INTERVAL_SECS_MIN }.validate().is_ok());
    assert!(ScheduleWhen::Every { interval_secs: INTERVAL_SECS_MAX + 1 }.validate().is_err());
    assert!(ScheduleWhen::Daily { hh: 24, mm: 0, tz_offset_minutes: 0 }.validate().is_err());
    assert!(ScheduleWhen::Daily { hh: 23, mm: 60, tz_offset_minutes: 0 }.validate().is_err());
    assert!(ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: TZ_OFFSET_MINUTES_MAX }.validate().is_ok());
    assert!(ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: -TZ_OFFSET_MINUTES_MAX - 1 }.validate().is_err());
    assert!(ScheduleWhen::Once { at: -1 }.validate().is_err());
    assert!(ScheduleWhen::Once { at: TIMESTAMP_MAX + 1 }.validate().is_err());

    let mut bad = spec(HOURLY);
    bad.runs_max = 0;
    assert!(bad.validate().is_err());
    bad.runs_max = RUNS_MAX_LIMIT + 1;
    assert!(bad.validate().is_err());
    let mut bad = spec(HOURLY);
    bad.prompt = "  ".into();
    assert!(bad.validate().is_err());
    let mut bad = spec(HOURLY);
    bad.thread = RunThread::New(NewThreadSpec { title: String::new(), projects: vec![] });
    assert!(bad.validate().is_err());
}

#[test]
fn the_wire_shape_matches_the_rfd() {
    let mut s = spec(ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: 60 });
    s.thread = RunThread::New(NewThreadSpec { title: "Dependency audit".into(), projects: vec!["/src/axel".into()] });
    let json = serde_json::to_value(s.to_wire()).unwrap();
    assert_eq!(json["op"], "add");
    assert_eq!(json["when"], serde_json::json!({"daily": {"hh": 9, "mm": 0, "tz_offset_minutes": 60}}));
    assert_eq!(json["new_thread"]["title"], "Dependency audit");
    assert!(json.get("thread").is_none());
    let every = serde_json::to_value(ScheduleWhen::Every { interval_secs: 90 }).unwrap();
    assert_eq!(every, serde_json::json!({"every": {"interval_s": 90}}));
    let back = ScheduleSpec::from_wire(serde_json::from_value(json).unwrap()).unwrap();
    assert_eq!(back, s);
    // Without runs_max: the default.
    let mut wire = spec(HOURLY).to_wire();
    wire.runs_max = None;
    assert_eq!(ScheduleSpec::from_wire(wire).unwrap().runs_max, RUNS_MAX_DEFAULT);
    let cancel = serde_json::to_value(ScheduleWire::cancel("01SCHED")).unwrap();
    assert_eq!(cancel, serde_json::json!({"schedule_id": "01SCHED", "op": "cancel"}));
}

// --- the store ------------------------------------------------------------------

fn store() -> Store {
    Store::open_memory().expect("open_memory")
}

fn pk(kp: &Keypair) -> String {
    kp.public().to_base58()
}

fn run_note(signer: &Keypair, to: &str, thread: &str, run_number: u32, at: i64, text: &str) -> axel_inbox::Envelope {
    EnvelopeBuilder::new(kind::NOTE, text)
        .mention(to)
        .thread(thread)
        .tag(run_tag("01SCHED", run_number))
        .created_at(u64::try_from(at).unwrap())
        .sign(signer)
        .unwrap()
}

#[test]
fn a_schedule_is_projected_and_cancelled_by_its_author_only() {
    let s = store();
    let (owner, stranger) = (Keypair::generate(), Keypair::generate());
    let added = s.add_schedule(&owner, &spec(HOURLY), DAY).unwrap();
    assert_eq!((added.author.as_str(), added.created_at, added.cancelled_at), (pk(&owner).as_str(), DAY, None));
    assert_eq!(s.schedules(SCHEDULES_LIST_MAX).unwrap(), vec![added.clone()]);
    assert_eq!(s.check_schedule(&added, DAY + HOUR).unwrap(), RunCheck::Due { run_number: 1, missed_runs: 0 });

    assert!(s.cancel_schedule(&stranger, "01SCHED", DAY + 10).is_err(), "not the author");
    assert!(s.cancel_schedule(&owner, "01NOPE", DAY + 10).is_err(), "no such schedule");
    s.cancel_schedule(&owner, "01SCHED", DAY + 10).unwrap();
    let cancelled = s.schedule("01SCHED").unwrap().unwrap();
    assert_eq!(cancelled.cancelled_at, Some(DAY + 10));
    assert_eq!(s.check_schedule(&cancelled, DAY + HOUR).unwrap(), RunCheck::Finished(FinishReason::Cancelled));
}

#[test]
fn a_cancel_that_arrives_before_its_add_still_cancels() {
    let (home, replica) = (store(), store());
    let owner = Keypair::generate();
    home.add_schedule(&owner, &spec(HOURLY), DAY).unwrap();
    home.cancel_schedule(&owner, "01SCHED", DAY + 10).unwrap();
    let mut events: Vec<_> = home.events_since(0, 100).unwrap().into_iter().map(|(_, e)| e).collect();
    events.reverse();
    for e in &events {
        replica.append(e).unwrap();
    }
    assert_eq!(replica.schedule("01SCHED").unwrap().unwrap().cancelled_at, Some(DAY + 10));
}

#[test]
fn a_malformed_or_unfireable_schedule_is_rejected() {
    let s = store();
    let owner = Keypair::generate();
    let garbage = EnvelopeBuilder::new(kind::SCHEDULE, "{not json").sign(&owner).unwrap();
    assert!(s.append(&garbage).unwrap().rejected_reason.unwrap().contains("malformed"));
    let mut wire = spec(HOURLY).to_wire();
    wire.when = Some(ScheduleWhen::Every { interval_secs: 5 });
    let fast = EnvelopeBuilder::new(kind::SCHEDULE, serde_json::to_string(&wire).unwrap()).sign(&owner).unwrap();
    assert!(s.append(&fast).unwrap().rejected_reason.unwrap().contains("invalid schedule"));
    assert!(s.schedules(SCHEDULES_LIST_MAX).unwrap().is_empty());
}

#[test]
fn a_second_run_with_the_same_number_is_dropped_at_ingest() {
    let s = store();
    let (owner, other_device) = (Keypair::generate(), Keypair::generate());
    let reviewer = pk(&Keypair::generate());
    let thread = s.create_thread(&owner, "Reviews", "daily reviews", None).unwrap();
    let first = run_note(&owner, &reviewer, &thread.id, 1, DAY, "review");
    s.post(&first).unwrap();
    // Re-appending the same note is idempotent, not a duplicate.
    assert!(s.append(&first).unwrap().rejected_reason.is_none());

    let twin = run_note(&other_device, &reviewer, &thread.id, 1, DAY + 5, "review");
    let appended = s.append(&twin).unwrap();
    assert!(!appended.inserted);
    assert!(appended.rejected_reason.unwrap().contains("already sent"));
    assert!(s.get_event(&twin.id).unwrap().is_none(), "dropped, not stored");
    assert_eq!(s.inbox(&reviewer, 10).unwrap().len(), 1);
    assert_eq!(s.last_schedule_run("01SCHED").unwrap().unwrap().event_id, first.id);

    s.post(&run_note(&owner, &reviewer, &thread.id, 2, DAY + 10, "review")).unwrap();
    let runs = s.schedule_runs("01SCHED", 10).unwrap();
    assert_eq!(runs.iter().map(|r| (r.run_number, r.fired_at)).collect::<Vec<_>>(), vec![(2, DAY + 10), (1, DAY)]);
}

#[test]
fn schedules_and_runs_survive_a_rebuild() {
    let s = store();
    let owner = Keypair::generate();
    let thread = s.create_thread(&owner, "Reviews", "daily reviews", None).unwrap();
    s.add_schedule(&owner, &spec(HOURLY), DAY).unwrap();
    s.post(&run_note(&owner, "pk-reviewer", &thread.id, 1, DAY + HOUR, "review")).unwrap();
    let (before, runs_before) = (s.schedules(10).unwrap(), s.schedule_runs("01SCHED", 10).unwrap());
    s.rebuild_projections().unwrap();
    assert_eq!(s.schedules(10).unwrap(), before);
    assert_eq!(s.schedule_runs("01SCHED", 10).unwrap(), runs_before);
    assert_eq!(runs_before.len(), 1);
}

#[test]
fn a_run_waits_while_the_previous_runs_session_is_working() {
    let s = store();
    let (owner, reviewer) = (Keypair::generate(), Keypair::generate());
    let thread = s.create_thread(&owner, "Reviews", "daily reviews", None).unwrap();
    let mut armed = spec(HOURLY);
    armed.to = pk(&reviewer);
    let schedule = s.add_schedule(&owner, &armed, DAY).unwrap();
    s.post(&run_note(&owner, &pk(&reviewer), &thread.id, 1, DAY + HOUR, "review")).unwrap();
    let session = s
        .start_session(
            &owner,
            &NewSession {
                thread_id: thread.id.clone(),
                agent_pubkey: pk(&reviewer),
                environment_id: "env-home".into(),
                environment_label: "home".into(),
                project_repo_key: None,
                harness: Harness::Claude,
                runtime_mode: RuntimeMode::Autopilot,
                resumed_from: None,
                runtime: SessionRuntime::Pane,
            },
        )
        .unwrap();
    let report = |state| StatusReport { state, kind: None, title: None, msg: None, progress: None };
    s.record_status(&owner, &session.id, &report(StatusState::Working)).unwrap();
    let busy = s.check_schedule(&schedule, DAY + 2 * HOUR).unwrap();
    assert_eq!(busy, RunCheck::Busy { session_id: session.id.clone() });

    s.record_status(&owner, &session.id, &report(StatusState::Idle)).unwrap();
    // What fell due while it worked arrives as one run.
    assert_eq!(s.check_schedule(&schedule, DAY + 3 * HOUR).unwrap(), RunCheck::Due { run_number: 2, missed_runs: 1 });
}
