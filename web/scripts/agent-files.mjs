#!/usr/bin/env node
// Files for agents and language models, generated from the docs at build time so they
// can't drift: `content/docs/*.mdx` is the one source.
//
//   public/llms.txt                                 the map (llmstxt.org)
//   public/llms-full.txt                            every page, in one file
//   public/.well-known/agent-skills/index.json      Agent Skills discovery index (0.2.0)
//   public/.well-known/agent-skills/axel.tar.gz     the skill: skill/SKILL.md + references/
//   public/.well-known/agent-skills/axel/           the same, unpacked, for browsing and curl
//   public/SKILL.md                                 a copy of the skill's SKILL.md (/skill)
//
// `npx skills add https://axel.md` reads the index, fetches the archive, checks its
// sha256 and installs SKILL.md with its references. No dependencies: node only.

import { createHash } from 'node:crypto';
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const docsDir = join(root, 'content', 'docs');
const publicDir = join(root, 'public');
// AGENT_FILES_SITE points the index at a test server (the CLI checks the digest, not the host).
const site = process.env.AGENT_FILES_SITE?.replace(/\/$/, '') || 'https://axel.md';
const skillName = 'axel';
const wellKnown = join(publicDir, '.well-known', 'agent-skills');

// ------------------------------------------------------------------ the docs

/** The pages in the sidebar's order (meta.json), each with its frontmatter and body. */
function pages() {
  const meta = JSON.parse(readFileSync(join(docsDir, 'meta.json'), 'utf8'));
  const ordered = meta.pages.filter((p) => !p.startsWith('---'));
  const onDisk = readdirSync(docsDir).filter((f) => f.endsWith('.mdx')).map((f) => f.slice(0, -4));
  for (const slug of onDisk) {
    if (!ordered.includes(slug)) ordered.push(slug);
  }
  return ordered.map((slug) => {
    const raw = readFileSync(join(docsDir, `${slug}.mdx`), 'utf8');
    const { front, body } = splitFrontmatter(raw);
    return { slug, title: front.title ?? slug, description: front.description ?? '', body: toMarkdown(body) };
  });
}

function splitFrontmatter(raw) {
  const m = raw.match(/^---\n([\s\S]*?)\n---\n?/);
  if (!m) return { front: {}, body: raw };
  const front = {};
  for (const line of m[1].split('\n')) {
    const kv = line.match(/^(\w+):\s*(.*)$/);
    if (kv) front[kv[1]] = kv[2].replace(/^["']|["']$/g, '');
  }
  return { front, body: raw.slice(m[0].length) };
}

/** MDX to plain Markdown: components become lists, site links become absolute. */
function toMarkdown(body) {
  let out = body.replace(/^import .*$\n?/gm, '');
  // <Card title="…" href="…" description="…" /> → a list item.
  out = out.replace(/<Card\s+([^>]*?)\/>/g, (_, attrs) => {
    const a = {};
    for (const m of attrs.matchAll(/(\w+)="([^"]*)"/g)) a[m[1]] = m[2];
    const href = a.href?.startsWith('/') ? `${site}${a.href}` : a.href ?? '';
    return `- [${a.title ?? href}](${href})${a.description ? `: ${a.description}` : ''}`;
  });
  out = out.replace(/^\s*<\/?Cards>\s*$\n?/gm, '');
  // [text](/docs/x#y) → absolute.
  out = out.replace(/\]\((\/[^)\s]*)\)/g, (_, p) => `](${site}${p})`);
  return out.replace(/\n{3,}/g, '\n\n').trim() + '\n';
}

// ---------------------------------------------------------------- llms.txt

function llmsTxt(all) {
  const lines = [
    '# Axel',
    '',
    '> Agent orchestration, designed for Rex. Threads and named agents, back in the terminal. You type what you want done; a named agent (Claude Code, Codex, OpenCode, Goose, Cursor and more, run as their real CLIs) works on it in a durable thread, on this machine or another one on your tailnet.',
    '',
    `Docs: ${site}/docs. Install: \`curl -fsSL ${site}/install | sh\`, then run \`axel\` in a Rex terminal.`,
    '',
    '## Docs',
    '',
    ...all.map((p) => `- [${p.title}](${site}/docs/${p.slug === 'index' ? '' : p.slug}): ${p.description}`),
    '',
    '## For agents',
    '',
    `- [SKILL.md](${site}/SKILL.md): an Agent Skill that installs, sets up and runs Axel on someone's behalf. \`npx skills add ${site}\` installs it with its references from the well-known index at ${site}/.well-known/agent-skills/index.json.`,
    `- [llms-full.txt](${site}/llms-full.txt): every docs page in one file.`,
    '',
    '## Source and design',
    '',
    '- [Repository](https://github.com/scarce/axel): Rust workspace, Apache-2.0. Every commit on `main` that passes CI is a release.',
    '- [Design](https://github.com/scarce/axel/blob/main/docs/DESIGN.md): the source of truth for concepts and flows.',
    '- [Decision records](https://github.com/scarce/axel/tree/main/docs/adr): why Axel is the way it is.',
    '- [Contracts](https://github.com/scarce/axel/blob/main/docs/CONTRACTS.md): the public API of every crate.',
    '',
  ];
  return lines.join('\n');
}

function llmsFull(all) {
  const parts = [
    '# Axel, every docs page in one file',
    '',
    `Generated from ${site}/docs. Commands are exact; \`axel <command> --help\` has the flags.`,
    '',
  ];
  for (const p of all) {
    parts.push(`# ${p.title}`, '', p.description ? `> ${p.description}` : '', '', p.body, '');
  }
  return parts.join('\n').replace(/\n{3,}/g, '\n\n');
}

// ---------------------------------------------------------------- the skill

/** A deterministic POSIX tar.gz of `files` ({ path → Buffer }), paths at the root. */
function tarGz(files) {
  const blocks = [];
  const header = (path, size) => {
    const h = Buffer.alloc(512, 0);
    h.write(path, 0, 100, 'utf8');
    h.write('0000644\0', 100, 8); // mode
    h.write('0000000\0', 108, 8); // uid
    h.write('0000000\0', 116, 8); // gid
    h.write(size.toString(8).padStart(11, '0') + '\0', 124, 12);
    h.write('00000000000\0', 136, 12); // mtime: fixed, so the digest only changes with content
    h.write('        ', 148, 8); // checksum placeholder
    h.write('0', 156, 1); // regular file
    h.write('ustar\0', 257, 6);
    h.write('00', 263, 2);
    const sum = [...h].reduce((a, b) => a + b, 0);
    h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 8);
    return h;
  };
  for (const path of Object.keys(files).sort()) {
    const data = files[path];
    if (path.length > 100) throw new Error(`tar path too long: ${path}`);
    blocks.push(header(path, data.length), data);
    const pad = (512 - (data.length % 512)) % 512;
    if (pad) blocks.push(Buffer.alloc(pad, 0));
  }
  blocks.push(Buffer.alloc(1024, 0));
  return gzipSync(Buffer.concat(blocks), { level: 9 });
}

function buildSkill(all) {
  const skillMd = readFileSync(join(root, 'skill', 'SKILL.md'), 'utf8');
  const front = splitFrontmatter(skillMd).front;
  if (front.name !== skillName) throw new Error(`skill/SKILL.md must be named ${skillName}`);
  if (!front.description) throw new Error('skill/SKILL.md needs a description');
  // Progressive disclosure: the body is read whole once the skill triggers; detail belongs in references/.
  if (skillMd.split('\n').length > 500) throw new Error('skill/SKILL.md is over 500 lines; move detail into the docs (references/)');
  const files = { 'SKILL.md': Buffer.from(skillMd) };
  for (const p of all) {
    const text = `# ${p.title}\n\n${p.description ? `> ${p.description}\n\n` : ''}From ${site}/docs/${p.slug === 'index' ? '' : p.slug}\n\n${p.body}`;
    files[`references/${p.slug}.md`] = Buffer.from(text);
  }
  // Every reference the body names must exist, so a stale table fails the build.
  for (const m of skillMd.matchAll(/`references\/([a-z0-9-]+\.md)`/g)) {
    if (!files[`references/${m[1]}`]) throw new Error(`skill/SKILL.md names references/${m[1]}, which no docs page produces`);
  }
  return { files, description: readDescription(skillMd) };
}

/** The frontmatter description, folded onto one line (YAML `>`). */
function readDescription(skillMd) {
  const m = skillMd.match(/^description:\s*>?\s*\n((?:[ \t]+.*\n)+)/m);
  const text = m ? m[1].split('\n').map((l) => l.trim()).filter(Boolean).join(' ') : splitFrontmatter(skillMd).front.description;
  if (!text || text.length > 1024) throw new Error('skill description must be 1–1024 characters');
  return text;
}

// ---------------------------------------------------------------------- main

const all = pages();
mkdirSync(publicDir, { recursive: true });
writeFileSync(join(publicDir, 'llms.txt'), llmsTxt(all));
writeFileSync(join(publicDir, 'llms-full.txt'), llmsFull(all));

const { files, description } = buildSkill(all);
rmSync(wellKnown, { recursive: true, force: true });
mkdirSync(join(wellKnown, skillName, 'references'), { recursive: true });
for (const [path, data] of Object.entries(files)) {
  writeFileSync(join(wellKnown, skillName, path), data);
}
writeFileSync(join(publicDir, 'SKILL.md'), files['SKILL.md']);
const archive = tarGz(files);
const archiveName = `${skillName}.tar.gz`;
writeFileSync(join(wellKnown, archiveName), archive);
const digest = `sha256:${createHash('sha256').update(archive).digest('hex')}`;
const index = {
  $schema: 'https://schemas.agentskills.io/discovery/0.2.0/schema.json',
  skills: [
    {
      name: skillName,
      type: 'archive',
      description,
      url: `${site}/.well-known/agent-skills/${archiveName}`,
      digest,
    },
  ],
};
writeFileSync(join(wellKnown, 'index.json'), JSON.stringify(index, null, 2) + '\n');

console.log(
  `agent files: ${all.length} docs pages → llms.txt, llms-full.txt, ${skillName} skill (${Object.keys(files).length} files, ${archive.length} bytes, ${digest.slice(0, 19)}…)`
);
