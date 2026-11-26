// Axel's web pane.  Renders the thread the pane's server sends
// (`/api/thread`, the terminal view's own model), refetches it when the
// event stream says it changed, and sends what you type through
// `/api/send`, which routes it as the terminal composer does.
"use strict";

const $ = (id) => document.getElementById(id);
const timeline = $("timeline");
const input = $("input");
let thread = null;
let busy = false;
// The most messages remembered for ↑ in an empty composer.
const HISTORY_MAX = 50;
const history = [];
let historyAt = -1;

// ---------------------------------------------------------------- text

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

// A little Markdown, on escaped text only: code fences, inline code, bold,
// headings, bullets and links.  Nothing the agent writes becomes markup.
function markdown(text) {
  const blocks = escapeHtml(text).split(/```/);
  return blocks
    .map((block, i) => {
      if (i % 2 === 1) {
        const body = block.replace(/^[\w+-]*\n/, "");
        return `<pre><code>${body.replace(/\n$/, "")}</code></pre>`;
      }
      return block
        .split(/\n{2,}/)
        .filter((p) => p.trim() !== "")
        .map(paragraph)
        .join("");
    })
    .join("");
}

function paragraph(p) {
  const lines = p.split("\n");
  if (lines.every((l) => /^\s*[-*] /.test(l))) {
    return `<ul>${lines.map((l) => `<li>${inline(l.replace(/^\s*[-*] /, ""))}</li>`).join("")}</ul>`;
  }
  const heading = /^#{1,6} (.*)$/.exec(p);
  if (heading && lines.length === 1) return `<h3>${inline(heading[1])}</h3>`;
  return `<p>${lines.map(inline).join("<br>")}</p>`;
}

function inline(s) {
  return s
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/\bhttps?:\/\/[^\s<]+/g, (u) => `<a href="${u}" target="_blank" rel="noreferrer">${u}</a>`);
}

// The CSS class for an agent's colour: classes, not inline styles, which
// the page's security policy refuses.
const COLORS = ["purple", "yellow", "red", "green", "blue", "gray", "orange"];
function fg(agent) {
  return agent && COLORS.includes(agent.color) ? `fg-${agent.color}` : "";
}
function bg(agent) {
  return agent && COLORS.includes(agent.color) ? `bg-${agent.color}` : "";
}

function clock(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

// ---------------------------------------------------------------- render

function render() {
  if (!thread) return;
  if (thread.error) {
    $("title").textContent = "Couldn't load this thread";
    setStatus(thread.error, true);
    return;
  }
  document.title = `${thread.title} · Axel`;
  $("title").textContent = thread.title;
  $("agents").innerHTML = thread.agents
    .map((a) => `<span class="chip"><span class="dot ${escapeHtml(a.state)}"></span><span class="${fg(a)}">${escapeHtml(a.name)}</span></span>`)
    .join("");
  const tokens = thread.tokens >= 1000 ? `${(thread.tokens / 1000).toFixed(1)}k` : `${thread.tokens}`;
  $("spend").textContent = thread.tokens ? `${tokens} tokens · $${thread.cost_usd.toFixed(2)}` : "";
  $("stop").hidden = !thread.agents.some((a) => a.state === "working");

  const atBottom = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80;
  timeline.innerHTML = timelineHtml(thread.items);
  if (atBottom) {
    timeline.scrollTop = timeline.scrollHeight;
    $("jump").hidden = true;
  } else {
    $("jump").hidden = false;
  }
}

function timelineHtml(items) {
  if (items.length === 0) {
    const name = (thread.known_agents[0] || {}).name || "atlas";
    return `<div class="empty">Nothing here yet.<br>Type <b>@${escapeHtml(name)}</b> and what you want done.</div>`;
  }
  const out = [];
  if (thread.skipped) out.push(`<div class="skipped">${thread.skipped} earlier items not shown</div>`);
  let tools = [];
  const flush = () => {
    if (tools.length) out.push(toolsHtml(tools));
    tools = [];
  };
  for (const it of items) {
    if (it.kind === "tool_call") {
      tools.push(it);
      continue;
    }
    flush();
    out.push(itemHtml(it));
  }
  flush();
  return out.join("");
}

function itemHtml(it) {
  const b = it.body;
  switch (it.kind) {
    case "user_message":
    case "assistant_message":
    case "thought":
    case "error":
      return b.type === "text" ? messageHtml(it) : "";
    case "permission":
    case "question":
      return b.type === "choice" ? choiceHtml(it) : "";
    case "answer":
      return b.type === "answered" ? `<div class="answered">✓ ${escapeHtml(b.label)}</div>` : "";
    case "turn_end":
      return b.type === "changes" ? changesHtml(b) : "";
    default:
      return "";
  }
}

function messageHtml(it) {
  const you = it.kind === "user_message" && !it.agent;
  const cls = you ? "you" : it.kind === "thought" ? "thought" : it.kind === "error" ? "error" : "agent";
  const name = you ? escapeHtml(it.author || "you") : escapeHtml(it.agent ? it.agent.name : "agent");
  const to = it.relay_to ? ` → ${escapeHtml(it.relay_to)}` : "";
  const tags = [
    it.steering ? `<span class="tag steering">read at its next step</span>` : "",
    it.status === "pending" && !it.steering && you ? `<span class="tag">queued</span>` : "",
    it.kind === "thought" ? `<span class="tag">thinking</span>` : "",
  ].join("");
  return `<article class="msg ${cls}">
    <div class="who"><span class="name ${you ? "" : fg(it.agent)}">${name}${to}</span><span class="time">${clock(it.at_ms)}</span>${tags}</div>
    <div class="bubble">${markdown(it.body.text)}</div>
  </article>`;
}

function toolsHtml(run) {
  const last = run[run.length - 1];
  const failed = run.filter((t) => t.status === "failed").length;
  const running = run.some((t) => t.status === "running" || t.status === "pending");
  const summary = escapeHtml(last.body.summary || last.body.title || "tool call");
  const count = run.length === 1 ? "1 tool call" : `${run.length} tool calls`;
  const extra = [running ? "running…" : "", failed ? `${failed} failed` : ""].filter(Boolean).join(" · ");
  return `<details class="tools"><summary>${count} · ${summary}${extra ? ` · ${extra}` : ""}</summary>${run.map(toolHtml).join("")}</details>`;
}

function toolHtml(t) {
  const b = t.body;
  const state = t.status === "failed" ? "!" : t.status === "running" || t.status === "pending" ? "●" : "✓";
  const output = b.output && b.output.length ? `<pre>${escapeHtml(b.output.join("\n"))}</pre>` : "";
  return `<div class="tool"><div class="line"><span class="state ${t.status || ""}">${state}</span>${escapeHtml(b.summary || b.title)}</div>${output}</div>`;
}

function choiceHtml(it) {
  const b = it.body;
  const open = !b.chosen && (it.status === "pending" || it.status === null);
  if (!open) {
    return `<div class="choice done"><div class="ask">${escapeHtml(b.prompt)}</div>${b.chosen ? `✓ ${escapeHtml(b.chosen)}` : ""}</div>`;
  }
  const options = b.options
    .map((o) => `<button type="button" data-answer="${escapeHtml(o.id)}" data-item="${escapeHtml(it.id)}" data-session="${escapeHtml(it.session_id)}">${escapeHtml(o.label)}</button>`)
    .join("");
  const who = it.agent ? escapeHtml(it.agent.name) : "An agent";
  return `<div class="choice"><div class="ask">${who} asks: ${escapeHtml(b.prompt)}</div><div class="options">${options}</div></div>`;
}

function changesHtml(b) {
  const files = b.files === 1 ? "1 file" : `${b.files} files`;
  return `<div class="changes">${files} changed <span class="add">+${b.added}</span> <span class="del">−${b.removed}</span></div>`;
}

// ---------------------------------------------------------------- talk to the server

async function load() {
  const r = await fetch("/api/thread", { cache: "no-store" });
  if (!r.ok) throw new Error(`the pane answered ${r.status}`);
  thread = await r.json();
  render();
}

async function post(action, body) {
  const r = await fetch(`/api/${action}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const reply = await r.json().catch(() => ({ error: `the pane answered ${r.status}` }));
  if (!r.ok || reply.error) throw new Error(reply.error || `the pane answered ${r.status}`);
  return reply;
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
    grow();
    setStatus(reply.note || "");
  } catch (e) {
    setStatus(e.message, true);
  } finally {
    busy = false;
    $("send").disabled = false;
    input.focus();
  }
}

function listen() {
  const events = new EventSource("/api/events");
  events.onmessage = () => load().catch((e) => setStatus(e.message, true));
  events.onerror = () => setStatus("Reconnecting to the pane…");
  events.onopen = () => setStatus("");
}

// ---------------------------------------------------------------- composer

function grow() {
  input.style.height = "auto";
  input.style.height = `${input.scrollHeight}px`;
}

function mentionQuery() {
  const before = input.value.slice(0, input.selectionStart);
  const m = /(^|\s)@([\w-]*)$/.exec(before);
  return m ? m[2].toLowerCase() : null;
}

function suggest() {
  const box = $("suggest");
  const q = mentionQuery();
  const names = q === null || !thread ? [] : thread.known_agents.filter((a) => a.name.toLowerCase().startsWith(q)).slice(0, 8);
  box.hidden = names.length === 0;
  box.innerHTML = names.map((a, i) => `<div class="${i === 0 ? "on" : ""}" data-name="${escapeHtml(a.name)}"><span class="dot ${bg(a)}"></span>${escapeHtml(a.name)}</div>`).join("");
}

function complete(name) {
  const at = input.selectionStart;
  const before = input.value.slice(0, at).replace(/@[\w-]*$/, `@${name} `);
  input.value = before + input.value.slice(at);
  input.selectionStart = input.selectionEnd = before.length;
  $("suggest").hidden = true;
  input.focus();
}

input.addEventListener("input", () => {
  grow();
  suggest();
});

input.addEventListener("keydown", (e) => {
  const box = $("suggest");
  if (!box.hidden && (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey))) {
    const on = box.querySelector(".on");
    if (on) {
      e.preventDefault();
      complete(on.dataset.name);
      return;
    }
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    send(e.metaKey || e.ctrlKey);
  } else if (e.key === "Escape") {
    if (!box.hidden) box.hidden = true;
    else stop();
  } else if (e.key === "ArrowUp" && input.value === "" && history.length) {
    historyAt = historyAt < 0 ? history.length - 1 : Math.max(0, historyAt - 1);
    input.value = history[historyAt];
    e.preventDefault();
  }
});

$("suggest").addEventListener("mousedown", (e) => {
  const row = e.target.closest("[data-name]");
  if (row) {
    e.preventDefault();
    complete(row.dataset.name);
  }
});

$("form").addEventListener("submit", (e) => {
  e.preventDefault();
  send(false);
});

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
$("stop").addEventListener("click", stop);

timeline.addEventListener("click", async (e) => {
  const button = e.target.closest("[data-answer]");
  if (!button) return;
  button.disabled = true;
  try {
    await post("answer", { session_id: button.dataset.session, item_id: button.dataset.item, option_id: button.dataset.answer });
    setStatus("Answered.");
  } catch (err) {
    button.disabled = false;
    setStatus(err.message, true);
  }
});

timeline.addEventListener("scroll", () => {
  if (timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80) $("jump").hidden = true;
});
$("jump").addEventListener("click", () => {
  timeline.scrollTop = timeline.scrollHeight;
  $("jump").hidden = true;
});

load().catch((e) => setStatus(e.message, true)).finally(listen);
input.focus();
