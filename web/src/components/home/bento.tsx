"use client";

import { Laptop, Server } from "lucide-react";
import { motion, useReducedMotion, useScroll, useTransform } from "motion/react";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import type { Act } from "@/stageshow/kit/act";
import { Badge } from "@/stageshow/kit/controls/Badge";
import { Count } from "@/stageshow/kit/motion/Count";
import { States } from "@/stageshow/kit/motion/States";
import { Check } from "@/stageshow/kit/parts/Check";
import { Icon } from "@/stageshow/kit/parts/Icon";
import { Row } from "@/stageshow/kit/parts/Row";
import { Stage } from "@/stageshow/kit/stage";
import "./bento.css";

// The tile list, written before any tile was built (Stageshow, bento.md):
//
//   Tile       Span   Kind        The one thing                        Moves
//   Threads    4 x 1  Hero UI     three sessions of one thread         the last one finishes, the thread settles
//   Cost       2 x 1  Stat        $0.84, on the one dark tile          counts up
//   Harnesses  2 x 1  Collection  the agents Axel runs                 -
//   Agents     4 x 1  Component   two specialists and their skills     -
//   Hosts      3 x 1  Component   the laptop and the mini              -
//   Guard      3 x 1  Component   four modes, one chosen               the tick lands
//
// Six columns, so the rows split 4/2, 2/4 and 3/3 and no two rows share a
// seam. Each picture is its own Stage at 1:1; the ones that move play one
// beat, once, as the grid scrolls in.

const STEP = 110;
const LEAD = 450;

type Beat = { on: boolean };
const before: Beat = { on: false };
const after: Beat = { on: true };
const beat = (index: number) => async (a: Act<Beat>) => {
  await a.wait(LEAD + index * STEP);
  await a.set({ on: true });
};
const scripts = [0, 1, 2, 3, 4, 5].map(beat);

const on = (value: boolean) => (value ? "" : undefined);

/** The tile owns the crop, so each stage is drawn at its tile"s own width. */
function usePictureWidth() {
  const ref = useRef<HTMLDivElement>(null);
  // Zero until measured, so a stage is never drawn at a guessed width and then resized.
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node) return;
    const measure = () => setWidth(Math.max(1, Math.round(node.getBoundingClientRect().width)));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

function useSeen<T extends Element>(delay: number) {
  const ref = useRef<T>(null);
  const [seen, setSeen] = useState(false);
  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    let timer = 0;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (!entry.isIntersecting) return;
        observer.disconnect();
        timer = window.setTimeout(() => setSeen(true), delay);
      },
      { threshold: 0.4 },
    );
    observer.observe(node);
    return () => {
      observer.disconnect();
      window.clearTimeout(timer);
    };
  }, [delay]);
  return [ref, seen] as const;
}

/** True once the grid is wide enough for its six-column layout; stacked tiles hold still. */
function useColumns() {
  const [columns, setColumns] = useState(false);
  useEffect(() => {
    const query = window.matchMedia("(min-width: 1000px)");
    const update = () => setColumns(query.matches);
    update();
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);
  return columns;
}

function Tile({
  area,
  lead,
  children,
  picture,
  dark,
  drift = 0,
}: {
  area: string;
  lead?: string;
  children?: ReactNode;
  picture?: ReactNode;
  dark?: boolean;
  /** How far below its place this tile starts, in px, for the staggered parallax. */
  drift?: number;
}) {
  const ref = useRef<HTMLElement>(null);
  const reduced = useReducedMotion();
  const columns = useColumns();
  // Each tile starts lower than its neighbors and settles as it comes up the screen.
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start end", "start 0.4"] });
  const y = useTransform(scrollYProgress, [0, 1], [drift, 0]);
  return (
    <motion.section
      ref={ref}
      className={`vb-tile vb-tile--${area}`}
      data-dark={on(!!dark)}
      style={reduced || !drift || !columns ? undefined : { y }}
    >
      {lead ? (
        <p className="vb-copy">
          <b>{lead}</b> {children}
        </p>
      ) : (
        children
      )}
      {picture}
    </motion.section>
  );
}

const SESSIONS = [
  { agent: "terra", n: 1, where: "laptop" },
  { agent: "opus", n: 2, where: "laptop" },
  { agent: "terra", n: 3, where: "mini" },
];

function Threads() {
  const [picture, width] = usePictureWidth();
  const pad = width < 480 ? 10 : 14;
  return (
    <Tile
      area="threads"
      lead="The unit of work is the thread, not the tab."
      picture={
        <div ref={picture} className="vb-picture" aria-hidden>
          {width > 0 && (
            <Stage<Beat>
              width={width}
              height={282}
              initial={before}
              poster={after}
              play="once"
              script={scripts[0]}
              className="vb-stage"
            >
              {(scene) => (
                <div className="vb-panel vb-panel--large">
                  {SESSIONS.map((session, index) => {
                    const live = index === 2 && !scene.on;
                    return (
                      <Row
                        key={session.n}
                        lead={<i className="vb-dot" data-agent={session.agent} />}
                        title={
                          <span className="vb-name">
                            {session.agent}
                            <span className="vb-quiet">
                              {" "}
                              · session {session.n} · {session.where}
                            </span>
                          </span>
                        }
                        pad={pad}
                      >
                        {index === 2 ? (
                          <States
                            value={live ? "working" : "ended"}
                            align="end"
                            states={{
                              working: <Badge tone="accent">working</Badge>,
                              ended: <Badge>ended</Badge>,
                            }}
                          />
                        ) : (
                          <Badge>ended</Badge>
                        )}
                      </Row>
                    );
                  })}
                  <div className="vb-life">
                    {["open", "settled", "archived"].map((step, index) => (
                      <span key={step} data-active={on(index === (scene.on ? 1 : 0))}>
                        {step}
                      </span>
                    ))}
                  </div>
                </div>
              )}
            </Stage>
          )}
        </div>
      }
    >
      The conversation, every tool call, the diff, the agents that joined. A chat tab dies with its
      window; a thread doesn&rsquo;t.
    </Tile>
  );
}

function Cost() {
  const reduced = useReducedMotion();
  const [ref, seen] = useSeen<HTMLDivElement>(LEAD + STEP);
  return (
    <Tile area="cost" dark>
      <div ref={ref} className="vb-stat">
        <span className="vb-stat-lead">This thread cost</span>
        <span className="vb-stat-number">
          <span className="vb-stat-unit">$</span>
          <Count
            value={seen || reduced ? 84 : 0}
            format={(n) => (Math.round(n) / 100).toFixed(2)}
            duration={1100}
          />
        </span>
        <span className="vb-stat-label">
          58k tokens, two agents. Every token is counted, per thread and per agent.
        </span>
      </div>
    </Tile>
  );
}

const HARNESSES = ["Claude Code", "Codex", "OpenCode", "Goose", "Cursor", "Any ACP agent"];

function Harnesses() {
  return (
    <Tile
      area="harnesses"
      drift={40}
      lead="Runs the agents you already use."
      picture={
        <span className="vb-foot vb-foot--wrap">
          {HARNESSES.map((label) => (
            <span key={label}>{label}</span>
          ))}
        </span>
      }
    >
      Status comes from hooks or the Agent Client Protocol. No screen scraping.
    </Tile>
  );
}

const AGENTS = [
  { name: "terra", role: "builds", model: "codex", skills: ["rust", "axum", "sqlx"] },
  { name: "opus", role: "reviews", model: "claude/opus", skills: ["review", "security"] },
];

function Agents() {
  const [picture, width] = usePictureWidth();
  return (
    <Tile
      area="agents"
      drift={72}
      lead="Specialists, not one agent that knows everything."
      picture={
        <div ref={picture} className="vb-picture" aria-hidden>
          {width > 0 && (
            <Stage width={width} height={236} className="vb-stage">
              {() => (
                <div className="vb-panel vb-panel--large">
                  {AGENTS.map((agent) => (
                    <Row
                      key={agent.name}
                      lead={<i className="vb-dot" data-agent={agent.name} />}
                      title={
                        <span className="vb-name">
                          {agent.name}
                          <span className="vb-quiet"> {agent.role}</span>
                        </span>
                      }
                      subtitle={
                        <span className="vb-skills">
                          {agent.skills.map((skill) => (
                            <span key={skill}>{skill}</span>
                          ))}
                        </span>
                      }
                      pad={width < 480 ? 10 : 14}
                    >
                      <span className="vb-model">{agent.model}</span>
                    </Row>
                  ))}
                </div>
              )}
            </Stage>
          )}
        </div>
      }
    >
      Each named agent carries only its own skills, key, permissions and budget.
    </Tile>
  );
}

function Hosts() {
  const [picture, width] = usePictureWidth();
  return (
    <Tile
      area="hosts"
      drift={40}
      lead="Ask, then close the laptop."
      picture={
        <div ref={picture} className="vb-picture" aria-hidden>
          {width > 0 && (
            <Stage width={width} height={206} className="vb-stage">
              {() => (
                <div className="vb-panel">
                  <Row
                    lead={
                      <span className="vb-glyph">
                        <Laptop size={18} strokeWidth={1.8} />
                      </span>
                    }
                    title={<span className="vb-name">laptop</span>}
                    subtitle="terra’s key stays here"
                    pad={14}
                  />
                  <Row
                    lead={
                      <span className="vb-glyph" data-on>
                        <Server size={18} strokeWidth={1.8} />
                      </span>
                    }
                    title={<span className="vb-name">mini</span>}
                    subtitle="signs for terra: one thread, 7 days"
                    pad={14}
                  />
                </div>
              )}
            </Stage>
          )}
        </div>
      }
    >
      <code>@terra@mini</code> runs terra on the mini at home. Its branch and uncommitted work come
      back when you reconnect.
    </Tile>
  );
}

const MODES = [
  { mode: "autopilot", what: "never asks" },
  { mode: "edits", what: "file edits go through" },
  { mode: "supervised", what: "asks every time" },
  { mode: "plan", what: "read-only" },
];

function Guard() {
  const [picture, width] = usePictureWidth();
  return (
    <Tile
      area="guard"
      drift={72}
      lead="No babysitting."
      picture={
        <div ref={picture} className="vb-picture" aria-hidden>
          {width > 0 && (
            <Stage<Beat>
              width={width}
              height={262}
              initial={before}
              poster={after}
              play="once"
              script={scripts[5]}
              className="vb-stage"
            >
              {(scene) => (
                <div className="vb-panel">
                  {MODES.map((item, index) => (
                    <Row
                      key={item.mode}
                      title={
                        <span className="vb-name">
                          {item.mode}
                          <span className="vb-quiet"> {item.what}</span>
                        </span>
                      }
                      pad={12}
                    >
                      {index === 0 && <Check on={scene.on} size={22} />}
                    </Row>
                  ))}
                </div>
              )}
            </Stage>
          )}
        </div>
      }
    >
      A mode for routine permissions, a guard that still refuses irreversible commands, and a daily
      budget.
    </Tile>
  );
}

/** The features grid: six tiles of three sizes. */
export function Bento() {
  return (
    <div className="vb">
      <div className="vb-grid">
        <Threads />
        <Cost />
        <Harnesses />
        <Agents />
        <Hosts />
        <Guard />
      </div>
    </div>
  );
}
