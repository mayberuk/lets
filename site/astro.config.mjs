import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

export default defineConfig({
  site: 'https://lets.mayberuk.com',
  trailingSlash: 'always',
  build: { format: 'directory' },
  // The windows render lets output in pre-wrap blocks and set inline elements on separate
  // lines; Astro's default JSX whitespace rules would join those lines and drop the spaces.
  compressHTML: false,
  integrations: [sitemap()],
});
