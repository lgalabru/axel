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
  let column = timeline.firstElementChild;
  if (!column) {
    column = document.createElement("div");
    column.className = "column";
    timeline.appendChild(column);
  }
  patch(column, timelineBlocks());
  followLatest();
}

// Whether the reader is at the latest message, as of their last scroll.
// Read from scrolling, not worked out at each render: the page's font
// arrives after the first one and lengthens every line, and a pane opened
// on a long thread must still end up at its latest message.
let atLatest = true;
const LATEST_SLACK_PX = 80;

function followLatest() {
  const timeline = $("timeline");
  if (atLatest) timeline.scrollTop = timeline.scrollHeight;
  $("jump").hidden = atLatest;
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
    atLatest = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < LATEST_SLACK_PX;
    $("jump").hidden = atLatest;
  }, { passive: true });
  $("jump").addEventListener("click", () => {
    atLatest = true;
    followLatest();
  });
  // A late font, or a pane resized while docking, moves the latest message.
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(followLatest);
  window.addEventListener("resize", followLatest);
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

// Charts in braille geometry, as the terminal view draws them, but with what
// a page has and a terminal doesn't: as many dots as the pane fits, a top
// dot lit in part for the exact value, and a gradient.  A braille cell is
// two dots across and four down; one cell is 0.6em by a line.
const DOT_X_EM = 0.3;
const DOT_Y_EM = 1.55 / 4;
const DOT_R = 0.32;
// The most dot columns and rows a chart draws: a very wide pane, bounded.
const DOTS_ACROSS_MAX = 400;
const DOTS_DOWN_MAX = 24;
let gradients = 0;

// `values` (oldest first) resampled at `n` points, linearly.
function resample(values, n) {
  if (!values.length) return new Array(n).fill(0);
  if (values.length === 1) return new Array(n).fill(values[0]);
  const out = [];
  for (let i = 0; i < n; i++) {
    const at = (i / Math.max(1, n - 1)) * (values.length - 1);
    const lo = Math.floor(at), hi = Math.min(values.length - 1, lo + 1);
    out.push(values[lo] + (values[hi] - values[lo]) * (at - lo));
  }
  return out;
}

// The dot grid that fits `el`, in dots and in pixels between dots.
function grid(el) {
  const em = parseFloat(getComputedStyle(el).fontSize) || 13;
  const pitchX = DOT_X_EM * em, pitchY = DOT_Y_EM * em;
  const across = Math.min(DOTS_ACROSS_MAX, Math.max(2, Math.floor(el.clientWidth / pitchX)));
  const down = Math.min(DOTS_DOWN_MAX, Math.max(2, Math.floor(el.clientHeight / pitchY)));
  return { across, down, pitchX, pitchY, r: DOT_R * em * DOT_X_EM * 1.6 };
}

// A chart of `values` filling `el`: each column lit from the bottom to its
// value, the top dot in part; quiet columns keep a faint baseline (⣀).
function brailleChart(el, values) {
  const g = grid(el);
  const series = resample(values || [], g.across);
  const max = Math.max(...series, 0);
  const id = `g${gradients++}`;
  const width = g.across * g.pitchX, height = g.down * g.pitchY;
  let dots = "";
  series.forEach((v, c) => {
    const x = (c + 0.5) * g.pitchX;
    const level = max > 0 ? (v / max) * g.down : 0;
    if (level < 0.05) {
      dots += `<circle class="off" cx="${x.toFixed(1)}" cy="${(height - g.pitchY / 2).toFixed(1)}" r="${g.r.toFixed(2)}"/>`;
      return;
    }
    for (let r = 0; r < Math.ceil(level); r++) {
      const part = Math.min(1, level - r);
      const opacity = part < 1 ? ` fill-opacity="${Math.max(0.2, part).toFixed(2)}"` : "";
      dots += `<circle cx="${x.toFixed(1)}" cy="${(height - (r + 0.5) * g.pitchY).toFixed(1)}" r="${g.r.toFixed(2)}" fill="url(#${id})"${opacity}/>`;
    }
  });
  el.innerHTML = `<svg class="braille" viewBox="0 0 ${width.toFixed(1)} ${height.toFixed(1)}" preserveAspectRatio="xMinYMax meet">
    <defs><linearGradient id="${id}" gradientUnits="userSpaceOnUse" x1="0" y1="${height.toFixed(1)}" x2="0" y2="0"><stop offset="0" class="lo"/><stop offset="1" class="hi"/></linearGradient></defs>${dots}</svg>`;
}

// A gauge two dots high filling `el`, lit to `fraction`, its colour running
// green to yellow to red along it so a full one reads as full at a glance.
function brailleGauge(el, fraction) {
  const g = grid(el);
  const id = `g${gradients++}`;
  const width = g.across * g.pitchX, height = 2 * g.pitchY;
  const lit = Math.max(0, Math.min(1, fraction)) * g.across;
  let dots = "";
  for (let c = 0; c < g.across; c++) {
    const on = c < lit;
    const part = Math.min(1, lit - c);
    for (let r = 0; r < 2; r++) {
      const fill = on ? ` fill="url(#${id})"${part < 1 ? ` fill-opacity="${Math.max(0.25, part).toFixed(2)}"` : ""}` : ' class="off"';
      dots += `<circle cx="${((c + 0.5) * g.pitchX).toFixed(1)}" cy="${((r + 0.5) * g.pitchY).toFixed(1)}" r="${g.r.toFixed(2)}"${fill}/>`;
    }
  }
  el.innerHTML = `<svg class="braille" viewBox="0 0 ${width.toFixed(1)} ${height.toFixed(1)}" preserveAspectRatio="xMinYMid meet">
    <defs><linearGradient id="${id}" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="${width.toFixed(1)}" y2="0"><stop offset="0" class="ok"/><stop offset=".75" class="warn"/><stop offset="1" class="bad"/></linearGradient></defs>${dots}</svg>`;
}

const pct = (fraction) => `${Math.round(fraction * 100)}%`;
const heat = (fraction) => (fraction > 0.95 ? "full" : fraction > 0.8 ? "hot" : "");
const STATE_MARK = { working: "⠿ working", blocked: "! needs you", error: "✗ error" };

function renderActivity() {
  const u = thread.usage;
  const states = new Map(thread.agents.map((a) => [a.name, a.state]));
  const byName = new Map(u.agents.map((a) => [a.name, a]));
  const names = [...new Set([...thread.agents.map((a) => a.name), ...u.agents.map((a) => a.name)])];
  const charts = [];
  const rows = names.map((name) => {
    const spend = byName.get(name) || { total: { tokens: 0, cost_usd: 0 }, last_hour: { tokens: 0 }, spark: [] };
    const known = thread.agents.find((a) => a.name === name) || spend;
    const state = states.get(name) || "idle";
    const ctx = spend.context && spend.context.size ? spend.context.used / spend.context.size : null;
    charts.push({ values: spend.spark, ctx });
    const hourly = spend.last_hour && spend.last_hour.tokens;
    const hour = `<span class="${hourly ? "axis" : "quiet"}">${hourly ? `${compact(hourly)}/h` : "quiet"}</span>`;
    const context = ctx === null ? "" : `<span class="ctx"><span class="gauge"></span><span class="${heat(ctx)}">${pct(ctx)}</span></span>`;
    return `<div class="agent ${cc(known)}"><div class="chart"></div>
      <div class="over top"><span class="name">${esc(name)}</span><span class="state ${esc(state)}">${STATE_MARK[state] || ""}</span>${context}<span class="grow"></span>${hour}<span class="tokens">${compact(spend.total.tokens)}</span><span class="cost">${money(spend.total.cost_usd)}</span></div></div>`;
  });
  const limits = u.limits.flatMap((p) => p.windows.filter((w) => w.used_pct !== null).map((w) => {
    const label = { five_hour: "5h", week: "wk", month: "mo" }[w.window] || esc(w.window);
    return { label, fraction: w.used_pct / 100 };
  }));
  $("activity-body").innerHTML = `${rows.join("") || '<div class="quiet">no agent yet</div>'}
    ${limits.length ? `<div class="plan">${limits.map((l) => `<div class="bar-row"><span class="what">${l.label}</span><span class="gauge"></span><span class="pct ${heat(l.fraction)}">${pct(l.fraction)}</span></div>`).join("")}</div>` : ""}`;
  // The charts are drawn once their boxes have a size: as many dots as fit.
  gradients = 0;
  document.querySelectorAll("#activity .agent").forEach((el, i) => {
    brailleChart(el.querySelector(".chart"), charts[i].values);
    const gauge = el.querySelector(".ctx .gauge");
    if (gauge) brailleGauge(gauge, charts[i].ctx);
  });
  document.querySelectorAll("#activity .plan .gauge").forEach((el, i) => brailleGauge(el, limits[i].fraction));
}

// ---------------------------------------------------------------- controls

// The thread controls (Ctrl+D), as the terminal pane walks them: the menu,
// then an agent or a host to pick, a question to answer, or a title to type.
// What each action does and asks is the server's (`manage.rs`); the page
// only walks the steps and sends the outcome.
let ctl = { step: "menu", cursor: 0 };
let ctlNote = null;

const sessionsLine = (n, one, many) => `${n} ${n === 1 ? one : many}`;

function renderControls() {
  const d = thread;
  const meta = [`<span class="state-${esc(d.state)}">${esc(d.state)}</span>`, esc(d.age), ...d.projects.map(esc)].join(" · ");
  const sessions = d.sessions.map((s) => {
    const ctx = s.context && s.context.size ? s.context.used / s.context.size : null;
    const where = s.host ? ` on ${esc(s.host)}` : "";
    return `<div class="session ${s.live ? "" : "ended"} ${cc(s)}">
      <span class="badge">${esc(s.agent)}</span>
      <span class="runs">${esc(s.runs)}${where}<span class="st ${esc(s.state)}">${esc(s.state)}</span></span>
      <span class="ctx">${ctx === null ? "" : `<span class="gauge"></span><span class="ctxp ${heat(ctx)}">${pct(ctx)}</span>`}</span>
      <span class="nums">${sessionsLine(s.turns, "turn", "turns")} · ${s.tool_calls} calls · ${esc(s.duration)} · <b>${compact(s.tokens)}</b> ${money(s.cost_usd)}</span></div>`;
  }).join("");
  const totals = `<span><b>${compact(d.tokens)}</b> tokens</span><span>${money(d.cost_usd)}</span><span>${sessionsLine(d.turns, "turn", "turns")}</span><span>${d.tool_calls} calls</span><span>${sessionsLine(d.files_changed, "file", "files")}</span>`;
  $("controls-body").innerHTML = `
    <div class="ctl-title"><h2>${esc(d.title)}</h2><span class="meta">${meta}</span></div>
    ${sessions ? `<div class="section-label">sessions</div>${sessions}` : '<div class="dim">no sessions yet</div>'}
    <div class="hour c-none"><div class="chart"></div><div class="over">${totals}<span class="grow"></span><span class="dim">last hour</span></div></div>
    ${ctlStepHtml()}`;
  gradients = 0;
  brailleChart(document.querySelector("#controls .hour .chart"), d.spark);
  document.querySelectorAll("#controls .session").forEach((el, i) => {
    const s = d.sessions[i];
    const gauge = el.querySelector(".gauge");
    if (gauge) brailleGauge(gauge, s.context.used / s.context.size);
  });
  if (ctl.step === "rename") {
    const input = document.querySelector("#controls .rename input");
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
  }
  renderFoot();
}

// The rows of the step the pane is on: `[key, label, about, danger]`.
function stepRows() {
  const d = thread;
  switch (ctl.step) {
    case "menu": return d.actions.map((a) => [a.key, a.label, a.about, a.danger]);
    case "pick_agent": return d.agents.map((a, i) => [String(i + 1), a, "", false]);
    case "pick_host": return d.hosts.map((h, i) => [String(i + 1), h, "", false]);
    case "agent": return (d.agent_actions[ctl.agent] || []).map((o) => [o.key, o.label, "", o.outcome.op === "remove_agent"]);
    default: return [];
  }
}

function ctlStepHtml() {
  if (ctl.step === "rename") return `<div class="step-title">new title</div><div class="rename"><span class="prompt">&gt;</span><input type="text" value="${esc(ctl.title)}" spellcheck="false"></div>`;
  if (ctl.step === "confirm") {
    return `<div class="step-title">${esc(ctl.question)}</div><div class="confirm-buttons"><button type="button" class="${ctl.danger ? "danger" : "primary"}" data-yes>yes</button><button type="button" class="ghost" data-no>no</button></div>`;
  }
  const title = { pick_agent: "which agent?", pick_host: `run ${esc(ctl.agent || "")} where?`, agent: esc(ctl.agent || "") }[ctl.step];
  const rows = stepRows().map(([key, label, about, danger], i) =>
    `<div class="row ${i === ctl.cursor ? "on" : ""} ${danger ? "danger" : ""}" data-row="${i}"><span class="k">${esc(key)}</span><span class="label">${esc(label)}</span><span class="about">${esc(about)}</span></div>`).join("");
  return `${title ? `<div class="step-title">${title}</div>` : ""}<div class="menu">${rows}</div>`;
}

function renderFoot() {
  const foot = $("controls-foot");
  foot.className = "controls-foot";
  if (ctl.step === "confirm") {
    foot.classList.add("ask");
    if (ctl.danger) foot.classList.add("danger");
    foot.innerHTML = "<kbd>y</kbd> yes · <kbd>n</kbd> no";
  } else if (ctl.step === "rename") {
    foot.innerHTML = "<kbd>⏎</kbd> rename · <kbd>esc</kbd> back";
  } else if (ctlNote) {
    foot.classList.add("note");
    foot.textContent = ctlNote;
  } else {
    foot.innerHTML = ctl.step === "menu" ? "<kbd>↑↓</kbd> choose · <kbd>⏎</kbd> go · <kbd>esc</kbd> close" : "<kbd>↑↓</kbd> choose · <kbd>⏎</kbd> pick · <kbd>esc</kbd> back";
  }
}

function goTo(step, extra) {
  ctl = { step, cursor: 0, ...extra };
  renderControls();
}

function startAction(action) {
  const d = thread;
  if (["reset", "move", "agents"].includes(action)) {
    if (d.agents.length > 1) return goTo("pick_agent", { action });
    if (d.agents.length === 1) return afterAgent(action, d.agents[0]);
    return;
  }
  if (action === "rename") return goTo("rename", { title: d.title });
  ask({ op: action });
}

function afterAgent(action, agent) {
  const d = thread;
  if (action === "agents") return goTo("agent", { agent });
  if (action === "move" && d.hosts.length > 1) return goTo("pick_host", { agent });
  if (action === "move") return ask({ op: "move", agent, host: d.hosts[0] });
  ask({ op: "reset", agent });
}

async function control(outcome, confirmed) {
  try {
    const reply = await post("control", { outcome, confirmed });
    if (reply.confirm) return goTo("confirm", { outcome, question: reply.confirm, danger: reply.danger });
    if (reply.note) ctlNote = reply.note;
    if (!reply.close) goTo("menu");
  } catch (e) {
    ctlNote = e.message;
    goTo("menu");
  }
}
const ask = (outcome) => control(outcome, false);

function pickRow(i) {
  const rows = stepRows();
  if (i < 0 || i >= rows.length) return;
  const d = thread;
  if (ctl.step === "menu") return startAction(d.actions[i].action);
  if (ctl.step === "pick_agent") return afterAgent(ctl.action, d.agents[i]);
  if (ctl.step === "pick_host") return ask({ op: "move", agent: ctl.agent, host: d.hosts[i] });
  if (ctl.step === "agent") return ask(d.agent_actions[ctl.agent][i].outcome);
}

function setupControls() {
  document.addEventListener("keydown", (e) => {
    if (!thread) return;
    if (ctl.step === "rename") {
      if (e.key === "Escape") goTo("menu");
      else if (e.key === "Enter") {
        e.preventDefault();
        const title = document.querySelector("#controls .rename input").value.trim();
        if (title && title !== thread.title) control({ op: "rename", title }, true);
        else goTo("menu");
      }
      return;
    }
    if (ctl.step === "confirm") {
      if (e.key === "y" || e.key === "Enter") control(ctl.outcome, true);
      else if (e.key === "n" || e.key === "Escape") goTo("menu");
      return;
    }
    const rows = stepRows();
    if (e.key === "Escape" || (ctl.step === "menu" && e.key === "q")) {
      e.preventDefault();
      if (ctl.step === "menu") control({ op: "close" }, true);
      else goTo("menu");
    } else if (e.key === "ArrowDown" || e.key === "j") {
      ctl.cursor = Math.min(rows.length - 1, ctl.cursor + 1);
      renderControls();
    } else if (e.key === "ArrowUp" || e.key === "k") {
      ctl.cursor = Math.max(0, ctl.cursor - 1);
      renderControls();
    } else if (e.key === "Enter") {
      pickRow(ctl.cursor);
    } else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) {
      const at = rows.findIndex(([key]) => key === e.key.toLowerCase());
      if (at >= 0) pickRow(at);
    }
  });
  $("controls-body").addEventListener("click", (e) => {
    const row = e.target.closest("[data-row]");
    if (row) return pickRow(Number(row.dataset.row));
    if (e.target.closest("[data-yes]")) return control(ctl.outcome, true);
    if (e.target.closest("[data-no]")) return goTo("menu");
  });
}

// Ctrl+D opens the thread controls, from the composer or the conversation,
// as it does in the terminal view.
function opensControls() {
  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey && (e.key === "d" || e.key === "D")) {
      e.preventDefault();
      post("open_controls", {}).catch(report);
    }
  });
}

// ---------------------------------------------------------------- scrolling

// Ladybird eases every wheel step over some 200 ms, a dozen full-pane frames
// each; a trackpad sends a stream of steps, so the page lurched.  The page
// takes the wheel itself: one line per step, applied at once, and at most
// one frame for all the steps that came in since the last.
const WHEEL_STEP_PX = 40;
function scrollsItself(el) {
  let owed = 0, queued = false;
  el.addEventListener("wheel", (e) => {
    if (e.ctrlKey) return;
    // An open fold's output scrolls on its own, as long as it can.
    const inner = e.target.closest && e.target.closest(".step pre");
    if (inner && inner !== el && inner.scrollHeight > inner.clientHeight) return;
    e.preventDefault();
    const line = parseFloat(getComputedStyle(el).fontSize) * 1.55 || 20;
    const steps = e.deltaMode === 1 ? e.deltaY : e.deltaY / WHEEL_STEP_PX;
    owed += steps * line;
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      el.scrollTop += owed;
      owed = 0;
    });
  }, { passive: false });
}

// ---------------------------------------------------------------- talk to the server

async function post(action, body) {
  const r = await fetch(`/api/${action}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const reply = await r.json().catch(() => ({ error: `the pane answered ${r.status}` }));
  if (!r.ok || reply.error) throw new Error(reply.error || `the pane answered ${r.status}`);
  return reply;
}

const RENDER = { messages: renderMessages, composer: renderComposer, activity: renderActivity, controls: renderControls };

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
if (VIEW === "messages") scrollsItself($("timeline"));
if (VIEW === "activity") scrollsItself($("activity"));
// The charts fit their boxes: drawn again when the pane changes size.
if (VIEW === "activity") window.addEventListener("resize", () => thread && renderActivity());
if (VIEW === "composer") setupComposer();
if (VIEW === "controls") setupControls();
if (VIEW === "controls") scrollsItself($("controls-body"));
if (VIEW === "composer" || VIEW === "messages") opensControls();
load().catch(report).finally(() => {
  const events = new EventSource("/api/events");
  events.onmessage = () => load().catch(report);
});
