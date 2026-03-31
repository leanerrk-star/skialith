import { defineConfig } from 'astro/config';

import cloudflare from "@astrojs/cloudflare";

export default defineConfig({
  site: 'https://skialith.io',
  compressHTML: true,
  output: "hybrid",
  adapter: cloudflare()
});