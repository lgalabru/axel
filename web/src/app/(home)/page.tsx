"use client";

import { useEffect, useRef, useState } from "react";
import {
  ArrowRight,
  BadgeCheck,
  Coins,
  GitBranch,
  KeyRound,
  Laptop,
  MessagesSquare,
  Server,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { DOCS, REPO } from "@/lib/site";

/* ---------------------------------------------------------------- terminal */

type Agent = { name: string; color: string; dot: string };
const ATLAS: Agent = { name: "atlas", color: "text-tangerine", dot: "bg-tangerine" };
const NOVA: Agent = { name: "nova", color: "text-sky", dot: "bg-sky" };
const AGENTS = [ATLAS, NOVA];

const PROMPT = "@atlas add retries to the webhook sender, then ask @nova@mini to review it";

/** Text with `@name` (and `@name@host`) in the agent's colour, the host in violet. */
function Mentions({ text }: { text: string }) {
  const parts = text.split(/(@[a-z]+(?:@[a-z]+)?)/g);
  return (
    <>
      {parts.map((p, i) => {
        const [, name, host] = p.match(/^@([a-z]+)(?:@([a-z]+))?$/) ?? [];
        const agent = AGENTS.find((a) => a.name === name);
        if (!agent) return <span key={i}>{p}</span>;
        return (
          <span key={i} className="font-semibold">
            <span className={agent.color}>@{name}</span>
            {host && <span className="text-violet">@{host}</span>}
          </span>
        );
      })}
    </>
  );
}

type Line =
  | { kind: "head"; agent: Agent; where: string }
  | { kind: "tool"; glyph: string; text: string; note?: string; ok?: boolean }
  | { kind: "say"; text: string }
  | { kind: "event"; text: string }
  | { kind: "done"; text: string };

type State = "idle" | "working" | "done";

/** The scene, one line at a time: [line, ms before it appears, agent states after it]. */
const SCENE: [Line, number, Partial<Record<string, State>>][] = [
  [{ kind: "head", agent: ATLAS, where: "laptop" }, 500, { atlas: "working" }],
  [{ kind: "tool", glyph: "✎", text: "edit   src/webhooks.rs", note: "+42 −6" }, 900, {}],
  [{ kind: "tool", glyph: "$", text: "cargo test", note: "128 passed", ok: true }, 1100, {}],
  [{ kind: "say", text: "Retries with exponential backoff are in. Asking nova to review." }, 900, {}],
  [{ kind: "event", text: "atlas → summoned @nova on mini" }, 800, { atlas: "idle", nova: "working" }],
  [{ kind: "head", agent: NOVA, where: "mini" }, 700, {}],
  [{ kind: "tool", glyph: "◎", text: "read   src/webhooks.rs" }, 900, {}],
  [{ kind: "say", text: "Backoff looks right. One catch: no jitter, so every worker retries at the same instant under load." }, 1300, {}],
  [{ kind: "event", text: "nova → replied to @atlas" }, 800, { nova: "done", atlas: "working" }],
  [{ kind: "head", agent: ATLAS, where: "laptop" }, 600, {}],
  [{ kind: "tool", glyph: "✎", text: "edit   src/webhooks.rs", note: "+9 −2" }, 900, {}],
  [{ kind: "tool", glyph: "$", text: "cargo test", note: "129 passed", ok: true }, 1000, {}],
  [{ kind: "say", text: "Added full jitter, with a test for it. Ready to merge." }, 900, { atlas: "done" }],
  [{ kind: "done", text: "done · 2 agents · laptop + mini · 58k tokens · $0.84" }, 700, {}],
];

const TYPE_MS = 32;

/**
 * Plays the scene once: types the prompt into the bar, presses Enter (the keycap lights),
 * opens the bar into a window and reveals the lines. The finished conversation stays.
 */
function useScene() {
  const [typed, setTyped] = useState(0);
  const [pressed, setPressed] = useState(false);
  const [open, setOpen] = useState(false);
  const [shown, setShown] = useState(0);
  useEffect(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      setTyped(PROMPT.length);
      setOpen(true);
      setShown(SCENE.length);
      return;
    }
    let timer: ReturnType<typeof setTimeout>;
    let cancelled = false;
    const wait = (ms: number) => new Promise<void>((r) => (timer = setTimeout(r, ms)));
    (async () => {
      await wait(1100);
      for (let i = 1; i <= PROMPT.length && !cancelled; i++) {
        setTyped(i);
        await wait(TYPE_MS + (PROMPT[i - 1] === " " ? 30 : 0));
      }
      await wait(450);
      setPressed(true); // the Enter keycap goes down
      await wait(260);
      setOpen(true); // and the bar opens into a window
      await wait(700);
      for (let i = 0; i < SCENE.length && !cancelled; i++) {
        await wait(SCENE[i][1]);
        setShown(i + 1);
      }
      // Plays once; the finished conversation stays.
    })();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);
  return { typed: PROMPT.slice(0, typed), pressed, open, shown };
}

function stateAt(shown: number): Record<string, State> {
  const st: Record<string, State> = { atlas: "idle", nova: "idle" };
  for (let i = 0; i < shown; i++) Object.assign(st, SCENE[i][2]);
  return st;
}

function StateChip({ agent, state, where }: { agent: Agent; state: State; where: string }) {
  const glyph = state === "working" ? "◐" : state === "done" ? "✓" : "○";
  return (
    <span className="flex items-center gap-1.5">
      <span className={`${agent.color} ${state === "working" ? "animate-pulse" : ""}`}>{glyph}</span>
      <span className={agent.color}>{agent.name}</span>
      <span className="text-white/30">{where}</span>
    </span>
  );
}

function SceneLine({ line }: { line: Line }) {
  switch (line.kind) {
    case "head":
      return (
        <div className="mt-3 flex items-center gap-2 first:mt-0">
          <span className={`h-2 w-2 rounded-full ${line.agent.dot}`} />
          <span className={`font-semibold ${line.agent.color}`}>{line.agent.name}</span>
          <span className="text-white/30">· {line.where}</span>
        </div>
      );
    case "tool":
      return (
        <div className="flex gap-3 pl-4 text-white/55">
          <span className="w-3 text-white/35">{line.glyph}</span>
          <span className="min-w-0 truncate">{line.text}</span>
          {line.note && <span className={`ml-auto shrink-0 ${line.ok ? "text-emerald-400" : "text-white/40"}`}>{line.ok ? `✓ ${line.note}` : line.note}</span>}
        </div>
      );
    case "say":
      return <div className="pl-4 text-white/90">{line.text}</div>;
    case "event":
      return (
        <div className="mt-2 pl-4 text-violet">
          <Mentions text={line.text} />
        </div>
      );
    case "done":
      return (
        <div className="mt-3 border-t border-line pt-3 text-emerald-400">
          ✓ done<span className="text-white/45">{line.text.replace(/^done/, "")}</span>
        </div>
      );
  }
}

/**
 * The hero: a big command bar you watch being typed into. Enter presses its keycap and
 * the bar opens into a terminal window where two agents on two machines get to work.
 */
function HeroTerminal() {
  const { typed, pressed, open, shown } = useScene();
  const st = stateAt(shown);
  // Once the conversation outgrows the window it slides up, newest line at the bottom.
  const log = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const [offset, setOffset] = useState(0);
  useEffect(() => {
    const box = log.current;
    const inner = body.current;
    if (!box || !inner) return;
    setOffset(Math.max(0, inner.offsetHeight - (box.clientHeight - 32)));
  }, [shown, open]);

  return (
    <div className="relative w-full overflow-hidden rounded-[13px] bg-surface text-left font-mono shadow-2xl shadow-black/40">
      {/* The bar: the prompt being typed, then the window's title bar. */}
      <div className={`relative transition-[height] duration-500 ease-out ${open ? "h-[56px]" : "h-[104px] sm:h-[96px]"}`}>
        <div
          className={`absolute inset-0 flex items-center gap-3 px-5 transition-all duration-500 ease-out ${
            open ? "pointer-events-none -translate-y-3 opacity-0" : "translate-y-0 opacity-100"
          }`}
        >
          <div className="min-w-0 flex-1 pl-6 -indent-6 text-[15px] leading-7 text-white sm:text-base">
            <span className="inline-block w-6 indent-0 font-bold text-pink">›</span>
            {typed ? <Mentions text={typed} /> : <span className="text-white/35">What are we building?</span>}
            <span className="ml-px inline-block h-[1.15em] w-[2px] translate-y-[3px] bg-pink animate-[blink_1s_step-end_infinite]" />
          </div>
          <kbd
            className={`hidden shrink-0 items-center gap-1.5 rounded-lg border px-3 py-1.5 text-xs transition-all duration-150 sm:flex ${
              pressed
                ? "translate-y-[2px] border-pink bg-pink text-white shadow-[0_0_28px_rgba(247,69,198,0.9)]"
                : "border-line bg-raised text-white/50 shadow-[0_3px_0_0_rgba(255,255,255,0.06)]"
            }`}
          >
            ⏎ Enter
          </kbd>
        </div>
        <div
          className={`absolute inset-0 flex items-center justify-between gap-4 border-b border-line px-5 text-[13px] text-white/50 transition-all duration-500 ease-out ${
            open ? "translate-y-0 opacity-100" : "pointer-events-none translate-y-3 opacity-0"
          }`}
        >
          <div className="flex min-w-0 items-center gap-3">
            <span className="flex shrink-0 gap-1.5">
              <span className="h-3 w-3 rounded-full bg-[#ff5f57]" />
              <span className="h-3 w-3 rounded-full bg-[#febc2e]" />
              <span className="h-3 w-3 rounded-full bg-[#28c840]" />
            </span>
            <span className="truncate text-white/70">add retries to the webhook sender</span>
          </div>
          <div className="hidden shrink-0 items-center gap-4 sm:flex">
            <StateChip agent={ATLAS} state={st.atlas} where="laptop" />
            <StateChip agent={NOVA} state={st.nova} where="mini" />
          </div>
        </div>
      </div>

      {/* The window's body unfolds below the bar (0fr → 1fr). */}
      <div className={`grid transition-[grid-template-rows] duration-700 ease-[cubic-bezier(.2,.8,.2,1)] ${open ? "grid-rows-[1fr]" : "grid-rows-[0fr]"}`}>
        <div className="min-h-0 overflow-hidden">
          <div
            ref={log}
            className={`h-[19rem] overflow-hidden px-5 py-4 text-[13px] leading-6 ${offset > 0 ? "[mask-image:linear-gradient(to_bottom,transparent,black_3.5rem)]" : ""}`}
          >
            <div ref={body} className="transition-transform duration-500 ease-out" style={{ transform: `translateY(-${offset}px)` }}>
              <div className="mb-1 text-white">
                <span className="font-semibold text-pink">&gt; </span>
                <Mentions text={PROMPT} />
              </div>
              {SCENE.slice(0, shown).map(([line], i) => (
                <div key={i} className="animate-[fadein_300ms_ease-out]">
                  <SceneLine line={line} />
                </div>
              ))}
            </div>
          </div>
          <div className="flex items-center gap-2 border-t border-line px-5 py-3 text-[13px]">
            <span className="text-pink">›</span>
            <span className="text-white/35">steer, or @mention another agent</span>
            <span className="ml-px inline-block h-[1.1em] w-[2px] bg-pink/60 animate-[blink_1s_step-end_infinite]" />
          </div>
        </div>
      </div>
    </div>
  );
}

/* ---------------------------------------------------------------- sections */

function Section({
  id,
  eyebrow,
  title,
  children,
  aside,
}: {
  id?: string;
  eyebrow: string;
  title: React.ReactNode;
  children: React.ReactNode;
  aside: React.ReactNode;
}) {
  return (
    <section id={id} className="relative mx-auto grid max-w-5xl scroll-mt-16 items-center gap-12 px-6 py-24 md:grid-cols-2">
      <div>
        <p className="text-spectrum mb-3 inline-block text-sm font-semibold uppercase tracking-widest">{eyebrow}</p>
        <h2 className="mb-5 text-4xl font-semibold tracking-tight text-fg">
          {title}
        </h2>
        <div className="space-y-4 text-lg leading-relaxed text-muted">{children}</div>
      </div>
      <div>{aside}</div>
    </section>
  );
}

function Point({ icon: Icon, title, children }: { icon: React.ComponentType<{ className?: string }>; title: string; children: React.ReactNode }) {
  return (
    <div className="flex gap-3">
      <Icon className="mt-1 h-5 w-5 shrink-0 text-pink" />
      <p className="text-base text-muted">
        <span className="font-medium text-fg">{title}</span> {children}
      </p>
    </div>
  );
}

function Card({ children, className = "" }: { children: React.ReactNode; className?: string }) {
  return <div className={`rounded-2xl border border-line bg-surface p-6 font-mono text-[13px] leading-6 text-fg ${className}`}>{children}</div>;
}

function Lifecycle() {
  const steps = [
    { label: "open", note: "being worked on", dot: "bg-violet" },
    { label: "settled", note: "finished, still listed", dot: "bg-emerald-500" },
    { label: "archived", note: "put away", dot: "bg-dim" },
  ];
  return (
    <Card>
      <div className="mb-4 text-muted">thread · Add retries to the webhook sender</div>
      <div className="space-y-1 text-muted">
        <div>
          <span className="text-tangerine">atlas</span> · session 1 · laptop · <span className="text-dim">ended</span>
        </div>
        <div>
          <span className="text-sky">nova</span> · session 2 · laptop · <span className="text-dim">ended</span>
        </div>
        <div>
          <span className="text-tangerine">atlas</span> · session 3 · mini · <span className="text-pink">working</span>
        </div>
      </div>
      <div className="mt-6 flex flex-wrap items-center gap-2">
        {steps.map((s, i) => (
          <div key={s.label} className="flex items-center gap-2">
            {i > 0 && <ArrowRight className="h-4 w-4 text-dim" />}
            <div className="rounded-lg border border-line bg-ink px-3 py-2">
              <div className="flex items-center gap-2 font-semibold text-fg">
                <span className={`h-2 w-2 rounded-full ${s.dot}`} />
                {s.label}
              </div>
              <div className="text-xs text-muted">{s.note}</div>
            </div>
          </div>
        ))}
      </div>
    </Card>
  );
}

function Hosts() {
  return (
    <Card>
      <div className="flex items-stretch justify-between gap-4">
        <div className="flex-1 rounded-lg border border-line bg-ink p-4">
          <Laptop className="mb-2 h-6 w-6 text-fg" />
          <div className="font-semibold text-fg">laptop</div>
          <div className="text-xs text-muted">atlas&apos;s key stays here</div>
        </div>
        <div className="flex flex-col items-center justify-center gap-1 text-xs text-dim">
          <span>thread + code →</span>
          <span>delegation →</span>
          <span>← events, branch</span>
        </div>
        <div className="flex-1 rounded-lg border border-pink/50 bg-ink p-4">
          <Server className="mb-2 h-6 w-6 text-pink" />
          <div className="font-semibold text-fg">mini</div>
          <div className="text-xs text-muted">signs for atlas: one thread, 7 days</div>
        </div>
      </div>
      <div className="mt-5 space-y-1 text-muted">
        <div>
          <span className="text-dim">$</span> axel agent edit atlas --host mini
        </div>
        <div>
          <span className="text-dim">$</span> axel session move 01j9… --to local
        </div>
      </div>
    </Card>
  );
}

function Guardrails() {
  const rows = [
    { mode: "autopilot", what: "never asks; the guard still refuses irreversible commands" },
    { mode: "edits", what: "file edits go through, everything else asks" },
    { mode: "supervised", what: "asks before every action" },
    { mode: "plan", what: "read-only" },
  ];
  return (
    <Card>
      {rows.map((r) => (
        <div key={r.mode} className="grid grid-cols-[7rem_1fr] gap-2 py-1">
          <span className="font-semibold text-fg">{r.mode}</span>
          <span className="text-muted">{r.what}</span>
        </div>
      ))}
      <div className="mt-4 border-t border-line pt-4 text-muted">
        <div>
          atlas · 41k tokens · <span className="text-fg">$0.62</span> today
        </div>
        <div className="text-dim">refused: nova has spent $20.00 today and reached its daily budget</div>
      </div>
    </Card>
  );
}

const HARNESSES = "Claude Code, Codex, OpenCode and any ACP agent";
const REX = "https://www.superlogical.com/rex";

/** Two specialists instead of one agent that knows everything. */
function Specialists() {
  const agents = [
    { name: "atlas", color: "text-tangerine", dot: "bg-tangerine", role: "builds", model: "claude", skills: ["rust", "axum", "sqlx"] },
    { name: "nova", color: "text-sky", dot: "bg-sky", role: "reviews", model: "codex", skills: ["review", "security"] },
  ];
  return (
    <Card className="space-y-4">
      {agents.map((a) => (
        <div key={a.name} className="rounded-lg border border-line bg-ink p-3">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2 font-semibold text-fg">
              <span className={`h-2.5 w-2.5 rounded-full ${a.dot}`} /> {a.name}
              <BadgeCheck className="h-4 w-4 text-violet" />
              <span className="font-normal text-dim">{a.role}</span>
            </div>
            <span className="text-dim">{a.model}</span>
          </div>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {a.skills.map((sk) => (
              <span key={sk} className="rounded-md border border-line px-2 py-0.5 text-xs text-muted">
                {sk}
              </span>
            ))}
          </div>
        </div>
      ))}
      <div className="rounded-lg border border-line bg-ink p-3">
        <div className="text-xs text-dim">Message from atlas, another Axel agent on this thread:</div>
        <div className="mt-1">
          <span className="font-semibold text-sky">@nova</span> the retry backoff is in src/webhooks.rs, tests pass. Review it and reply to atlas.
        </div>
      </div>
    </Card>
  );
}

/** Why Rex is the foundation. */
function RexFoundation() {
  return (
    <section id="rex" className="relative mx-auto max-w-5xl scroll-mt-16 px-6 py-24">
      <p className="text-spectrum mb-3 inline-block text-sm font-semibold uppercase tracking-widest">The foundation</p>
      <h2 className="mb-5 max-w-3xl text-4xl font-semibold tracking-tight text-fg">Rex fixed the terminal. So orchestration can live there again.</h2>
      <div className="space-y-4 text-lg leading-relaxed text-muted">
        <div className="flex items-center gap-10">
          <p className="max-w-3xl">
            The agent apps of the past year found the right ideas, then rebuilt the terminal badly to deliver them: their own windows, their own status lights, their own way to
            reach a remote machine. They had to. The terminal couldn&apos;t keep work alive, couldn&apos;t tell you a program was waiting on you, didn&apos;t know your other
            machines, and couldn&apos;t even scroll: a long agent conversation in a full-screen TUI, or inside tmux, is junk the moment you scroll up.
          </p>
          <a href={REX} className="hidden shrink-0 transition hover:scale-105 md:block" aria-label="Rex, by Superlogical">
            <img src="/rex.png" alt="Rex" width={160} height={160} className="h-40 w-40 drop-shadow-[0_0_40px_rgba(247,69,198,0.35)]" />
          </a>
        </div>
        <p className="max-w-3xl">
          <a href={REX} className="text-fg underline decoration-pink/60 underline-offset-4 hover:decoration-pink">
            Rex
          </a>
          , by Superlogical, starts from scratch on what a terminal session is: a durable session around the work itself, driven by software while you can always see and take
          control of it. Sessions survive disconnects and devices. Programs report when they&apos;re working, done or blocked on you, and Rex shows it on the tab. Servers join
          your tailnet on their own. Scrollback is the terminal&apos;s, not a TUI&apos;s redraw. And every bit of it is an API a program can drive.
        </p>
        <p className="max-w-3xl">
          Axel is just a program in a Rex pane, built on exactly that. A thread streams into its pane as plain terminal output, so you scroll it like anything else.
        </p>
      </div>
    </section>
  );
}

/* ---------------------------------------------------------------- page */

export default function Home() {
  return (
    <div className="relative">
      <section className="relative z-10 flex min-h-screen flex-col items-center px-6 pb-16 pt-20 text-center">
        {/* Axel the axolotl. */}
        <img src="/mascot.webp" alt="Axel, a smiling axolotl in neon colours" width={984} height={827} className="mb-3 w-[min(11rem,45vw)] drop-shadow-[0_0_50px_rgba(247,69,198,0.45)]" />
        <h1 className="mt-2 max-w-4xl text-5xl font-semibold leading-[1.05] tracking-tight text-fg sm:text-7xl">
          Agent orchestration, <span className="text-spectrum">designed for Rex.</span>
        </h1>
        <p className="mt-6 max-w-2xl text-lg text-muted">
          A year of agent apps taught us what works: threads, and named agents. Axel brings them back to the terminal, on Rex, where your work already lives, on this machine
          or any other on your tailnet.
        </p>
        <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
          <a
            href={`${DOCS}/quick-start`}
            className="group inline-flex items-center gap-2 bg-spectrum rounded-full px-7 py-3 font-semibold text-white shadow-[0_0_40px_rgba(247,69,198,0.45)] transition hover:shadow-[0_0_60px_rgba(247,69,198,0.7)] hover:brightness-110"
          >
            Get started
            <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
          </a>
          <a href={REPO} className="inline-flex items-center gap-2 rounded-full border border-line px-7 py-3 font-semibold text-white/80 transition hover:bg-ink/5 hover:text-white">
            View on GitHub
          </a>
        </div>
        <div className="relative mt-10 flex w-full justify-center">
          {/* The axolotl's spectrum behind the panes, blurred into a glow. */}
          <div aria-hidden className="bg-spectrum absolute -inset-x-10 -inset-y-12 mx-auto max-w-4xl rounded-[3rem] opacity-40 blur-3xl" />
          {/* A 1px spectrum outline: the panes glow like the mascot. */}
          <div className="bg-spectrum relative w-full max-w-3xl rounded-xl p-px shadow-[0_0_60px_rgba(166,77,255,0.35)]">
            <HeroTerminal />
          </div>
        </div>
        <div className="mt-8 flex flex-wrap justify-center gap-x-8 gap-y-2 text-sm text-white/45">
          <span>
            <span className="text-fg/80">Threads</span> from the Codex app and T3 Code
          </span>
          <span>
            <span className="text-fg/80">Named agents</span> from Buzz and skills.sh
          </span>
          <span>
            <span className="text-fg/80">The terminal</span> from{" "}
            <a href={REX} className="underline decoration-white/20 underline-offset-4 hover:text-white/70">
              Rex
            </a>
          </span>
        </div>
      </section>

      <div className="relative z-10 bg-ink">
        <div className="pointer-events-none absolute inset-0 mx-auto max-w-5xl">
          <div className="absolute bottom-0 left-0 top-0 w-px border-l border-dashed border-line" />
          <div className="absolute bottom-0 right-0 top-0 w-px border-r border-dashed border-line" />
        </div>

        <RexFoundation />

        <Section id="threads" eyebrow="Threads" title="The unit of work is the thread, not the tab" aside={<Lifecycle />}>
          <p>
            The Codex app and T3 Code got this right. A thread is what you asked for and everything done about it: the conversation, every tool call, the diff, the agents that
            joined. A chat tab dies with its window; a thread doesn&apos;t.
          </p>
          <p>
            That changes how you work. You run five things at once without losing any of them, come back tomorrow and resume, hand a thread to another agent, and settle it when
            it&apos;s done.
          </p>
          <div className="space-y-3 pt-2">
            <Point icon={GitBranch} title="One worktree per thread, if you want it.">
              Parallel agents on one repository don&apos;t step on each other. Or work in place, across several projects, or none.
            </Point>
            <Point icon={Terminal} title="Signed, so it travels.">
              In Axel a thread is a log of signed events. Copies on two machines merge, which is what lets a thread run on the mini while your laptop sleeps.
            </Point>
          </div>
        </Section>

        <Section id="agents" eyebrow="Named agents" title="Specialists, not one agent that knows everything" aside={<Specialists />}>
          <p>
            Buzz gave agents names and keys; skills.sh made skills a one-line install. Put together, you stop prompting a generic assistant and start working with a team:
            atlas builds, nova reviews.
          </p>
          <div className="space-y-3 pt-2">
            <Point icon={MessagesSquare} title="A second pair of eyes.">
              The best review comes from someone who didn&apos;t write the code. atlas asks nova, a different agent with a different model and a review skill, and gets the
              answer as its next prompt. You see every message.
            </Point>
            <Point icon={Coins} title="No skill bloat.">
              Install sixty skills into one agent and every session pays for their descriptions, and picks the wrong one. Each named agent carries only its own.
            </Point>
            <Point icon={KeyRound} title="Its own key, permissions and budget.">
              Everything atlas does is signed by atlas. It reaches only the agents you allowed, can&apos;t loop, and stops at its daily budget.
            </Point>
          </div>
        </Section>

        <Section id="hosts" eyebrow="Remote hosts" title="Ask, then close the laptop" aside={<Hosts />}>
          <p>
            Your Mac mini at home is on your tailnet, and Rex is on it. <code className="rounded bg-raised px-1.5 py-0.5 text-base">@atlas@mini</code> runs atlas there, in the
            mini&apos;s own Rex session, or make the mini its default.
          </p>
          <p>
            The thread streams to your laptop as if it ran locally. Close the lid on the subway; its branch and uncommitted work come home when you reconnect. A running session
            can even move between machines and keep its conversation.
          </p>
          <div className="space-y-3 pt-2">
            <Point icon={ShieldCheck} title="Keys never leave home.">
              The host gets a short-lived grant to sign for atlas, on one thread only.
            </Point>
          </div>
        </Section>

        <Section id="autopilot" eyebrow="Autopilot" title="No babysitting" aside={<Guardrails />}>
          <p>
            Each agent has a mode for routine permissions, a guard for the things that are never routine, and a daily budget. When an agent does need you, Rex shows it on the
            tab, and the question waits in the thread and in your inbox.
          </p>
          <div className="space-y-3 pt-2">
            <Point icon={Coins} title="Every token counted.">
              Input, output and cache tokens, and the estimated cost, per thread and per agent.
            </Point>
            <Point icon={Terminal} title="No screen scraping.">
              Status comes from hooks, the Agent Client Protocol or <code className="rounded bg-raised px-1 py-0.5 text-sm">axel status</code>. It runs {HARNESSES}.
            </Point>
          </div>
        </Section>

        <section className="relative mx-auto max-w-5xl border-t border-line px-6 py-24 text-center">
          <h2 className="text-4xl font-semibold tracking-tight text-fg">
            Why not today?
          </h2>
          <p className="mx-auto mt-4 max-w-xl text-lg text-muted">Two commands. The first time, Axel checks your machine and helps you create your first agent.</p>
          <div className="mx-auto mt-8 max-w-xl rounded-2xl bg-surface p-6 text-left font-mono text-sm leading-7 text-white/80">
            <div>
              <span className="text-white/35">$</span> curl -fsSL https://axel.md/install | sh
            </div>
            <div>
              <span className="text-white/35">$</span> axel
            </div>
          </div>
          <a href={`${DOCS}/installation`} className="mt-8 inline-flex items-center gap-2 font-semibold text-pink hover:text-sun">
            Read the installation guide <ArrowRight className="h-4 w-4" />
          </a>
        </section>
      </div>
    </div>
  );
}
