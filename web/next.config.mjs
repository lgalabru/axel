import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
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
