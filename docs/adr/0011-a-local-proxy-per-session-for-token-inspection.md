# ADR 0011: A local proxy per managed session, to inspect what reaches the model

Status: accepted (2026-11-16).

## Context

Users say Axel "eats tokens", and today nobody can check. Usage comes from
harness transcripts after the fact, as totals per turn: they say how much,
never why. One "say ok" from Claude Code is a 108 KB request (about 27k
tokens: system prompt, tool definitions, the conversation), and the owner can
see none of it.

Both harnesses accept a model endpoint of our choosing while keeping the
user's own login:

- Claude Code honours `ANTHROPIC_BASE_URL` and sends its subscription OAuth
  token (or API key) there, keeping a path prefix (`/<prefix>/v1/messages`).
- Codex ignores `OPENAI_BASE_URL` with a ChatGPT login, but a custom model
  provider (`-c model_provider=…`, `base_url`, `wire_api="responses"`,
  `requires_openai_auth=true`) sends its ChatGPT bearer and
  `chatgpt-account-id` there (`/<prefix>/responses`, `/<prefix>/models`).

## Decision

Each managed session's host (`axel session host`) runs a small HTTP proxy for
the life of the session and points its harness at it. The proxy forwards
every request unchanged to the real provider and, on the way, records one
`model_requests` row per request: model, token usage (from the streamed
response), timings, status, sizes, and a breakdown of the request's context
by kind (system prompt, each tool's definition, user and assistant text, tool
calls and results by tool, images, reasoning). `axel inspect` opens a large
Rex pane that reads those rows: a list of requests and, for the selected
one, what it cost and what its context was made of.

No daemon: the proxy lives and dies with its session (DESIGN principle 4).

### Security

The proxy holds the user's model credentials in flight, so it is built to be
useless to anyone but the harness it serves:

1. **Loopback only.** It binds `127.0.0.1` on a port the OS picks, and drops
   any connection whose peer isn't loopback.
2. **A secret per session.** 32 random bytes, hex, as the first path segment
   (`/<secret>/v1/messages`), compared in constant time. Anything else gets a
   bare `404`. The secret exists only in the host's memory and in the
   harness's environment or arguments.
3. **Not an open proxy.** The upstream is fixed when the session starts
   (`https://api.anthropic.com`, the user's own `ANTHROPIC_BASE_URL` if they
   set one, `https://api.openai.com/v1`, or ChatGPT's Codex backend) and
   never comes from the request: absolute-form URIs, `CONNECT` and any path
   outside an allowlist (`/v1/messages`, `/v1/messages/count_tokens`,
   `/v1/models`, `/responses`, `/models`, …) are refused.
4. **Browsers can't reach it.** Requests carrying an `Origin` header, or a
   `Host` other than `127.0.0.1:<port>` / `localhost:<port>`, are refused,
   against DNS rebinding.
5. **TLS to the provider, verified.** rustls with the webpki roots and the
   provider's hostname; no plaintext upstream.
6. **Nothing secret is stored or logged.** `Authorization`, `x-api-key`,
   `chatgpt-account-id` and cookies are forwarded, never recorded. Bodies are
   parsed in memory for the breakdown and dropped; no body, prompt text or
   tool output reaches the database or a log.
7. **Bounded.** Request bodies over 32 MiB are refused; at most 32
   connections at once; header size limits; connect and idle timeouts. The
   response streams through as it arrives; only a bounded window of each SSE
   line is kept for parsing usage.
8. **It fails open for the user, closed for everyone else.** If the proxy
   can't start, the session runs direct and says so; a request the proxy
   refuses never reaches the provider.

**Off until the owner turns it on, one thread at a time.** The proxy carries
credentials, so it never runs unasked, and the owner chooses what to look at:
`axel inspect on` switches it on for a thread (`--thread`, else the thread it
runs in; `r` in the pane does the same), `axel inspect off` back off,
`axel inspect status` says which (with no thread: which threads are on). The
switch is local-only, in the store's `inspected_threads`. A thread's sessions
read it when they start; a running session keeps what it started with.
`AXEL_INSPECT=1` turns it on for one session, `AXEL_INSPECT=0` off whatever the
switch says, and a switch that can't be read is off. Sessions a remote host runs
read that host's switches, not this machine's. Pane sessions (the harness's own
CLI in a Rex pane) aren't proxied yet: they have no host process to own it.

## Consequences

The owner can see every request, live, with what it cost and why it was that
big, per thread, agent, turn and model. Recording is metadata only, so the
database never holds prompts or code. The proxy is on the critical path of
every model call: a bug there breaks agents, so it is kept small, streams
without buffering, and has its own tests against a fake upstream. Requests
are pruned after 30 days by `axel gc`.

## Network throttling (2026-11-19)

To replay a bad network at a desk, without sudo or system tools, a session's
proxy can be throttled: `AXEL_THROTTLE=airport` (or `train`, `3g`, or a spec
such as `up=1mbit,down=5mbit,rtt=300ms,loss=2`) in the environment of
`axel session host` turns the proxy on for that session and adds half the
round trip each way, paces request bodies and responses to the given rates,
and loses a share of requests by closing the connection before any response
byte, so the harness retries as it would on a real network. It is a testing
aid, never on by default, and changes nothing above: refusals are checked
first, a lost request never reaches the provider, and rows say
`throttled` so slow requests read as what they were.

## Possible future improvements

Opt-in full capture of bodies to files under `~/.axel/inspect/` (mode 0600)
for debugging a single session; proxying pane sessions through a wrapper
process; a web view on the same data.
