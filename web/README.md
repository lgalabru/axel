# axel.md

The Axel website, one Next.js app: the landing page at `/` (`src/app/(home)`) and the
documentation at `/docs` (`src/app/(docs)`, Fumadocs, pages in `content/docs/*.mdx`).
Each half has its own root layout and stylesheet, so their styles never mix.

The landing page is built from `src/components/home/`: the page shell with the
hero and the product video (`site.tsx`) and a grid of feature tiles (`bento.tsx`).
The tiles are Stageshow stages; the kit is
vendored in `src/stageshow/kit` (see `src/stageshow/README.md`). Scroll parallax
uses Motion. The repository's star count in the navigation is read on the server,
at most once an hour.

```bash
pnpm install
pnpm dev         # http://localhost:3000 and http://localhost:3000/docs
pnpm build
```

`pnpm build` first runs `scripts/agent-files.mjs`, which generates the files for agents and
language models from `content/docs/*.mdx`: `public/llms.txt`, `public/llms-full.txt`, and the
Agent Skill at `public/.well-known/agent-skills/` (`index.json` in the 0.2.0 discovery format,
`axel.tar.gz` with its sha256 in the index, and the same files unpacked under `axel/`), plus
`public/SKILL.md` for `/skill`. The skill's short body is hand-written in `skill/SKILL.md`;
every docs page becomes one of its `references/`. They're git-ignored: never edit them in
`public/`. `npx skills add https://axel.md` installs the skill; to test against a local
server, generate with `AGENT_FILES_SITE=http://127.0.0.1:8765` and serve `public/`.

URLs live in `src/lib/site.ts`. On Vercel, the project's root directory is `web`.
Requests to the old `docs.axel.md` redirect to `axel.md/docs` (see `next.config.mjs`),
as long as that domain stays attached to the project.

The favicons in `src/app` are cut from `assets/axel-icon-source.png`; `public/og.png` is `assets/axel-og.png`.
`assets/make-mascot.py` cuts the mascot out of `assets/axel-mascot-source.png` (its checkerboard is painted in) and writes the hero image, icons and social preview. Run it from `web/`.

## README banner

`docs/assets/axel-banner-dark.png` and `-light.png` (2560x800, transparent) are the mascot
(`web/assets/axel-mascot.png`) with the Product Hunt badges stacked on its right, rendered from HTML with
headless Chrome at 2x. The badges are drawn, not Product Hunt's live widgets, so the vote
count (280) is fixed. The README shows them at `width="100%"`, so keep them wide and short.
