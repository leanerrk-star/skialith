import { defineConfig } from 'astro/config';

import cloudflare from "@astrojs/cloudflare";

export default defineConfig({
  site: 'https://skialith-protocol.org',
  compressHTML: true,
  output: "hybrid",
  adapter: cloudflare()
});