"use client";

import { ArrowRight, Github, Play, Star } from "lucide-react";
import { MotionConfig, motion, useScroll, useTransform } from "motion/react";
import { useRef, useState, type ReactNode } from "react";
import { DOCS, REPO } from "@/lib/site";
import { Bento } from "./bento";
import "./home.css";

const REX = "https://www.superlogical.com/rex";

const links = [
  { href: "#demo", label: "Demo" },
  { href: "#features", label: "Features" },
  { href: "#rex", label: "Rex" },
  { href: DOCS, label: "Docs" },
];

const rexPoints = [
  {
    title: "Sessions that last.",
    body: "A session survives disconnects and devices. Close the lid; the work keeps going.",
  },
  {
    title: "Status on the tab.",
    body: "Programs report when they are working, done or blocked on you, and Rex shows it.",
  },
  {
    title: "Your other machines.",
    body: "Servers join your tailnet on their own, so an agent can run on the mini at home.",
  },
  {
    title: "Real scrollback.",
    body: "A thread streams in as plain terminal output. You scroll it like anything else.",
  },
];

/** A block that rises into place the first time it scrolls into view. */
export function Reveal({
  children,
  delay = 0,
  className,
}: {
  children: ReactNode;
  delay?: number;
  className?: string;
}) {
  return (
    <motion.div
      className={className}
      initial={{ opacity: 0, y: 28 }}
      whileInView={{ opacity: 1, y: 0 }}
      viewport={{ once: true, margin: "0px 0px -12% 0px" }}
      transition={{ duration: 0.7, delay, ease: [0.22, 1, 0.36, 1] }}
    >
      {children}
    </motion.div>
  );
}

/**
 * The floating navigation. `stars` is the repository's star count, read on the
 * server; it is left out when GitHub could not be reached.
 */
function Header({ stars }: { stars?: number | null }) {
  return (
    <>
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <header className="site-header">
        <nav className="navigation" aria-label="Main navigation">
          <a className="brand" href="#" aria-label="Axel home">
            <img src="/icon.png" alt="" width={28} height={28} />
            axel
          </a>
          <div className="nav-links">
            {links.map((link) => (
              <a key={link.href} href={link.href}>
                {link.label}
              </a>
            ))}
          </div>
          <a className="nav-badge" href={REPO} target="_blank" rel="noopener noreferrer">
            <Github size={16} />
            GitHub
            {stars != null && (
              <span className="nav-stars" aria-label={`${stars} GitHub stars`}>
                <Star size={12} fill="currentColor" />
                {new Intl.NumberFormat("en", { notation: "compact" }).format(stars)}
              </span>
            )}
          </a>
        </nav>
      </header>
    </>
  );
}

function Hero() {
  const ref = useRef<HTMLElement>(null);
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start start", "end start"] });
  const y = useTransform(scrollYProgress, [0, 1], [0, 160]);
  const mascotY = useTransform(scrollYProgress, [0, 1], [0, -60]);
  return (
    <section ref={ref} className="hero" aria-labelledby="hero-heading">
      <motion.div className="hero-copy shell" style={{ y }}>
        {/* Axel the axolotl. */}
        <motion.img
          className="hero-mascot"
          src="/mascot.webp"
          alt="Axel, a smiling axolotl in neon colours"
          width={984}
          height={827}
          style={{ y: mascotY }}
        />
        <h1 id="hero-heading">
          Agent orchestration,
          <br />
          <span>designed for Rex.</span>
        </h1>
        <p className="lead">
          A year of agent apps taught us what works: threads, and named agents. Axel brings them
          back to the terminal, on Rex, where your work already lives, on this machine or any other
          on your tailnet.
        </p>
        <div className="actions">
          <a className="button" href={`${DOCS}/quick-start`}>
            Get started <ArrowRight size={18} />
          </a>
          <code className="install">curl -fsSL https://axel.md/install | sh</code>
        </div>
      </motion.div>
    </section>
  );
}

/** The product walkthrough: a click-to-play recording with native controls. */
function Demo() {
  const video = useRef<HTMLVideoElement>(null);
  const [started, setStarted] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function play() {
    const player = video.current;
    if (!player) return;
    setError(null);
    setStarted(true);
    try {
      await player.play();
    } catch {
      setStarted(false);
      setError("Playback couldn’t start. Try again or open the video directly.");
    }
  }

  return (
    <section className="shell demo-section" id="demo" aria-labelledby="demo-heading">
      <h2 id="demo-heading" className="sr-only">
        See Axel in action
      </h2>
      <Reveal>
        <figure className="demo">
          <video
            ref={video}
            aria-label="Axel product walkthrough, silent screen recording"
            width={1696}
            height={1080}
            poster="/media/axel-demo-poster.jpg"
            preload="none"
            playsInline
            controls={started}
            onPlay={() => {
              setStarted(true);
              setError(null);
            }}
            onError={() => setError("This video couldn’t load. You can try opening it directly.")}
          >
            <source src="/media/axel-demo.mp4" type="video/mp4" />
            <a href="/media/axel-demo.mp4">Watch the Axel demo</a>
          </video>
          {!started && (
            <button type="button" onClick={play} aria-label="Play the Axel demo">
              <span>
                <Play size={26} fill="currentColor" />
              </span>
            </button>
          )}
        </figure>
        <p className="demo-note">A quick tour of Axel in Rex. One minute twenty, no audio.</p>
        {error && (
          <p role="alert" className="demo-error">
            {error} <a href="/media/axel-demo.mp4">Open video</a>
          </p>
        )}
      </Reveal>
    </section>
  );
}

/** Why Rex is the foundation. */
function Rex() {
  const ref = useRef<HTMLDivElement>(null);
  const { scrollYProgress } = useScroll({ target: ref, offset: ["start end", "end start"] });
  const y = useTransform(scrollYProgress, [0, 1], [40, -40]);
  return (
    <section className="section shell" id="rex" aria-labelledby="rex-heading">
      <div ref={ref} className="rex">
        <Reveal className="heading">
          <p className="tag">The foundation</p>
          <h2 id="rex-heading">
            Rex fixed the terminal.
            <br />
            Orchestration can live there again.
          </h2>
          <p>
            The agent apps of the past year found the right ideas, then rebuilt the terminal to
            deliver them: their own windows, their own status lights, their own way to reach a
            remote machine.{" "}
            <a href={REX}>Rex</a>, by Superlogical, starts from scratch on what a terminal session
            is. Axel is just a program in a Rex pane, built on exactly that.
          </p>
        </Reveal>
        <motion.a className="rex-mark" href={REX} aria-label="Rex, by Superlogical" style={{ y }}>
          <img src="/rex.png" alt="Rex" width={320} height={320} />
        </motion.a>
      </div>
      <ul className="points">
        {rexPoints.map((point, index) => (
          <li key={point.title}>
            <Reveal delay={index * 0.08}>
              <h3>{point.title}</h3>
              <p>{point.body}</p>
            </Reveal>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Closing() {
  return (
    <section className="shell" id="install" aria-labelledby="install-heading">
      <Reveal className="closing">
        <img src="/mascot-icon.png" alt="" width={72} height={72} />
        <h2 id="install-heading">Why not today?</h2>
        <p>
          Two commands. The first time, Axel checks your machine and helps you create your first
          agent.
        </p>
        <pre className="closing-code">
          <span>$</span> curl -fsSL https://axel.md/install | sh{"\n"}
          <span>$</span> axel
        </pre>
        <a className="closing-link" href={`${DOCS}/installation`}>
          Read the installation guide <ArrowRight size={18} />
        </a>
      </Reveal>
    </section>
  );
}

function Footer() {
  return (
    <footer className="site-footer shell">
      <a className="brand" href="#" aria-label="Axel home">
        axel
      </a>
      <p>&copy; 2026 Axel</p>
      <span>
        <a href={REPO} target="_blank" rel="noopener noreferrer">
          GitHub
        </a>
        <a href={DOCS}>Docs</a>
      </span>
    </footer>
  );
}

/** The landing page. */
export function Site({ stars }: { stars?: number | null }) {
  return (
    <MotionConfig reducedMotion="user">
      <Header stars={stars} />
      <main id="main">
        <Hero />
        <Demo />
        <section className="section shell" id="features" aria-labelledby="features-heading">
          <Reveal className="heading">
            <h2 id="features-heading">
              A team of agents.
              <br />
              One terminal.
            </h2>
            <p>
              You stop prompting a generic assistant and start working with specialists, each with
              its own name, key, skills and budget.
            </p>
          </Reveal>
          <Bento />
        </section>
        <Rex />
        <Closing />
      </main>
      <Footer />
    </MotionConfig>
  );
}
