# axel.md

The Axel website, one Next.js app: the landing page at `/` (`src/app/(home)`) and the
documentation at `/docs` (`src/app/(docs)`, Fumadocs, pages in `content/docs/*.mdx`).
Each half has its own root layout and stylesheet, so their styles never mix.

```bash
pnpm install
pnpm dev         # http://localhost:3000 and http://localhost:3000/docs
pnpm build
```

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
