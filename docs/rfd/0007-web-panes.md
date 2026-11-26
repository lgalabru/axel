# RFD 0007: Web panes: the thread view as a web page inside its Rex pane

State: discussion
Authors: astra, for Ludo  ·  Started: 2026-11-26

## Problem

The thread view is terminal text. That gets us native scrollback and copy
(ADR 0010), but some things a conversation wants are hard to draw with cells:
- Markdown with real typography.
- Code blocks that wrap and scroll on their own.
- Tool runs that fold and unfold.
- Images, and later diffs with syntax colour.
- A composer that behaves like a text field: selection, IME, mouse.

The terminal view also takes three panes (stream, composer, tokens), kept in
step by hand (`settle_thread_row`, `fit_compose_pane`, the hang-up handler).

Someone in the community ran a web view in a pane with `terminal-browser`
(see `RESEARCH.md`, "terminal-browser"). The owner asked for a spike: web
panes by default, terminal panes kept as an option.

## Sketch

What this branch builds:

- **One pane, one process.** `axel web pane THREAD`:
  - serves the thread on `127.0.0.1` from this process;
  - runs `terminal-browser open --no-merge URL` in the same pane, which
    draws Chromium with the Kitty graphics protocol;
  - ends when the browser closes, taking the server with it.

  No daemon (ADR 0005), and the pane is the whole thread view: conversation
  and composer in one.
- **The same model as the terminal view.** The page renders
  `axel_tui::load_thread_data`, the `ThreadData` the terminal view draws,
  serialized by `cmd/web/view.rs`. What you type goes through
  `axel_tui::thread::dispatch`, as the composer's does:
  - a prompt to the live managed session whose agent replied last;
  - `@name` to that agent's session, or a summons when it has none.

  Answers and Stop go to the session's socket (`ui::send_to_session`). The two
  views can't disagree about who gets a message.
- **Live without a framework.** One poller thread watches a change marker:
  `Store::change_marker`, the last event `seq` plus the items' revision.
  - When the marker moves, it reloads the view, and in any case every 5 s.
  - When the view changed, it wakes the page over Server-Sent Events, and the
    page refetches `/api/thread`.
  - No dependency added: HTTP/1.1 on `std::net`, about 500 lines with tests.
- **Locked to its pane.**
  - Only `127.0.0.1`, and the `Host` must match, which stops DNS rebinding.
  - A 256-bit secret in the first URL becomes an `HttpOnly`,
    `SameSite=Strict` cookie that every request needs.
  - Writes must be same-origin JSON POSTs.
  - A strict `Content-Security-Policy`: nothing loads from anywhere else, no
    inline script or style.
  - Every read and queue is bounded: headers 16 KiB, body 256 KiB, 32
    connections, 8 event streams, 10 s read timeout.
- **Where it's used.** Docking a thread from the launcher, a thread in its own
  tab, and `axel thread open`.
  - The choice is `axel panes auto|web|terminal`, kept in
    `$AXEL_HOME/settings.json`; `$AXEL_PANES` wins over it.
  - `auto` (the default) means web where `terminal-browser` is installed and
    we're in Rex, else terminal.
- **Falling back.** The pane shows the terminal view instead, saying why, when:
  - `terminal-browser` is missing;
  - or it exits non-zero within 3 s having loaded nothing, as it does in a
    terminal without Kitty graphics.
- **Debugging.** `axel web serve THREAD` prints the address and serves until
  interrupted, for a desktop browser.

Not in this spike:
- The launcher, the dashboard, the inbox and the question popups stay
  terminal panes.
- No attachments from the page, and no tokens pane.
- `relaunch_thread_panes`, which reopens a thread's panes on a new binary,
  still finds only terminal composers.

## Measurements

On a copy of the owner's 359 MB database, the busiest thread has 10 684
conversation items; the view shows the newest 500.
- `load_thread_data` takes 8.1 ms median and 8.4 ms p95 over 20 runs, in a
  release build on an M-series Mac.
- At a 300 ms poll, loading every time would cost about 3% of a core per open
  web pane, all day. The change marker makes a still thread cost two
  single-row reads a poll instead.

## Open questions

- **Does `terminal-browser` draw in Rex as it does in Ghostty?** Its README
  lists Rex among supported terminals, but Rex's docs say nothing about the
  Kitty graphics protocol. It isn't installed on this machine, so the spike
  only checks the contract: the arguments, the page, the cookie, the fallback.
  To find out, `brew install terminal-browser` and `axel panes web`, then dock
  a thread.
- **Startup time.** Electron's offscreen rendering takes about a second to
  come up, against about 50 ms for the terminal stream. Docking feels slower.
  A warm browser per window, opening tabs with `--no-merge` off, would hide
  it, but then the pane and the browser no longer share a lifetime.
- **Telemetry.** `terminal-browser` reports usage by default
  (`terminal-browser config set telemetry.usage off` to stop). Should Axel set
  `DO_NOT_TRACK=1` for the panes it starts, or leave that to the user?
- **Keyboard.** `terminal-browser` takes `cmd+l` and others for its own UI,
  and it has no option to hide its bar ("app mode was removed"). Which Axel
  keys collide? Try each composer shortcut in the real thing.
- **A native web block.** Rex `0.1.0-preview.25` (API revision 24) has no
  browser block. Its creators are `terminal`, `tunnel`, `fs` and
  `onboarding`. It does load executable block plugins (`--block-dir`), whose
  protocol isn't published yet. A Rex-drawn web block would beat Chromium in
  a terminal on cost and input; RFD 0003 already waits on that protocol.
- **The other panes.** If the thread pane holds up, the dashboard and inbox
  are the next candidates. The floating launcher should stay text: it must
  open in well under a second.

## Risks

- **Two views to keep.** Every feature now has a terminal and a web
  rendering. Sharing the model and the dispatch keeps the logic in one place;
  the drawing is double.
- **A local server is attack surface.** It is bounded and locked down as
  above, but it is a socket any local process can reach. Without the secret it
  gets 403s; with it, it can do what the owner can on that one thread.
- **A dependency we don't control.** `terminal-browser` is young (MIT, 77 open
  issues on 2026-11-26) and its CLI already changed once. The fallback keeps
  Axel working when it breaks.

## What we learned

- 2026-11-26: `terminal-browser open` runs in the foreground of its terminal
  until closed, and exits 0. Without `--no-merge` it may adopt the page into a
  browser already open elsewhere and return.
- 2026-11-26: Rex `preview.25` sets `OSC7577_TOKEN` and keeps the commands
  programs report (`com.superlogical.terminal.program_resume_records`).
  - Axel's reports from #45 are accepted there, with a valid signature, and
    are marked `untrusted_reason: no_rule`.
  - Rex resumes only commands a `resume.trust.*` rule in `init.lua` allows,
    and it checks that the reporting process is the program the command
    starts.
  - So Axel must ship trust rules in its Rex module, and an agent's pane
    session, which `exec`s the harness after reporting, fails that check.
    That is a follow-up to #45, not part of this RFD.
