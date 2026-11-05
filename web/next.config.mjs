import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // `curl -fsSL https://axel.md/install | sh`. `/skill` and `/SKILL.md` are the agent
  // skill; the full skill with its references is under /.well-known/agent-skills/
  // (Agent Skills discovery: `npx skills add https://axel.md`). All generated at build
  // by scripts/agent-files.mjs from content/docs.
  async rewrites() {
    return [
      { source: '/install', destination: '/install.sh' },
      { source: '/skill', destination: '/SKILL.md' },
    ];
  },
  async headers() {
    return [
      {
        source: '/install(\\.sh)?',
        headers: [{ key: 'Content-Type', value: 'text/plain; charset=utf-8' }],
      },
      {
        // For agents and LLMs: served as text, readable with curl.
        source: '/(skill|SKILL\\.md|llms\\.txt|llms-full\\.txt)',
        headers: [{ key: 'Content-Type', value: 'text/plain; charset=utf-8' }],
      },
      {
        source: '/.well-known/agent-skills/:path*.md',
        headers: [{ key: 'Content-Type', value: 'text/plain; charset=utf-8' }],
      },
      {
        source: '/.well-known/agent-skills/index.json',
        headers: [
          { key: 'Content-Type', value: 'application/json; charset=utf-8' },
          { key: 'Access-Control-Allow-Origin', value: '*' },
        ],
      },
    ];
  },
  async redirects() {
    return [
      // The docs used to live on their own subdomain.
      {
        source: '/:path*',
        has: [{ type: 'host', value: 'docs.axel.md' }],
        destination: 'https://axel.md/docs/:path*',
        permanent: true,
      },
    ];
  },
};

export default withMDX(nextConfig);
