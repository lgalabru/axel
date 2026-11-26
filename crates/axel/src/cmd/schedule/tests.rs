//! `axel schedule`: the argument parsers, and `tick` end to end on an
//! in-memory store with a fake clock.

use axel_identity::{Keypair, Keystore};
use axel_store::{
    AgentPolicy, Harness, NewAgent, NewSession, RunThread, RuntimeMode, SessionRuntime, Store, StatusReport,
    StatusState,
};

use super::*;

/// A UTC midnight (2024-10-04T00:00:00Z): the fake clock starts here.
const DAY: i64 = 20_000 * 86_400;
const HOUR: i64 = 3_600;

#[test]
fn durations_times_and_offsets_parse_at_the_edge() {
    assert_eq!(parse_duration_secs("90s").unwrap(), 90);
    assert_eq!(parse_duration_secs("15m").unwrap(), 900);
    assert_eq!(parse_duration_secs("2h").unwrap(), 7_200);
    assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
    for bad in ["", "m", "15", "15w", "-5m", "1.5h", "1234567890s"] {
        assert!(parse_duration_secs(bad).is_err(), "{bad:?}");
    }
    assert_eq!(parse_time_of_day("09:00").unwrap(), (9, 0));
    assert_eq!(parse_time_of_day("23:59").unwrap(), (23, 59));
    for bad in ["24:00", "12:60", "9", "09:0", "ab:cd", "123:00"] {
        assert!(parse_time_of_day(bad).is_err(), "{bad:?}");
    }
    assert_eq!(parse_utc_offset_minutes("+01:00").unwrap(), 60);
    assert_eq!(parse_utc_offset_minutes("-05:30").unwrap(), -330);
    assert_eq!(parse_utc_offset_minutes("+09").unwrap(), 540);
    assert_eq!(parse_utc_offset_minutes("Z").unwrap(), 0);
    assert_eq!(parse_utc_offset_minutes("+14:00").unwrap(), 840);
    for bad in ["+15:00", "01:00", "+1:00", "+01:60", ""] {
        assert!(parse_utc_offset_minutes(bad).is_err(), "{bad:?}");
    }
    assert_eq!(parse_rfc3339("2024-10-04T09:00:00+01:00").unwrap(), DAY + 8 * HOUR);
    assert!(parse_rfc3339("tomorrow").is_err());
    assert_eq!(describe_duration(604_800), "7d");
    assert_eq!(describe_duration(90), "90s");
    assert_eq!(describe_when(ScheduleWhen::Daily { hh: 9, mm: 5, tz_offset_minutes: -330 }), "daily 09:05 -05:30");
}

#[test]
fn every_is_at_least_a_minute_and_daily_takes_its_zone() {
    let args = |every: Option<&str>, daily: Option<&str>, tz: Option<&str>| ScheduleAdd {
        agent: "@reviewer".into(),
        thread: Some("t".into()),
        new: None,
        projects: vec![],
        at: None,
        every: every.map(String::from),
        daily: daily.map(String::from),
        tz: tz.map(String::from),
        runs_max: None,
        until: None,
        prompt: vec!["go".into()],
    };
    assert!(parse_when(&args(Some("59s"), None, None), 0).is_err());
    assert_eq!(parse_when(&args(Some("60s"), None, None), 0).unwrap(), ScheduleWhen::Every { interval_secs: 60 });
    let daily = parse_when(&args(None, Some("09:00"), None), 60).unwrap();
    assert_eq!(daily, ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: 60 }, "this machine's zone by default");
    let daily = parse_when(&args(None, Some("09:00"), Some("-05:00")), 60).unwrap();
    assert_eq!(daily, ScheduleWhen::Daily { hh: 9, mm: 0, tz_offset_minutes: -300 });
}

/// A machine with an owner, an agent `reviewer` working in a pane on a
/// thread (so a run is posted and waits in its inbox, and nothing is
/// launched), and that thread's id.
struct World {
    ctx: Ctx,
    owner: Keypair,
    reviewer: String,
    thread: String,
    session: String,
    _keys: tempfile::TempDir,
}

fn world() -> World {
    let keys = tempfile::tempdir().unwrap();
    let ctx = Ctx { store: Store::open_memory().unwrap(), keystore: Keystore::open_at(keys.path().join("keys.json"), false).unwrap(), rex: None };
    let owner = ctx.owner().unwrap();
    let reviewer = insert_agent(&ctx, "reviewer");
    let thread = ctx.store.create_thread(&owner, "Reviews", "review the merges", None).unwrap().id;
    let env = ctx.store.local_environment().unwrap();
    let new_session = NewSession {
        thread_id: thread.clone(),
        agent_pubkey: reviewer.clone(),
        environment_id: env.id,
        environment_label: env.label,
        project_repo_key: None,
        harness: Harness::Claude,
        runtime_mode: RuntimeMode::Autopilot,
        resumed_from: None,
        runtime: SessionRuntime::Pane,
    };
    let session = ctx.store.start_session(&owner, &new_session).unwrap().id;
    World { ctx, owner, reviewer, thread, session, _keys: keys }
}

fn insert_agent(ctx: &Ctx, name: &str) -> String {
    let pubkey = Keypair::generate().public().to_base58();
    let agent = NewAgent { pubkey: pubkey.clone(), name: name.into(), harness: Harness::Claude, ..Default::default() };
    ctx.store.insert_agent(&agent).unwrap();
    pubkey
}

impl World {
    fn spec(&self, when: ScheduleWhen) -> ScheduleSpec {
        ScheduleSpec {
            schedule_id: axel_store::new_id(),
            to: self.reviewer.clone(),
            thread: RunThread::Existing(self.thread.clone()),
            prompt: "review yesterday's merges".into(),
            when,
            runs_max: axel_store::RUNS_MAX_DEFAULT,
            until: None,
            device: self.ctx.store.local_environment().unwrap().id,
        }
    }

    fn arm(&self, spec: &ScheduleSpec) -> Schedule {
        self.ctx.store.add_schedule(&self.owner, spec, DAY).unwrap()
    }

    /// `(run_number, missed_runs)` of each run sent, or panics on anything else.
    fn sent(&self, now: i64) -> Vec<(u32, u64)> {
        tick(&self.ctx, now)
            .unwrap()
            .into_iter()
            .map(|(_, fired)| match fired {
                Fired::Sent { run_number, missed_runs, report } => {
                    assert_eq!(report.thread.as_deref(), Some(self.thread.as_str()));
                    assert_eq!(report.deliveries.len(), 1);
                    assert!(matches!(report.deliveries[0].outcome, Outcome::InPane), "{:?}", report.deliveries[0].outcome);
                    (run_number, missed_runs)
                }
                other => panic!("expected a sent run, got {other:?}"),
            })
            .collect()
    }

    fn inbox(&self) -> Vec<axel_store::Envelope> {
        self.ctx.store.inbox(&self.reviewer, 50).unwrap()
    }
}

const HOURLY: ScheduleWhen = ScheduleWhen::Every { interval_secs: 3_600 };

#[test]
fn a_tick_sends_what_is_due_once_however_often_it_runs() {
    let w = world();
    let schedule = w.arm(&w.spec(HOURLY));
    assert!(w.sent(DAY + HOUR - 1).is_empty(), "not due yet");
    assert_eq!(w.sent(DAY + HOUR), vec![(1, 0)]);
    assert!(w.sent(DAY + HOUR).is_empty(), "a second tick sends nothing new");
    assert!(w.sent(DAY + HOUR + 59).is_empty());

    let inbox = w.inbox();
    assert_eq!(inbox.len(), 1);
    let run = &inbox[0];
    assert_eq!(run.from, w.owner.public().to_base58(), "a run is a note from the schedule's author");
    assert_eq!(run.content, "review yesterday's merges");
    assert_eq!(run.thread_id(), Some(w.thread.as_str()));
    assert_eq!(axel_store::parse_run_tag(run), Some((schedule.spec.schedule_id.clone(), 1)));
    assert_eq!(run.created_at, u64::try_from(DAY + HOUR).unwrap(), "stamped with the tick's clock");
}

#[test]
fn a_long_sleep_sends_one_run_that_counts_the_missed_ones() {
    let w = world();
    w.arm(&w.spec(HOURLY));
    assert_eq!(w.sent(DAY + HOUR), vec![(1, 0)]);
    assert_eq!(w.sent(DAY + 9 * HOUR + 30), vec![(2, 7)]);
    let newest = &w.inbox()[0];
    assert!(newest.content.contains("7 earlier runs were missed"), "{}", newest.content);
}

#[test]
fn runs_max_until_and_cancel_stop_the_runs() {
    let w = world();
    let mut capped = w.spec(HOURLY);
    capped.runs_max = 1;
    w.arm(&capped);
    let mut bounded = w.spec(ScheduleWhen::Daily { hh: 0, mm: 30, tz_offset_minutes: 60 });
    bounded.until = Some(DAY + 2 * HOUR);
    w.arm(&bounded);
    let cancelled = w.arm(&w.spec(HOURLY));
    w.ctx.store.cancel_schedule(&w.owner, &cancelled.spec.schedule_id, DAY + 10).unwrap();

    // 00:30 at +01:00 is 23:30 UTC: the bounded one's first run would fall after `until`.
    assert_eq!(w.sent(DAY + HOUR), vec![(1, 0)], "only the capped schedule fires");
    assert!(w.sent(DAY + 30 * HOUR).is_empty(), "capped at one run, past until, cancelled");
    assert_eq!(w.inbox().len(), 1);
}

#[test]
fn only_the_named_device_fires_a_schedule() {
    let w = world();
    let mut elsewhere = w.spec(HOURLY);
    elsewhere.device = "env-laptop".into();
    w.arm(&elsewhere);
    assert!(tick(&w.ctx, DAY + 5 * HOUR).unwrap().is_empty());
    assert!(w.inbox().is_empty());
}

#[test]
fn a_run_waits_while_the_last_one_is_still_working() {
    let w = world();
    w.arm(&w.spec(HOURLY));
    assert_eq!(w.sent(DAY + HOUR), vec![(1, 0)]);
    let report = |state| StatusReport { state, kind: None, title: None, msg: None, progress: None };
    w.ctx.store.record_status(&w.owner, &w.session, &report(StatusState::Working)).unwrap();
    let fired = tick(&w.ctx, DAY + 2 * HOUR).unwrap();
    assert!(matches!(&fired[..], [(_, Fired::Busy { session_id })] if *session_id == w.session), "{fired:?}");
    w.ctx.store.record_status(&w.owner, &w.session, &report(StatusState::Idle)).unwrap();
    assert_eq!(w.sent(DAY + 3 * HOUR), vec![(2, 1)], "what fell due meanwhile comes as one run");
}

#[test]
fn a_refused_reach_sends_nothing_and_stays_due() {
    let w = world();
    // An agent's schedule: its reach applies, and by default it reaches nobody.
    let scout_key = Keypair::generate();
    let scout = scout_key.public().to_base58();
    // Attested by the owner, as `axel agent new` does, so its notes are accepted on the owner's thread.
    let auth_tag = Some(axel_identity::attest(&w.owner, &scout_key.public(), "").unwrap().to_tag());
    let agent = NewAgent { pubkey: scout.clone(), name: "scout".into(), harness: Harness::Claude, auth_tag, ..Default::default() };
    w.ctx.store.insert_agent(&agent).unwrap();
    let agents = w.ctx.store.add_schedule(&scout_key, &w.spec(HOURLY), DAY).unwrap();
    // A stranger's schedule never fires here.
    w.ctx.store.add_schedule(&Keypair::generate(), &w.spec(HOURLY), DAY).unwrap();
    w.arm(&w.spec(HOURLY));

    let fired = tick(&w.ctx, DAY + HOUR).unwrap();
    let refusals: Vec<&str> = fired.iter().filter_map(|(_, f)| if let Fired::Refused { error } = f { Some(error.as_str()) } else { None }).collect();
    assert_eq!(refusals.len(), 2, "{fired:?}");
    assert!(refusals.iter().any(|e| e.contains("scout may not message reviewer")), "{refusals:?}");
    assert!(refusals.iter().any(|e| e.contains("not scheduled by the owner")), "{refusals:?}");
    assert_eq!(fired.iter().filter(|(_, f)| matches!(f, Fired::Sent { .. })).count(), 1, "the owner's still fires");
    assert_eq!(w.inbox().len(), 1);
    assert!(w.ctx.store.last_schedule_run(&agents.spec.schedule_id).unwrap().is_none(), "nothing recorded");

    // Once the owner lets scout reach reviewer, the run goes out as scout's.
    let mut policy = AgentPolicy::default();
    policy.reach.allow(&w.reviewer);
    w.ctx.store.set_agent_policy(&scout, &policy).unwrap();
    w.ctx.keystore.put_agent(&scout_key).unwrap();
    let fired = tick(&w.ctx, DAY + HOUR + 1).unwrap();
    let sent: Vec<&Schedule> = fired.iter().filter(|(_, f)| matches!(f, Fired::Sent { .. })).map(|(s, _)| s).collect();
    assert_eq!(sent.iter().map(|s| s.spec.schedule_id.as_str()).collect::<Vec<_>>(), vec![agents.spec.schedule_id.as_str()]);
    assert_eq!(w.inbox()[0].from, scout);
}
