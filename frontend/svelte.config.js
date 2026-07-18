import adapter from '@sveltejs/adapter-node';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    // Node adapter so we can run the dashboard as a single SSR process on the
    // VPS without needing a static file server. Output: frontend/build/
    adapter: adapter(),
  },
};

export default config;
