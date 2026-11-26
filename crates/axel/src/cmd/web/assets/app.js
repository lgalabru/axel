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

// Colours are classes, not inline styles, which the page's security policy refuses.
const COLORS = ["purple", "yellow", "red", "green", "blue", "gray", "orange"];
const fg = (a) => (a && COLORS.includes(a.color) ? `fg-${a.color}` : "");
const bg = (a) => (a && COLORS.includes(a.color) ? `bg-${a.color}` : "");
const initial = (name) => esc((name || "?").trim().charAt(0).toUpperCase());
const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

function compact(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return String(n);
}
const money = (usd) => `$${(usd || 0).toFixed(2)}`;

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  return `${Math.round(s / 3600)}h`;
}

const STATE_LABEL = { working: "working", blocked: "needs you", error: "error", done: "done", idle: "idle" };

// Small line icons, by tool kind.
const ICONS = {
  read: '<path d="M3 2.5h6l3 3v8H3z"/><path d="M9 2.5v3h3"/>',
  edit: '<path d="M10.5 2.5l3 3-7.5 7.5H3v-3z"/>',
  delete: '<path d="M3 4.5h10M6 4.5V3h4v1.5M4.5 4.5l.7 9h5.6l.7-9"/>',
  move: '<path d="M2.5 8h11M10.5 5l3 3-3 3"/>',
  search: '<circle cx="7" cy="7" r="4"/><path d="M10 10l3.5 3.5"/>',
  execute: '<path d="M3 4l4 4-4 4M8.5 12h4.5"/>',
  think: '<circle cx="8" cy="8" r="5.5"/><path d="M8 5.5v3l2 1"/>',
  fetch: '<circle cx="8" cy="8" r="5.5"/><path d="M2.5 8h11M8 2.5c2 2 2 9 0 11M8 2.5c-2 2-2 9 0 11"/>',
  other: '<circle cx="8" cy="8" r="2"/><circle cx="8" cy="8" r="5.5"/>',
};
const icon = (kind) => `<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">${ICONS[kind] || ICONS.other}</svg>`;
const CHEVRON = '<svg class="chev" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M4.5 2.5L8 6l-3.5 3.5"/></svg>';

// ---------------------------------------------------------------- messages

// Folds the reader opened stay open across updates; items already seen
// don't animate in again.
const openFolds = new Set();
const seen = new Set();

function renderMessages() {
  $("title").textContent = thread.title;
  document.title = `${thread.title} · Axel`;
  $("live").innerHTML = thread.agents
    .map((a) => `<span class="chip"><span class="dot ${esc(a.state)}"></span><span class="${fg(a)}">${esc(a.name)}</span></span>`)
    .join("");
  const timeline = $("timeline");
  const atBottom = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80;
  timeline.innerHTML = `<div class="column">${timelineHtml()}</div>`;
  for (const it of thread.items) seen.add(it.id);
  if (atBottom) timeline.scrollTop = timeline.scrollHeight;
  $("jump").hidden = atBottom;
}

function timelineHtml() {
  const items = thread.items;
  if (items.length === 0) {
    const name = (thread.known_agents[0] || {}).name || "atlas";
    return `<div class="empty"><div class="title">Nothing here yet</div>Write <b>@${esc(name)}</b> and what you want done, below.</div>`;
  }
  const out = [];
  if (thread.skipped) out.push(`<div class="skipped">${thread.skipped} earlier items not shown</div>`);
  let steps = [];
  const flush = () => {
    if (steps.length) out.push(stepsHtml(steps));
    steps = [];
  };
  for (const it of items) {
    if (it.kind === "tool_call") {
      steps.push(it);
      continue;
    }
    flush();
    out.push(itemHtml(it));
  }
  flush();
  const working = thread.agents.filter((a) => a.state === "working");
  if (working.length) {
    out.push(`<div class="working"><span class="dots"><span></span><span></span><span></span></span>${working.map((a) => `<b class="${fg(a)}">${esc(a.name)}</b>`).join(", ")} ${working.length === 1 ? "is" : "are"} working</div>`);
  }
  return out.join("");
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
  const you = it.kind === "user_message" && !it.agent;
  const cls = [you ? "you" : "agent", it.kind === "error" ? "error" : "", seen.has(it.id) ? "old" : ""].join(" ");
  const name = you ? esc(it.author || "You") : esc(it.agent ? it.agent.name : "Agent");
  const avatar = you ? `<span class="avatar you">${initial(it.author || "Y")}</span>` : `<span class="avatar ${bg(it.agent)}">${initial(it.agent && it.agent.name)}</span>`;
  const to = it.relay_to ? ` <span class="time">to</span> ${esc(it.relay_to)}` : "";
  const tag = it.steering ? `<span class="tag">read at its next step</span>` : it.status === "pending" && you ? `<span class="tag">queued</span>` : "";
  // The avatar sits in a plain block: Ladybird centres a grid or flex row's
  // short items whatever the alignment asks.
  return `<div class="turn ${cls}"><div class="gutter">${avatar}</div><div class="body">
    <div class="who"><span class="name ${you ? "" : fg(it.agent)}">${name}${to}</span><span class="time">${clock(it.at_ms)}</span>${tag}</div>
    <div class="text">${markdown(it.body.text)}</div></div></div>`;
}

function thoughtHtml(it) {
  const id = `t-${it.id}`;
  const first = it.body.text.split("\n")[0].slice(0, 90);
  return `<details class="thought" data-fold="${esc(id)}"${openFolds.has(id) ? " open" : ""}><summary>${CHEVRON}${icon("think")}<span>${esc(first)}${it.body.text.length > first.length ? "…" : ""}</span></summary><div class="said">${markdown(it.body.text)}</div></details>`;
}

function stepsHtml(run) {
  const id = `s-${run[0].id}`;
  const failed = run.filter((t) => t.status === "failed").length;
  const running = run.some((t) => t.status === "running" || t.status === "pending");
  const last = run[run.length - 1].body;
  const label = run.length === 1 ? esc(last.summary || last.title) : `${run.length} steps · ${esc(last.summary || last.title)}`;
  const state = running ? '<span class="spin"></span>' : failed ? `<span class="state failed">${failed} failed</span>` : "";
  return `<details class="steps" data-fold="${esc(id)}"${openFolds.has(id) ? " open" : ""}><summary>${CHEVRON}${icon(last.kind)}<span>${label}</span>${state}</summary>${run.map(stepHtml).join("")}</details>`;
}

function stepHtml(t) {
  const b = t.body;
  const state = t.status === "failed" ? '<span class="state failed">failed</span>' : t.status === "running" || t.status === "pending" ? '<span class="spin"></span>' : '<span class="state ok">✓</span>';
  const output = b.output && b.output.length ? `<pre>${esc(b.output.join("\n"))}</pre>` : "";
  return `<div class="step">${icon(b.kind)}<div class="what">${esc(b.summary || b.title)}${output}</div>${state}</div>`;
}

function askHtml(it) {
  const b = it.body;
  const who = it.agent ? esc(it.agent.name) : "An agent";
  const open = !b.chosen && (it.status === "pending" || it.status === null);
  if (!open) return `<div class="ask done"><div class="q">${esc(b.prompt)}</div>${b.chosen ? `✓ ${esc(b.chosen)}` : "Answered"}</div>`;
  const kind = it.kind === "permission" ? "asks to go ahead" : "asks you";
  const options = b.options
    .map((o) => `<button type="button" data-answer="${esc(o.id)}" data-item="${esc(it.id)}" data-session="${esc(it.session_id)}">${esc(o.label)}</button>`)
    .join("");
  return `<div class="ask"><div class="q"><small>${who} ${kind}</small>${esc(b.prompt)}</div><div class="options">${options}</div></div>`;
}

function changesHtml(b) {
  const files = b.files === 1 ? "1 file" : `${b.files} files`;
  return `<div class="changes">${files} changed <span class="add">+${b.added}</span><span class="del">−${b.removed}</span></div>`;
}

function setupMessages() {
  const timeline = $("timeline");
  timeline.addEventListener("toggle", (e) => {
    const fold = e.target.dataset && e.target.dataset.fold;
    if (fold) e.target.open ? openFolds.add(fold) : openFolds.delete(fold);
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
  });
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
    ? `<span class="avatar ${bg(agent)}">${initial(t.name)}</span>to <b class="${fg(agent)}">${esc(t.name)}</b>${t.live ? "" : " · summons it"}`
    : `<span class="dim">No agent on this thread yet</span>`;
  $("mode").textContent = t && t.mode ? t.mode : "";
  const working = thread.agents.some((a) => a.state === "working");
  $("stop").hidden = !working;
  $("send").textContent = working ? "Queue" : "Send";
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
  setStatus(steer ? "Interrupting…" : "Sending…");
  try {
    const reply = await post("send", { text, steer });
    history.push(text);
    if (history.length > HISTORY_MAX) history.shift();
    historyAt = -1;
    input.value = "";
    setStatus(reply.note || "Sent.");
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
    setStatus("Stopping…");
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
  box.innerHTML = names.map((a, i) => `<div class="${i === 0 ? "on" : ""}" data-name="${esc(a.name)}"><span class="avatar ${bg(a)}">${initial(a.name)}</span>${esc(a.name)}</div>`).join("");
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

function spark(values) {
  const width = 72, height = 18;
  if (!values || values.length < 2 || !values.some((v) => v > 0)) return "";
  const max = Math.max(...values);
  const points = values.map((v, i) => `${((i / (values.length - 1)) * width).toFixed(1)},${(height - 1 - (v / max) * (height - 2)).toFixed(1)}`).join(" ");
  return `<svg class="spark" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"><polyline points="${points}" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" stroke-linecap="round"/></svg>`;
}

function meter(fraction, hot) {
  const pct = Math.max(0, Math.min(100, Math.round(fraction * 100)));
  // A width in a style attribute is refused by the page's policy; a SVG
  // rect's width is an attribute, so the bar is drawn as one.
  return `<svg class="meter${hot ? " hot" : ""}" viewBox="0 0 100 4" preserveAspectRatio="none" height="4"><rect width="100" height="4" rx="2" fill="#2c2c33"/><rect width="${pct}" height="4" rx="2" fill="${hot ? "#e6b450" : "#61afef"}"/></svg>`;
}

function renderActivity() {
  const u = thread.usage;
  const states = new Map(thread.agents.map((a) => [a.name, a.state]));
  const modes = new Map(thread.live.map((s) => [s.agent, s.mode]));
  const byName = new Map(u.agents.map((a) => [a.name, a]));
  const names = [...new Set([...thread.agents.map((a) => a.name), ...u.agents.map((a) => a.name)])];
  const agents = names.map((name) => {
    const spend = byName.get(name) || { total: { tokens: 0, cost_usd: 0 }, spark: [] };
    const known = thread.agents.find((a) => a.name === name) || spend;
    const state = states.get(name) || "idle";
    const ctx = spend.context;
    const context = ctx && ctx.size ? `${meter(ctx.used / ctx.size, ctx.used / ctx.size > 0.8)}<span>${Math.round((ctx.used / ctx.size) * 100)}% context</span>` : "";
    return `<div class="agent"><span class="avatar ${bg(known)}">${initial(name)}</span>
      <div class="name"><span class="${fg(known)}">${esc(name)}</span><span class="state ${esc(state)}">${STATE_LABEL[state] || esc(state)}</span></div>
      <div class="spend">${compact(spend.total.tokens)} · ${money(spend.total.cost_usd)}</div>
      <div class="sub">${modes.get(name) ? `<span>${esc(modes.get(name))}</span>` : ""}${context}</div>
      <span class="${fg(known)}">${spark(spend.spark)}</span></div>`;
  });
  const recent = thread.items.filter((it) => it.kind === "tool_call").slice(-5).reverse()
    .map((it) => `<div><span class="t">${ago(it.at_ms)}</span><span class="s">${esc(it.body.summary || it.body.title)}</span></div>`).join("");
  const limits = u.limits.flatMap((p) => p.windows.filter((w) => w.used_pct !== null).map((w) =>
    `<div class="limit"><span class="what">${esc(p.harness)} ${w.window === "five_hour" ? "5h" : esc(w.window)}</span>${meter(w.used_pct / 100, w.used_pct > 80)}<span>${Math.round(w.used_pct)}%</span></div>`)).join("");
  $("activity-body").innerHTML = `
    <div class="section"><span>Agents</span><span>${compact(thread.tokens)} tokens · ${money(thread.cost_usd)}</span></div>
    ${agents.join("") || `<div class="agent"><span></span><div class="sub">No agent yet</div></div>`}
    ${recent ? `<div class="section">Recent steps</div><div class="recent">${recent}</div>` : ""}
    ${limits ? `<div class="section">Plan</div>${limits}` : ""}`;
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
// Relative times in the activity pane move on even when nothing else does.
if (VIEW === "activity") setInterval(() => thread && renderActivity(), 15000);
