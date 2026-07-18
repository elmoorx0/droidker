import type { Config } from 'tailwindcss';

const config: Config = {
  content: ['./src/**/*.{html,js,svelte,ts}'],
  theme: {
    extend: {
      colors: {
        // Dark, low-glare palette tuned for ops dashboards on low-end screens.
        droid: {
          bg: '#0b0f14',
          panel: '#121821',
          border: '#1f2a36',
          accent: '#22d3ee',
          'accent-2': '#a78bfa',
          ok: '#22c55e',
          warn: '#f59e0b',
          err: '#ef4444',
          muted: '#64748b',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['JetBrains Mono', 'monospace'],
      },
      animation: {
        'pulse-slow': 'pulse 3s cubic-bezier(0.4, 0, 0.6, 1) infinite',
      },
    },
  },
  plugins: [],
};

export default config;
