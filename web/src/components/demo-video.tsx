"use client";

import { useRef, useState } from "react";
import { Play, Terminal } from "lucide-react";

/** A lightweight, click-to-play recording with native playback controls. */
export function DemoVideo() {
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
    <section aria-labelledby="demo-title" className="relative z-10 mx-auto max-w-5xl scroll-mt-24 px-6 pb-20 pt-6 sm:pb-28">
      <div className="mb-7 flex flex-wrap items-end justify-between gap-3 sm:mb-9">
        <div>
          <p className="mb-3 text-sm font-medium text-pink">A closer look</p>
          <h2 id="demo-title" className="text-balance text-3xl font-semibold text-fg sm:text-4xl">See Axel in action.</h2>
        </div>
        <p className="text-sm text-muted">Your agents, at home in your terminal.</p>
      </div>

      <figure className="overflow-hidden rounded-2xl border border-line bg-surface shadow-2xl shadow-black/30">
        <div className="flex items-center justify-between gap-4 border-b border-line px-4 py-3 sm:px-5">
          <span className="flex items-center gap-2.5 text-sm text-muted">
            <Terminal className="size-4 text-pink" aria-hidden="true" />
            <span className="font-medium text-fg">axel</span>
            <span className="hidden sm:inline">/ product walkthrough</span>
          </span>
          <span className="text-xs tabular-nums text-muted">01:21 <span aria-hidden="true">·</span> No audio</span>
        </div>

        <div className="relative aspect-[212/135] bg-ink">
          <video
            ref={video}
            aria-label="Axel product walkthrough, silent screen recording"
            aria-describedby="demo-caption"
            className="block size-full object-contain"
            width={1696}
            height={1080}
            poster="/media/axel-demo-poster.jpg"
            preload="none"
            playsInline
            controls={started}
            onPlay={() => { setStarted(true); setError(null); }}
            onError={() => setError("This video couldn’t load. You can try opening it directly.")}
          >
            <source src="/media/axel-demo.mp4" type="video/mp4" />
            <a href="/media/axel-demo.mp4">Watch the Axel demo</a>
          </video>
          {!started && (
            <button
              type="button"
              onClick={play}
              aria-label="Play the Axel demo"
              className="group absolute inset-0 flex flex-col items-center justify-center gap-4 bg-black/25 text-white focus:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-pink"
            >
              <span className="flex size-16 items-center justify-center rounded-full border border-white/30 bg-white text-ink shadow-lg transition-transform duration-150 group-hover:scale-105 motion-reduce:transition-none sm:size-20">
                <Play className="ml-1 size-6 fill-current sm:size-7" aria-hidden="true" />
              </span>
              <span className="rounded-full border border-white/15 bg-ink/90 px-4 py-2 text-sm font-medium">Watch the demo</span>
            </button>
          )}
        </div>

        <figcaption id="demo-caption" className="border-t border-line px-4 py-3 text-sm text-muted sm:px-5">
          A quick tour of Axel in Rex. Play, scrub or go fullscreen to take a closer look.
        </figcaption>
        {error && (
          <p role="alert" className="border-t border-line px-4 py-3 text-sm text-fg sm:px-5">
            {error}{" "}
            <a href="/media/axel-demo.mp4" className="underline underline-offset-4 focus-visible:outline-pink">Open video ↗</a>
          </p>
        )}
      </figure>
    </section>
  );
}
