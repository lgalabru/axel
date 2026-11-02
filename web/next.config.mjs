import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // `curl -fsSL https://axel.md/install | sh`
  async rewrites() {
    return [{ source: '/install', destination: '/install.sh' }];
  },
  async headers() {
    return [
      {
        source: '/install(\\.sh)?',
        headers: [{ key: 'Content-Type', value: 'text/plain; charset=utf-8' }],
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
