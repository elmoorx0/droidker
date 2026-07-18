// Disable SSR for the dashboard — it's a pure SPA that talks to the daemon
// over REST. SSR would just complicate the deploy story on a 1-vCPU VPS.
export const ssr = false;
export const prerender = false;
