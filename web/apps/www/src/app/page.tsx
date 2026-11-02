"use client";

import { useEffect, useState } from "react";
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

const PROMPT = "add retries to the webhook sender @atlas";

/* ---------------------------------------------------------------- terminal */

type Agent = { name: string; color: string };
const ATLAS: Agent = { name: "atlas", color: "text-tangerine" };
const NOVA: Agent = { name: "nova", color: "text-sky" };
const VEGA: Agent = { name: "vega", color: "text-sun" };

/** The prompt text, with `@name`s of known agents in their colours. */
function Mentions({ text }: { text: string }) {
  const parts = text.split(/(@[a-z]+)/g);
  return (
    <>
      {parts.map((p, i) => {
        const agent = [ATLAS, NOVA, VEGA].find((a) => p === `@${a.name}`);
        return agent ? (
          <span key={i} className={`${agent.color} font-semibold`}>
            {p}
          </span>
        ) : (
          <span key={i}>{p}</span>
        );
      })}
    </>
  );
}

function useTyped(text: string, startMs = 600, stepMs = 45) {
  const [n, setN] = useState(0);
  useEffect(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      setN(text.length);
      return;
    }
    let i = 0;
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      i += 1;
      setN(i);
      if (i < text.length) timer = setTimeout(tick, stepMs);
    };
    timer = setTimeout(tick, startMs);
    return () => clearTimeout(timer);
  }, [text, startMs, stepMs]);
  return text.slice(0, n);
}

const THREADS: {
  glyph: string;
  glyphClass: string;
  title: string;
  agents: Agent[];
  project: string;
  tokens: string;
  age: string;
  settled?: boolean;
}[] = [
  { glyph: "●", glyphClass: "text-amber-400", title: "Migrate billing to Stripe", agents: [NOVA, VEGA], project: "billing", tokens: "120k", age: "12m ago" },
  { glyph: "○", glyphClass: "text-white/40", title: "Rate-limit the public API", agents: [ATLAS], project: "api", tokens: "41k", age: "2h ago" },
  { glyph: "◐", glyphClass: "text-rose-400", title: "Flaky auth test on CI", agents: [ATLAS, NOVA], project: "api", tokens: "18k", age: "yesterday" },
  { glyph: "✓", glyphClass: "text-white/30", title: "Dark mode for the dashboard", agents: [VEGA], project: "web", tokens: "64k", age: "3d ago", settled: true },
];

/** The two floating panes `axel` opens in Rex: the prompt, recent threads below. */
function Launcher() {
  const typed = useTyped(PROMPT);
  const done = typed.length === PROMPT.length;
  return (
    <div className="w-full overflow-hidden rounded-[11px] bg-surface text-left font-mono text-[13px] leading-6 shadow-2xl shadow-black/40 backdrop-blur">
      <div className="flex items-center gap-2 border-b border-line px-4 py-2 text-white/50">
        <Terminal className="h-3.5 w-3.5" />
        <span>why not today?</span>
      </div>
      <div className="flex items-center justify-between gap-4 px-5 py-4">
        <div className="min-w-0 truncate text-white">
          <span className="text-pink">› </span>
          {typed ? <Mentions text={typed} /> : <span className="text-white/35">What are we building?</span>}
          <span className="ml-px inline-block h-[1.1em] w-[2px] translate-y-[3px] bg-pink animate-[blink_1s_step-end_infinite]" />
        </div>
        <div className="hidden shrink-0 sm:block">
          <span className="text-tangerine">atlas</span>
          <span className="text-white/40"> · api</span>
        </div>
      </div>
      <div className="border-t border-line px-5 py-3">
        <div className="flex justify-between text-white/50">
          <span>
            <span className="font-semibold text-violet">threads</span> recent
          </span>
          <span>3 open · 1 settled</span>
        </div>
        <div className="mt-1 text-white/30">› type to filter</div>
        <div className="mt-1">
          {THREADS.map((t) => (
            <div key={t.title} className={`flex gap-3 whitespace-nowrap ${t.settled ? "opacity-45" : ""}`}>
              <span className={t.glyphClass}>{t.glyph}</span>
              <span className="w-56 truncate font-semibold text-white/90">{t.title}</span>
              <span className="hidden w-24 truncate md:inline">
                {t.agents.map((a, i) => (
                  <span key={a.name} className={a.color}>
                    {i > 0 ? " " : ""}
                    {a.name}
                  </span>
                ))}
              </span>
              <span className="hidden w-16 truncate text-white/60 md:inline">{t.project}</span>
              <span className="ml-auto w-12 text-right text-white/60">{t.tokens}</span>
              <span className="hidden w-20 text-right text-white/40 sm:inline">{t.age}</span>
            </div>
          ))}
        </div>
      </div>
      <div className={`border-t border-line px-5 py-2 text-white/40 transition-opacity duration-500 ${done ? "opacity-100" : "opacity-0"}`}>
        <span className="text-violet">Enter</span> starts atlas on it, in place. <span className="text-violet">↓</span> picks up a thread instead.
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

/** What Rex gives Axel, one tile each. */
function RexFoundation() {
  const tiles = [
    {
      title: "Sessions that outlive you",
      body: "A Rex session keeps every pane alive across disconnects, reboots of your laptop and switches of device. Agents run in it, not in a window you might close.",
      code: "rex -S mini",
    },
    {
      title: "Status the terminal understands",
      body: "Rex's Program Status Protocol lets a program say it is working, done, or blocked on a permission, a question or a login. Axel reports every agent through it, so Rex's tabs and session picker show who needs you.",
      code: "OSC 7501 ; state=blocked:kind=permission",
    },
    {
      title: "Tailscale, built in",
      body: "A Rex server joins your tailnet as its own device, and only its owner gets in. Your Mac mini at home is one host name away, from anywhere, with nothing exposed to the internet.",
      code: "https://rex-mini.<tailnet>.ts.net",
    },
    {
      title: "A terminal programs can drive",
      body: "Everything Rex does is a server API: floating panes, splits, labels, events. Axel is just a program in a Rex pane using it, and your agents can use it too, through Rex's own skill.",
      code: "session.new_layer · session.new_split",
    },
  ];
  return (
    <section id="rex" className="relative mx-auto max-w-5xl scroll-mt-16 px-6 py-24">
      <p className="text-spectrum mb-3 inline-block text-sm font-semibold uppercase tracking-widest">The foundation</p>
      <h2 className="mb-5 max-w-3xl text-4xl font-semibold tracking-tight text-fg">Rex fixed the terminal. So orchestration can live there again.</h2>
      <div className="max-w-3xl space-y-4 text-lg leading-relaxed text-muted">
        <p>
          The agent apps of the past year found the right ideas, then rebuilt the terminal badly to deliver them: their own windows, their own status lights, their own way to
          reach a remote machine. They had to, because the terminal couldn&apos;t keep work alive, couldn&apos;t tell you a program was waiting on you, and didn&apos;t know
          your other machines.
        </p>
        <p>
          <a href={REX} className="text-fg underline decoration-pink/60 underline-offset-4 hover:decoration-pink">
            Rex
          </a>
          , by Superlogical, starts from scratch on what a terminal session is: a durable session around the work itself, driven by software while you can always see and take
          control of it. Axel builds on exactly that.
        </p>
      </div>
      <div className="mt-12 grid gap-4 md:grid-cols-2">
        {tiles.map((t) => (
          <div key={t.title} className="rounded-2xl border border-line bg-surface p-6">
            <h3 className="text-lg font-semibold text-fg">{t.title}</h3>
            <p className="mt-2 text-base leading-relaxed text-muted">{t.body}</p>
            <div className="mt-4 truncate font-mono text-[13px] text-violet">{t.code}</div>
          </div>
        ))}
      </div>
    </section>
  );
}

/* ---------------------------------------------------------------- page */

export default function Home() {
  return (
    <div className="relative">
      <section className="relative z-10 flex min-h-screen flex-col items-center justify-center px-6 pb-16 pt-24 text-center">
        {/* Axel the axolotl. */}
        <img src="/mascot.webp" alt="Axel, a smiling axolotl in neon colours" width={984} height={827} className="mb-4 w-[min(16rem,60vw)] drop-shadow-[0_0_50px_rgba(247,69,198,0.45)]" />
        <p className="text-spectrum text-2xl font-bold tracking-tight" style={{ fontFamily: "'Space Grotesk', sans-serif" }}>
          axel
        </p>
        <h1 className="mt-4 max-w-4xl text-5xl font-semibold leading-[1.05] tracking-tight text-fg sm:text-7xl">
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
        <div className="relative mt-14 flex w-full justify-center">
          {/* The axolotl's spectrum behind the panes, blurred into a glow. */}
          <div aria-hidden className="bg-spectrum absolute -inset-x-10 -inset-y-12 mx-auto max-w-4xl rounded-[3rem] opacity-40 blur-3xl" />
          {/* A 1px spectrum outline: the panes glow like the mascot. */}
          <div className="bg-spectrum relative w-full max-w-3xl rounded-xl p-px shadow-[0_0_60px_rgba(166,77,255,0.35)]">
            <Launcher />
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
          <p className="mx-auto mt-4 max-w-xl text-lg text-muted">Build it, create an agent, and give it something to do.</p>
          <div className="mx-auto mt-8 max-w-xl rounded-2xl bg-surface p-6 text-left font-mono text-sm leading-7 text-white/80">
            <div>
              <span className="text-white/35">$</span> just install
            </div>
            <div>
              <span className="text-white/35">$</span> axel agent new atlas --harness claude
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
