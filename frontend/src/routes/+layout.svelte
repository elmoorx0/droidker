<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { startPolling, stopPolling } from '$lib/stores/containers';

  onMount(() => {
    startPolling(3000);
    return () => stopPolling();
  });
</script>

<div class="min-h-full flex flex-col">
  <!-- Top navigation bar -->
  <header class="border-b border-droid-border bg-droid-panel/50 backdrop-blur sticky top-0 z-50">
    <div class="max-w-7xl mx-auto px-4 py-3 flex items-center justify-between">
      <div class="flex items-center gap-3">
        <div class="w-8 h-8 rounded-lg bg-gradient-to-br from-droid-accent to-droid-accent-2
                    flex items-center justify-center font-bold text-slate-900">D</div>
        <div>
          <div class="text-sm font-semibold text-slate-100">DroidKer</div>
          <div class="text-xs text-slate-500">Android micro-container platform</div>
        </div>
      </div>

      <nav class="flex items-center gap-1 text-sm">
        <a href="/" class="btn-ghost px-3 py-1.5">Dashboard</a>
        <a href="/containers" class="btn-ghost px-3 py-1.5">Containers</a>
        <a href="/docs" class="btn-ghost px-3 py-1.5">Docs</a>
      </nav>
    </div>
  </header>

  <!-- Page content -->
  <main class="flex-1 max-w-7xl w-full mx-auto px-4 py-6">
    <slot />
  </main>

  <!-- Footer -->
  <footer class="border-t border-droid-border py-3 text-center text-xs text-slate-600">
    DroidKer Milestone 1 · Backend: Rust + Actix-Web · Frontend: SvelteKit + Tailwind
  </footer>
</div>
