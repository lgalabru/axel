# RFD 0006: Scheduled work: messages that arrive in an inbox at a set time

State: idea
Authors: Ludo  ·  Started: 2026-11-25

## Problem

Nothing in Axel starts or continues work at a time: a nightly dependency review, a
morning summary of open threads, a check an hour after a deploy. Issue #40 points out
that T3 Code has a `schedule_task` tool and Orca has `orca automations`, and proposes
cron plus the CLI (`axel thread new`, `axel send`). That works today, but the schedule
lives outside the log: no one on the thread sees it, it doesn't follow the owner to
another device, and two machines with the same crontab fire twice.

Agents already have inboxes, and delivery already knows what to do with a message: a
live session takes it as a note, otherwise the agent is summoned. A scheduled run should
be one more message, not a new kind of work.

## Sketch

A **schedule** is a signed envelope. A **run** is an ordinary message that arrives in
the recipient's inbox when it falls due.

```
axel schedule add @reviewer --thread 01JD… --daily 09:00 "review yesterday's merges"
axel schedule add @scout --new "Dependency audit" --project ~/Coding/axel --every 7d "…"
axel schedule ls
axel schedule rm SCHEDULE
```

- **Wire.** `kind 124 schedule`, signed by the owner or one of its devices, content
  `{schedule_id, op: add|cancel, to, thread | new_thread, prompt, when, runs_max,
  until, device}`. `when` is a closed set: `once(at)`, `every(interval_s)`,
  `daily(HH:MM, tz)`. No cron syntax: three cases cover the examples and parse with no
  dependency.
- **A run** is a kind-1 note from the owner to `to`, tagged
  `["sched", schedule_id, run_number]`. It goes through the same checks and delivery as
  `axel send`: reach, depth, budget, live note or summon. The agent sees a message, and
  the thread shows who scheduled it.
- **One device fires.** The schedule names a device (RFD 0004), this one by default.
  Other devices see the schedule and never fire it. Before linked devices exist, the
  device is simply the home.
- **What fires it.** `axel schedule tick` sends every run that is due and records
  `run_number`, so running it twice sends nothing new. It is all a timer needs:
  - a cron job or systemd timer calling `axel schedule tick` every minute, today;
  - the long-running `axel sync serve` block of RFD 0004, later, which ticks itself.
- **Missed runs coalesce.** A laptop asleep through three daily runs sends one on wake,
  marked with how many were missed, not three.
- **Limits.** `interval_s` is at least 60, the cron granularity. `runs_max` defaults to
  1000. A run never starts while the previous run's session is still working; it waits
  for the next tick.

Not in scope: event triggers (on push, on CI failure). A bot or a webhook can call
`axel send` for those today.

## Open questions

- **Who signs a run?** The owner key signing unattended is what RFD 0004 wants to stop.
  With device keys, the firing device signs as itself. Before that, the owner key.
  Settles with RFD 0004 phase 2.
- **Should agents schedule?** T3's `schedule_task` is an agent tool. An agent asking to
  be woken in an hour is useful, and dangerous without a bound. Option: an MCP
  `schedule` tool limited to `once`, within the agent's budget, to itself or agents it
  may reach. Find out whether #40's coordinator would use it.
- **Time zones on a host.** `daily(09:00, tz)` carries its zone, so the firing device's
  clock zone doesn't matter. Check the `jiff` or `time` cost against hand-rolled
  offsets before choosing.

## Risks

- **A schedule spends while no one watches.** Budget checks already apply to each run,
  and `axel schedule ls` shows what's armed. The dashboard should show armed schedules
  next to the token pane.
- **Two firing devices** after a botched device revoke. The `run_number` tag lets every
  replica drop a second run with the same number.

## What we learned

- 2026-11-25: #40 proposed cron or a chat bot calling the CLI, and the owner asked to
  build on inboxes. Both fit: `axel schedule tick` under cron is the first step, and the
  run is an inbox message.
