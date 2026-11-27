// Axel's web panes.  One page, three views (`<body data-view>`): the
// conversation, the composer and the activity pane of a thread view.  Each
// renders the thread its pane's server sends (`/api/thread`, the terminal
// view's own model), refetches it when the event stream says it changed, and
// sends what you do through the server, which routes it as the terminal
// composer does.
"use strict";

const VIEW = document.body.dataset.view;
const $ = (id) => document.getElementById(id);
let thread = null;

// ---------------------------------------------------------------- text

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

// A little Markdown, on escaped text only: code fences, inline code, bold,
// headings, lists and links.  Nothing an agent writes becomes markup.
function markdown(text) {
  return esc(text)
    .split(/```/)
    .map((block, i) => {
      if (i % 2 === 1) return `<pre><code>${block.replace(/^[\w+.-]*\n/, "").replace(/\n$/, "")}</code></pre>`;
      return block.split(/\n{2,}/).filter((p) => p.trim()).map(paragraph).join("");
    })
    .join("");
}

function paragraph(p) {
  const lines = p.split("\n").filter((l) => l.trim() !== "");
  if (lines.length && lines.every((l) => /^\s*[-*] /.test(l))) return `<ul>${lines.map((l) => `<li>${inline(l.replace(/^\s*[-*] /, ""))}</li>`).join("")}</ul>`;
  if (lines.length && lines.every((l) => /^\s*\d+[.)] /.test(l))) return `<ol>${lines.map((l) => `<li>${inline(l.replace(/^\s*\d+[.)] /, ""))}</li>`).join("")}</ol>`;
  const heading = /^#{1,6} (.*)$/.exec(p.trim());
  if (heading && lines.length === 1) return `<h3>${inline(heading[1])}</h3>`;
  return `<p>${lines.map(inline).join("<br>")}</p>`;
}

function inline(s) {
  return s
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/\bhttps?:\/\/[^\s<]+/g, (u) => `<a href="${u}" target="_blank" rel="noreferrer">${u}</a>`);
}

// Colours are classes, not inline styles, which the page's security policy
// refuses: `c-<colour>` sets `--agent` to the terminal palette entry the
// terminal view uses for it.
const COLORS = ["purple", "yellow", "red", "green", "blue", "gray", "orange"];
const cc = (a) => `c-${a && COLORS.includes(a.color) ? a.color : "none"}`;
const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });

function compact(n) {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (n >= 1e6) return `${Math.round(n / 1e6)}M`;
  if (n >= 1e3) return `${Math.round(n / 1e3)}k`;
  return String(n);
}
const money = (usd) => `~$${(usd || 0).toFixed(2)}`;

// Nothing moves.  Ladybird sends the whole pane again for any change, as a
// frame of several megabytes, so even a spinner ticking four times a second
// keeps it busy enough to make scrolling lag.  A working agent gets a still
// mark; the page redraws only when the thread changes.
const spinner = () => '<span class="spinner">⠿</span>';

// ---------------------------------------------------------------- messages

// Folds the reader opened stay open across updates.
const openFolds = new Set();
// What the timeline shows, one entry per child of `.column`: only the
// entries that changed are replaced, so an update while the reader scrolls
// costs a few nodes, not the whole thread.
let shown = [];

function renderMessages() {
  $("title").textContent = thread.title;
  document.title = `${thread.title} · Axel`;
  $("live").innerHTML = thread.agents.map(liveHtml).join("");
  const timeline = $("timeline");
  const atBottom = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80;
  let column = timeline.firstElementChild;
  if (!column) {
    column = document.createElement("div");
    column.className = "column";
    timeline.appendChild(column);
  }
  patch(column, timelineBlocks());
  if (atBottom) timeline.scrollTop = timeline.scrollHeight;
  $("jump").hidden = atBottom;
}

function liveHtml(a) {
  const state = a.state === "working" ? spinner() : a.state === "blocked" ? '<span class="state fg c-yellow">!</span>' : "";
  return `<span class="${cc(a)}"><span class="fg">${esc(a.name)}</span> ${state}</span>`;
}

// Replace the children of `column` from the first block that differs.
function patch(column, blocks) {
  let same = 0;
  while (same < blocks.length && same < shown.length && blocks[same].key === shown[same].key && blocks[same].html === shown[same].html) same++;
  while (column.children.length > same) column.lastElementChild.remove();
  const holder = document.createElement("div");
  for (const block of blocks.slice(same)) {
    holder.innerHTML = block.html;
    column.append(...holder.childNodes);
  }
  shown = blocks;
}

function timelineBlocks() {
  const items = thread.items;
  if (items.length === 0) {
    const name = (thread.known_agents[0] || {}).name || "atlas";
    return [{ key: "empty", html: `<div class="empty">nothing here yet · write <b>@${esc(name)}</b> and what you want done, below</div>` }];
  }
  const out = [];
  if (thread.skipped) out.push({ key: "skipped", html: `<div class="skipped">${thread.skipped} earlier items not shown</div>` });
  let run = [];
  const flush = () => {
    if (run.length) out.push({ key: `s-${run[0].id}`, html: stepsHtml(run) });
    run = [];
  };
  for (const it of items) {
    if (it.kind === "tool_call") {
      run.push(it);
      continue;
    }
    flush();
    const html = itemHtml(it);
    if (html) out.push({ key: it.id, html });
  }
  flush();
  const working = thread.agents.filter((a) => a.state === "working");
  if (working.length) {
    const names = working.map((a) => `<span class="fg ${cc(a)}">${esc(a.name)}</span>`).join(", ");
    out.push({ key: "working", html: `<div class="working">${spinner()} ${names} ${working.length === 1 ? "is" : "are"} working</div>` });
  }
  // Every block is one element: `patch` counts them as the column's children.
  return out;
}

function itemHtml(it) {
  const b = it.body;
  switch (it.kind) {
    case "user_message":
    case "assistant_message":
    case "error":
      return b.type === "text" ? turnHtml(it) : "";
    case "thought":
      return b.type === "text" ? thoughtHtml(it) : "";
    case "permission":
    case "question":
      return b.type === "choice" ? askHtml(it) : "";
    case "turn_end":
      return b.type === "changes" ? changesHtml(b) : "";
    default:
      return "";
  }
}

function turnHtml(it) {
  const tag = it.steering ? '<span class="tag">read at its next step</span>' : it.status === "pending" ? '<span class="tag">queued</span>' : "";
  if (it.kind === "user_message" && !it.agent) {
    return `<div class="you"><span class="prompt">&gt;</span><div class="text">${markdown(it.body.text)}</div>${tag}<span class="time">${clock(it.at_ms)}</span></div>`;
  }
  const name = esc(it.agent ? it.agent.name : "agent");
  const to = it.relay_to ? `<span class="dim">to ${esc(it.relay_to)}</span>` : "";
  return `<div class="msg ${it.kind === "error" ? "error" : ""}"><div class="head"><span class="badge ${cc(it.agent)}">${name}</span>${to}${tag}<span class="time">${clock(it.at_ms)}</span></div>
    <div class="text">${markdown(it.body.text)}</div></div>`;
}

function fold(id, summary, body) {
  return `<details data-fold="${esc(id)}"${openFolds.has(id) ? " open" : ""}><summary>${summary}</summary>${body}</details>`;
}

function thoughtHtml(it) {
  const first = it.body.text.split("\n")[0].slice(0, 120);
  return fold(`t-${it.id}`, `· thinking <span class="what">${esc(first)}</span>`, `<div class="said">${markdown(it.body.text)}</div>`).replace("<details", '<details class="thought"');
}

// "Edit a.rs, run cargo test +3", as the terminal view sums up a run.
function runSummary(run) {
  const parts = [...new Set(run.map((t) => t.body.summary || t.body.title).filter(Boolean))];
  const lower = (s, i) => (i === 0 ? s : s.charAt(0).toLowerCase() + s.slice(1));
  const head = parts.slice(0, 3).map(lower).join(", ");
  return parts.length > 3 ? `${head} +${parts.length - 3}` : head;
}

function stepsHtml(run) {
  const failed = run.filter((t) => t.status === "failed").length;
  const running = run.some((t) => t.status === "running" || t.status === "pending");
  const tail = [run.length > 1 ? `${run.length} calls` : "", failed ? `<span class="fg c-red">${failed} failed</span>` : ""].filter(Boolean).join(" · ");
  const summary = `${running ? spinner() : "↗"} <span class="what">${esc(runSummary(run))}</span>${tail ? ` · ${tail}` : ""}`;
  return fold(`s-${run[0].id}`, summary, `<div class="steps">${run.map(stepHtml).join("")}</div>`);
}

function stepHtml(t) {
  const b = t.body;
  const mark = t.status === "failed" ? '<span class="mark failed">✗</span>' : t.status === "running" || t.status === "pending" ? '<span class="mark running">·</span>' : '<span class="mark ok">✓</span>';
  const output = b.output && b.output.length ? `<pre>${esc(b.output.join("\n"))}</pre>` : "";
  return `<div class="step">${mark}<div class="what">${esc(b.summary || b.title)}${output}</div></div>`;
}

function askHtml(it) {
  const b = it.body;
  const who = it.agent ? esc(it.agent.name) : "an agent";
  const open = !b.chosen && (it.status === "pending" || it.status === null);
  if (!open) return `<div class="ask done">${esc(b.prompt)} · ${b.chosen ? `✓ ${esc(b.chosen)}` : "answered"}</div>`;
  const kind = it.kind === "permission" ? "asks to go ahead" : "asks you";
  const options = b.options
    .map((o) => `<button type="button" data-answer="${esc(o.id)}" data-item="${esc(it.id)}" data-session="${esc(it.session_id)}">${esc(o.label)}</button>`)
    .join("");
  return `<div class="ask"><div class="q"><small>! ${who} ${kind}</small>${esc(b.prompt)}</div><div class="options">${options}</div></div>`;
}

function changesHtml(b) {
  const files = b.files === 1 ? "1 file" : `${b.files} files`;
  return `<div class="changes">↗ ${files} <span class="add">+${b.added}</span> <span class="del">−${b.removed}</span></div>`;
}

function setupMessages() {
  const timeline = $("timeline");
  timeline.addEventListener("toggle", (e) => {
    const id = e.target.dataset && e.target.dataset.fold;
    if (id) e.target.open ? openFolds.add(id) : openFolds.delete(id);
  }, true);
  timeline.addEventListener("click", async (e) => {
    const button = e.target.closest("[data-answer]");
    if (!button) return;
    button.disabled = true;
    try {
      await post("answer", { session_id: button.dataset.session, item_id: button.dataset.item, option_id: button.dataset.answer });
    } catch (err) {
      button.disabled = false;
      alert(err.message);
    }
  });
  timeline.addEventListener("scroll", () => {
    if (timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80) $("jump").hidden = true;
  }, { passive: true });
  $("jump").addEventListener("click", () => {
    timeline.scrollTop = timeline.scrollHeight;
    $("jump").hidden = true;
  });
}

// ---------------------------------------------------------------- composer

const input = $("input");
// The most messages remembered for ↑ in an empty composer.
const HISTORY_MAX = 50;
const history = [];
let historyAt = -1;
let busy = false;

// Who gets what's typed, as the server will decide it (`dispatch`): the
// agent named first, else the live managed session whose agent replied last.
function target(text) {
  const named = /^@([\w-]+)/.exec(text.trim());
  if (named) {
    const live = thread.live.find((s) => s.managed && s.agent.toLowerCase() === named[1].toLowerCase());
    return { name: named[1], live: Boolean(live) };
  }
  const live = thread.live.filter((s) => s.managed).sort((a, b) => (b.last_reply_at || 0) - (a.last_reply_at || 0))[0];
  if (live) return { name: live.agent, live: true, mode: live.mode };
  // Nothing live: the server summons the thread's last agent back.
  return thread.last_agent ? { name: thread.last_agent, live: false } : null;
}

function renderComposer() {
  const t = target(input.value);
  const agent = t && thread.known_agents.find((a) => a.name.toLowerCase() === t.name.toLowerCase());
  $("target").innerHTML = t
    ? `<span class="badge ${cc(agent)}">${esc(t.name)}</span>${t.live ? "" : "summons it"}`
    : "no agent on this thread yet";
  $("mode").textContent = t && t.mode ? t.mode : "";
  const working = thread.agents.some((a) => a.state === "working");
  $("stop").hidden = !working;
  $("send").textContent = working ? "queue" : "send";
}

function setStatus(text, error) {
  const s = $("status");
  s.textContent = text || "";
  s.classList.toggle("error", Boolean(error));
}

async function send(steer) {
  const text = input.value.trim();
  if (!text || busy) return;
  busy = true;
  $("send").disabled = true;
  setStatus(steer ? "interrupting…" : "sending…");
  try {
    const reply = await post("send", { text, steer });
    history.push(text);
    if (history.length > HISTORY_MAX) history.shift();
    historyAt = -1;
    input.value = "";
    setStatus(reply.note || "sent");
    renderComposer();
  } catch (e) {
    setStatus(e.message, true);
  } finally {
    busy = false;
    $("send").disabled = false;
    input.focus();
  }
}

async function stop() {
  const working = thread && thread.live.find((s) => s.managed);
  if (!working) return;
  try {
    await post("cancel", { session_id: working.id });
    setStatus("stopping…");
  } catch (e) {
    setStatus(e.message, true);
  }
}

function mentionQuery() {
  const m = /(^|\s)@([\w-]*)$/.exec(input.value.slice(0, input.selectionStart));
  return m ? m[2].toLowerCase() : null;
}

function suggest() {
  const box = $("suggest");
  const q = mentionQuery();
  const names = q === null || !thread ? [] : thread.known_agents.filter((a) => a.name.toLowerCase().startsWith(q)).slice(0, 8);
  box.hidden = names.length === 0;
  box.innerHTML = names.map((a, i) => `<div class="${i === 0 ? "on" : ""} ${cc(a)}" data-name="${esc(a.name)}"><span class="fg">@${esc(a.name)}</span></div>`).join("");
}

function complete(name) {
  const at = input.selectionStart;
  const before = input.value.slice(0, at).replace(/@[\w-]*$/, `@${name} `);
  input.value = before + input.value.slice(at);
  input.selectionStart = input.selectionEnd = before.length;
  $("suggest").hidden = true;
  renderComposer();
  input.focus();
}

function moveSuggestion(step) {
  const rows = [...$("suggest").children];
  const at = rows.findIndex((r) => r.classList.contains("on"));
  if (at < 0) return;
  rows[at].classList.remove("on");
  rows[(at + step + rows.length) % rows.length].classList.add("on");
}

function setupComposer() {
  input.addEventListener("input", () => {
    suggest();
    if (thread) renderComposer();
  });
  input.addEventListener("keydown", (e) => {
    const box = $("suggest");
    if (!box.hidden) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        moveSuggestion(e.key === "ArrowDown" ? 1 : -1);
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        const on = box.querySelector(".on");
        if (on) {
          e.preventDefault();
          complete(on.dataset.name);
          return;
        }
      }
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send(e.metaKey || e.ctrlKey);
    } else if (e.key === "Escape") {
      if (!box.hidden) box.hidden = true;
      else stop();
    } else if (e.key === "ArrowUp" && input.value === "" && history.length) {
      e.preventDefault();
      historyAt = historyAt < 0 ? history.length - 1 : Math.max(0, historyAt - 1);
      input.value = history[historyAt];
    }
  });
  $("suggest").addEventListener("mousedown", (e) => {
    const row = e.target.closest("[data-name]");
    if (row) {
      e.preventDefault();
      complete(row.dataset.name);
    }
  });
  $("send").addEventListener("click", () => send(false));
  $("stop").addEventListener("click", stop);
  input.focus();
}

// ---------------------------------------------------------------- activity

// A dot matrix, as the terminal view's braille sparks are, drawn crisp:
// `values` in `COLUMNS` columns of `ROWS` dots, lit from the bottom.
const SPARK_COLUMNS = 20;
const SPARK_ROWS = 4;
const DOT = 5;
function sparkDots(values) {
  const per = Math.max(1, Math.ceil((values || []).length / SPARK_COLUMNS));
  const columns = [];
  for (let c = 0; c < SPARK_COLUMNS; c++) columns.push((values || []).slice(c * per, (c + 1) * per).reduce((x, y) => x + y, 0));
  const max = Math.max(...columns);
  let dots = "";
  columns.forEach((v, c) => {
    const lit = max > 0 && v > 0 ? Math.max(1, Math.round((v / max) * SPARK_ROWS)) : 0;
    for (let r = 0; r < SPARK_ROWS; r++) {
      dots += `<circle cx="${c * DOT + 2.5}" cy="${(SPARK_ROWS - 1 - r) * DOT + 2.5}" r="1.6"${r < lit ? ' class="on"' : ""}/>`;
    }
  });
  return `<svg class="dots spark" viewBox="0 0 ${SPARK_COLUMNS * DOT} ${SPARK_ROWS * DOT}">${dots}</svg>`;
}

// A meter of ten dots: `fraction` lit, yellow past 80%, red past 95%.
function meterDots(fraction) {
  const lit = Math.round(Math.max(0, Math.min(1, fraction)) * 10);
  const hot = fraction > 0.95 ? "full" : fraction > 0.8 ? "hot" : "on";
  let dots = "";
  for (let i = 0; i < 10; i++) dots += `<circle cx="${i * DOT + 2.5}" cy="2.5" r="1.8"${i < lit ? ` class="${hot}"` : ""}/>`;
  return `<svg class="dots meter" viewBox="0 0 ${10 * DOT} ${DOT}">${dots}</svg>`;
}

const pct = (fraction) => `${Math.round(fraction * 100)}%`;
const heat = (fraction) => (fraction > 0.95 ? "full" : fraction > 0.8 ? "hot" : "");

function renderActivity() {
  const u = thread.usage;
  const states = new Map(thread.agents.map((a) => [a.name, a.state]));
  const byName = new Map(u.agents.map((a) => [a.name, a]));
  const names = [...new Set([...thread.agents.map((a) => a.name), ...u.agents.map((a) => a.name)])];
  const rows = names.map((name) => {
    const spend = byName.get(name) || { total: { tokens: 0, cost_usd: 0 }, spark: [] };
    const known = thread.agents.find((a) => a.name === name) || spend;
    const state = states.get(name) || "idle";
    const mark = state === "working" ? spinner() : state === "blocked" ? "!" : state === "error" ? "✗" : "";
    const ctx = spend.context;
    const context = ctx && ctx.size ? `<span class="context">${meterDots(ctx.used / ctx.size)}<span class="${heat(ctx.used / ctx.size)}">${pct(ctx.used / ctx.size)}</span></span>` : "<span></span>";
    return `<div class="agent ${cc(known)}">
      <span class="name"><span class="fg">${esc(name)}</span> <span class="state ${esc(state)}">${mark}</span></span>
      ${context}<span class="n tokens">${compact(spend.total.tokens)}</span><span class="n cost">${money(spend.total.cost_usd)}</span>
      ${sparkDots(spend.spark)}</div>`;
  });
  const limits = u.limits.flatMap((p) => p.windows.filter((w) => w.used_pct !== null).map((w) => {
    const f = w.used_pct / 100;
    const label = { five_hour: "5h", week: "wk", month: "mo" }[w.window] || esc(w.window);
    return `<span class="limit">${label} ${meterDots(f)}<span class="${heat(f)}">${Math.round(w.used_pct)}%</span></span>`;
  })).join("");
  $("activity-body").innerHTML = `
    <div class="head-row"><span></span><span>ctx</span><span>tok</span><span>~$</span><span>1h</span></div>
    ${rows.join("") || '<div class="dim">no agent yet</div>'}
    ${limits ? `<div class="plan">${limits}</div>` : ""}`;
}

// ---------------------------------------------------------------- talk to the server

async function post(action, body) {
  const r = await fetch(`/api/${action}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const reply = await r.json().catch(() => ({ error: `the pane answered ${r.status}` }));
  if (!r.ok || reply.error) throw new Error(reply.error || `the pane answered ${r.status}`);
  return reply;
}

const RENDER = { messages: renderMessages, composer: renderComposer, activity: renderActivity };

async function load() {
  const r = await fetch("/api/thread", { cache: "no-store" });
  if (!r.ok) throw new Error(`the pane answered ${r.status}`);
  thread = await r.json();
  if (thread.error) throw new Error(thread.error);
  RENDER[VIEW]();
}

function report(e) {
  if (VIEW === "composer") setStatus(e.message, true);
  else console.error(e);
}

if (VIEW === "messages") setupMessages();
if (VIEW === "composer") setupComposer();
load().catch(report).finally(() => {
  const events = new EventSource("/api/events");
  events.onmessage = () => load().catch(report);
});
