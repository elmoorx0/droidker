import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// During development, the SvelteKit dev server runs on :3000 and proxies
// /api/* to the Rust backend on :8080. In production we recommend running
// the dashboard behind nginx which routes /api/* to the backend directly.
const backendUrl = process.env.DROIDKER_BACKEND || 'http://127.0.0.1:8080';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    host: '0.0.0.0',
    port: 3000,
    proxy: {
      '/api': {
        target: backendUrl,
        changeOrigin: true,
      },
    },
  },
});
