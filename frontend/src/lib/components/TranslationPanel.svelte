<script lang="ts">
  // TranslationPanel.svelte (M7.4)
  //
  // Polls `GET /api/v1/ready` and renders the host's translation
  // capability table — which ABIs are supported and which translator
  // (libhoudini / libndk_translation / qemu-user / native / none) will
  // be used for each.
  //
  // Mounted on the containers list page so the user knows upfront
  // whether their ARM APK will run on this host.

  import { onMount, onDestroy } from 'svelte';
  import { api, type ReadyResponse } from '$lib/api/api';

  let ready: ReadyResponse | null = null;
  let loading = true;
  let error: string | null = null;
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  const POLL_INTERVAL_MS = 10_000; // 10 seconds

  async function refresh() {
    try {
      ready = await api.ready();
      error = null;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    refresh();
    pollTimer = setInterval(refresh, POLL_INTERVAL_MS);
  });

  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });

  // Translate the strategy token into a human-friendly label + colour.
  function strategyLabel(token: string): { label: string; color: string } {
    switch (token) {
      case 'native':
        return { label: 'Native', color: 'text-emerald-400' };
      case 'libhoudini':
        return { label: 'libhoudini', color: 'text-sky-400' };
      case 'libndk_translation':
        return { label: 'libndk_translation', color: 'text-indigo-400' };
      case 'qemu-user':
        return { label: 'qemu-user', color: 'text-amber-400' };
      case 'none':
        return { label: 'Unavailable', color: 'text-rose-400' };
      default:
        return { label: token, color: 'text-slate-400' };
    }
  }

  function formatBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }
</script>

<div class="rounded-lg border border-slate-700 bg-slate-800/50 p-4">
  <div class="flex items-center justify-between">
    <h3 class="text-sm font-semibold text-slate-200">
      Translation Capability
    </h3>
    <button
      class="rounded border border-slate-600 px-2 py-0.5 text-xs text-slate-300 hover:bg-slate-700"
      on:click={refresh}
      disabled={loading}
    >
      {loading ? '...' : 'Refresh'}
    </button>
  </div>

  {#if error}
    <div class="mt-3 rounded border border-rose-500/40 bg-rose-500/10 p-2 text-xs text-rose-300">
      {error}
    </div>
  {:else if !ready}
    <div class="mt-3 text-xs text-slate-400">Loading…</div>
  {:else}
    <div class="mt-3 space-y-2 text-xs">
      <div class="flex justify-between">
        <span class="text-slate-400">Host arch</span>
        <span class="font-mono text-slate-200">{ready.host_arch}</span>
      </div>
      <div class="flex justify-between">
        <span class="text-slate-400">Containers loaded</span>
        <span class="font-mono text-slate-200">{ready.containers_loaded}</span>
      </div>

      <div class="mt-3 border-t border-slate-700 pt-3">
        <div class="mb-2 text-slate-400">Per-ABI translator</div>
        <table class="w-full text-left">
          <thead class="text-slate-500">
            <tr>
              <th class="pr-2 pb-1 font-medium">ABI</th>
              <th class="pr-2 pb-1 font-medium">Strategy</th>
              <th class="pb-1 font-medium">Usable</th>
            </tr>
          </thead>
          <tbody>
            {#each Object.entries(ready.translation) as [abi, info]}
              {@const s = strategyLabel(info.strategy)}
              <tr class="border-t border-slate-700/50">
                <td class="pr-2 py-1 font-mono text-slate-200">{abi}</td>
                <td class="pr-2 py-1 font-mono {s.color}">{s.label}</td>
                <td class="py-1">
                  {#if info.usable}
                    <span class="text-emerald-400">✓</span>
                  {:else}
                    <span class="text-rose-400">✗</span>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>

      {#if ready.translation['arm64-v8a'] && !ready.translation['arm64-v8a'].usable}
        <div class="mt-2 rounded border border-amber-500/40 bg-amber-500/10 p-2 text-amber-300">
          ARM64 APKs won't run on this host. Install a translator:
          <code class="ml-1 rounded bg-slate-900 px-1">sudo bash scripts/install-translation.sh</code>
        </div>
      {/if}
    </div>
  {/if}
</div>
