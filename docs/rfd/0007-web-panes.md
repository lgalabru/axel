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
(see `RESEARCH.md`, "terminal-browser"). The owner asked for a spike of web
panes, with terminal panes kept as an option. The renderer is now Ladybird's
own terminal mode instead (see "What we learned", 2026-11-26), and web panes
are off until the owner turns them on.

## Sketch

What this branch builds:

- **The same three panes, as web pages.** A thread view keeps the terminal
  view's layout and Rex labels: the conversation above, the composer below,
  and the activity (tokens) pane beside the composer. Each runs
  `axel web pane THREAD --view messages|composer|activity`, which:
  - serves the thread on `127.0.0.1` from that pane's own process;
  - runs `Ladybird --headless=terminal --temporary-profile
    --device-pixel-ratio R URL` in the same pane, drawing the page with the
    Kitty graphics protocol. `R` comes from the pane's pixels per cell, so a
    Retina pane isn't drawn at half size;
  - ends when Ladybird closes, taking the server with it.

  No daemon (ADR 0005). Closing the composer closes the thread view, as the
  terminal composer does: the tokens pane, then the conversation, bringing
  back the shell the thread stood in for.
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
  - Web panes are an experimental runtime flag, off by default:
    `axel panes web` turns them on, `axel panes terminal` off. The choice is
    kept in `$AXEL_HOME/settings.json`, and `$AXEL_PANES` wins over it.
  - When on, they are used only where Ladybird is installed: `$AXEL_LADYBIRD`,
    else `ladybird` on PATH, else `Ladybird.app` in `/Applications` or
    `~/Applications`. Elsewhere thread views stay terminal panes.
- **Falling back.** Each pane shows its terminal counterpart instead (the
  stream, the composer, the tokens screen), saying why, when:
  - Ladybird is missing;
  - or it exits non-zero within 3 s having loaded nothing, as it would in a
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

- **How does it look in a real Rex window?** Rex answers the Kitty graphics
  query with `OK`, and Ladybird draws frames into a pseudo-terminal through
  `axel web pane` (37 frames in 8 s). Nothing here could show those frames in
  a Rex client. To find out, `axel panes web` and dock a thread.
- **Frames by file or inline.** In the pseudo-terminal Ladybird sent full
  frames through temporary files (`t=t`), not just damaged regions. A real
  terminal acknowledges, and Ladybird then patches (`a=f`). A remote Rex
  client gets the server's frames: watch the bandwidth over Tailscale.
- **Its status line.** Terminal mode keeps the pane's last row for its own
  status line and its keys (a URL prompt). Which Axel keys collide? Try each
  composer shortcut in the real thing.
- **Memory and startup.** Ladybird starts eight processes for a pane, about
  285 MB once idle, so a thread view of three web panes is about 850 MB. The
  terminal stream starts in about 50 ms. Measure both, and consider one
  Ladybird page for the conversation and composer together.
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
- **A dependency we don't control.** Ladybird is an alpha (2026-11-23), its
  terminal mode landed on 2026-11-09, and it takes pull requests from
  maintainers only. The fallback keeps Axel working when it breaks, and the
  flag keeps it off unless the owner wants it.

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
- 2026-11-26: Ladybird replaces terminal-browser as the renderer.
  - Ladybird 2026.47.7 has `--headless=terminal` (`LibWebView/TerminalWebView`,
    added on 2026-11-09). It draws frames as Kitty graphics, patching only
    damaged regions; reads SGR-pixel mouse and the Kitty keyboard protocol;
    and follows `SIGWINCH`.
  - It renders this page the same as Chrome, cookie handshake and event
    stream included (a headless screenshot), and costs about 285 MB in
    eight processes, idle. It is BSD-2-Clause, and nothing is bundled.
  - terminal-browser is dropped: it is Electron (136 MB download), reports
    usage by default, and needs installing apart; the owner didn't want Axel
    to manage it either.
  - Embedding Ladybird as a library isn't possible today. Its Rust crates
    are internal to its C++ build, and `HeadlessWebView` and
    `TerminalWebView` are internal classes that change weekly.
  - Web panes are an opt-in runtime flag (`axel panes web`), off by default,
    as the owner asked.
- 2026-11-26: The first web pane held the whole thread in one page, drawn at a
  device pixel ratio of 1. The owner found it unusable: the text was tiny on
  Retina, and the layout was lost. It is now the terminal view's three panes,
  each a web page drawn at the pane's own pixel ratio, redesigned. Checked in
  a throwaway Rex `preview.25` server:
  - `axel thread open` makes the three labelled blocks;
  - each runs Ladybird;
  - closing the composer closes the others, and every Ladybird exits.
- 2026-11-26: Rex `preview.25` panes no longer export `REX_SERVER` or
  `REX_SESSION`. A pane being closed can't ask Rex which session it is in, so
  both composers now take their Rex address when they start, not when they
  hang up. The terminal composer had the same bug with newer Rex.
- 2026-11-26: Ladybird, like Chrome, applied the activity pane's `.agent` rule
  to a turn of the conversation whose class was also `agent`. The activity
  pane's rules are now under `#activity`.
- 2026-11-27: On a phone the text was minuscule, and the owner asked to lose the
  status line and to match the pane's background. Captured from a pty:
  - The status line is `CSI <last row>;1H CSI 2K`, the text, then `CSI 0m`. It is
    redrawn on every load. Frames start with `CSI 1;1H` and an APC. A pty one row
    taller lets us drop that row and nothing else.
  - With no pixels in its terminal size, Ladybird asks `CSI 14 t`. Unanswered, it
    draws an 800×600 frame at ratio 1, which is the minuscule text. We now ask
    first, and start Ladybird again when the ratio changes. We haven't yet seen
    what Rex on a phone reports.
  - Frames are `f=24`, RGB without alpha: a transparent page is impossible.
  - Terminal mode ignores SIGTERM. The pty's master must be close-on-exec, or
    Ladybird holds its own terminal open and outlives the pane.
  - The page's CSP (`style-src 'self'`) refuses an inline `style` attribute.
    Ladybird logs the refusal in `/tmp/ladybird-terminal-PID.log`.
- 2026-11-27: The owner found the text still small, scrolling very slow, and the
  activity pane wordy, and asked for the terminal view's look.
  - Scrolling: idle, the conversation sent 180 full frames in 3 s, from its
    spinner and pulsing dots, each 2040×1700 RGB (10 MB). 20 wheel events made
    908 frames and hadn't settled after 20 s. With no animation and a still
    working mark, an idle pane sends no frames, and 20 wheel events make 29
    frames, settled in 0.74 s. A spinner ticking 4 times a second alone cost 4
    full frames a second.
  - Font: Rex's terminal font is Ghostty's built-in JetBrains Mono, and Rex's
    preferences don't override it. It isn't installed as a system font, so the
    page bundles JetBrains Mono 2.304 from JetBrains' release.
  - Colours: the terminal view draws agents in palette colours (yellow is
    palette 3) and dim text in palette 8, so the page takes the whole palette.
