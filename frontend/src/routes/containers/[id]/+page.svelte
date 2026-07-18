<script lang="ts">
  // /containers/[id]/+page.svelte — container detail page.
  //
  // Four tabs: Overview (metadata + lifecycle buttons), Stats (live chart),
  // Terminal (interactive shell), Screen (live MJPEG + touch injection).
  // Logs has its own always-visible panel below the tabs because logs are
  // useful in every context.

  import { page } from '$app/stores';
  import { api, type Container } from '$lib/api/api';
  import LiveStatsChart from '$lib/components/LiveStatsChart.svelte';
  import LogViewer from '$lib/components/LogViewer.svelte';
  import ScreenStream from '$lib/components/ScreenStream.svelte';
  import Terminal from '$lib/components/Terminal.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';

  const id = $page.params.id as string;

  let container: Container | null = null;
  let loading = true;
  let error: string | null = null;
  let activeTab: 'overview' | 'stats' | 'terminal' | 'screen' = 'overview';

  // Action status.
  let acting = false;
  let actionError: string | null = null;

  async function load() {
    loading = true;
    error = null;
    try {
      container = await api.getContainer(id);
    } catch (e) {
      error = (e as Error).message;
    } finally {
      loading = false;
    }
  }

  async function doStart() {
    if (!container) return;
    acting = true; actionError = null;
    try {
      container = await api.startContainer(container.id);
    } catch (e) {
      actionError = (e as Error).message;
    } finally { acting = false; }
  }

  async function doStop() {
    if (!container) return;
    acting = true; actionError = null;
    try {
      container = await api.stopContainer(container.id);
    } catch (e) {
      actionError = (e as Error).message;
    } finally { acting = false; }
  }

  async function doDelete() {
    if (!container) return;
    if (!confirm(`Delete container ${container.name}? This cannot be undone.`)) return;
    acting = true; actionError = null;
    try {
      await api.deleteContainer(container.id);
      goto('/containers');
    } catch (e) {
      actionError = (e as Error).message;
      acting = false;
    }
  }

  function fmtDate(s: string | null): string {
    if (!s) return '—';
    try {
      const d = new Date(s);
      return d.toLocaleString();
    } catch {
      return s;
    }
  }

  const apiBase = (typeof window !== 'undefined') ? `${window.location.protocol}//${window.location.host}` : '';

  let pollTimer: ReturnType<typeof setInterval>;

  onMount(() => {
    load();
    // Poll container metadata every 3s so lifecycle changes propagate back
    // to the dashboard.
    pollTimer = setInterval(() => { if (!acting) load(); }, 3000);
  });

  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });
</script>

<div class="space-y-4">
  {#if loading}
    <div class="panel p-8 text-center text-slate-500">Loading container…</div>
  {:else if error}
    <div class="text-sm text-droid-err bg-droid-err/10 border border-droid-err/30 rounded-md p-3">
      {error}
    </div>
  {:else if container}
    <!-- Header -->
    <div class="panel p-4">
      <div class="flex items-start justify-between gap-4">
        <div>
          <div class="flex items-center gap-2 mb-1">
            <h1 class="text-lg font-semibold text-slate-100">{container.name}</h1>
            <StatusBadge status={container.status} />
          </div>
          <div class="text-xs text-slate-400 font-mono">{container.id}</div>
          <div class="text-xs text-slate-500 mt-1">{container.package}</div>
        </div>
        <div class="flex flex-wrap items-center gap-2">
          {#if container.status === 'running'}
            <button class="btn-secondary text-xs" on:click={doStop} disabled={acting}>Stop</button>
          {:else}
            <button class="btn-primary text-xs" on:click={doStart} disabled={acting}>Start</button>
          {/if}
          <button class="btn-secondary text-xs text-droid-err" on:click={doDelete} disabled={acting}>Delete</button>
        </div>
      </div>
      {#if actionError}
        <div class="mt-3 text-xs text-droid-err bg-droid-err/10 border border-droid-err/30 rounded p-2">
          {actionError}
        </div>
      {/if}
    </div>

    <!-- Tabs -->
    <div class="panel p-1 inline-flex gap-1">
      <button
        class="px-3 py-1.5 text-xs rounded transition-colors"
        class:bg-slate-700={activeTab === 'overview'}
        class:text-slate-100={activeTab === 'overview'}
        class:text-slate-400={activeTab !== 'overview'}
        on:click={() => (activeTab = 'overview')}
      >Overview</button>
      <button
        class="px-3 py-1.5 text-xs rounded transition-colors"
        class:bg-slate-700={activeTab === 'stats'}
        class:text-slate-100={activeTab === 'stats'}
        class:text-slate-400={activeTab !== 'stats'}
        on:click={() => (activeTab = 'stats')}
      >Stats</button>
      <button
        class="px-3 py-1.5 text-xs rounded transition-colors"
        class:bg-slate-700={activeTab === 'terminal'}
        class:text-slate-100={activeTab === 'terminal'}
        class:text-slate-400={activeTab !== 'terminal'}
        on:click={() => (activeTab = 'terminal')}
      >Terminal</button>
      <button
        class="px-3 py-1.5 text-xs rounded transition-colors"
        class:bg-slate-700={activeTab === 'screen'}
        class:text-slate-100={activeTab === 'screen'}
        class:text-slate-400={activeTab !== 'screen'}
        on:click={() => (activeTab = 'screen')}
      >Screen</button>
    </div>

    <!-- Tab body -->
    {#if activeTab === 'overview'}
      <div class="panel p-4 space-y-3">
        <div class="grid grid-cols-2 gap-3 text-sm">
          <div>
            <div class="text-xs text-slate-500 mb-1">Status</div>
            <div class="text-slate-200 capitalize">{container.status}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">PID</div>
            <div class="text-slate-200 font-mono">{container.pid || '—'}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">IP address</div>
            <div class="text-slate-200 font-mono">{container.ip || '—'}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">Host veth</div>
            <div class="text-slate-200 font-mono">{container.veth_host || '—'}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">Memory limit</div>
            <div class="text-slate-200">{container.memory_mb} MB</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">CPU quota</div>
            <div class="text-slate-200">{container.cpu_percent}%</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">APK SHA-256</div>
            <div class="text-slate-200 font-mono text-xs break-all">{container.apk_sha256}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">Created</div>
            <div class="text-slate-200 text-xs">{fmtDate(container.created_at)}</div>
          </div>
          <div>
            <div class="text-xs text-slate-500 mb-1">Updated</div>
            <div class="text-slate-200 text-xs">{fmtDate(container.updated_at)}</div>
          </div>
          <div class="col-span-2">
            <div class="text-xs text-slate-500 mb-1">Published Ports</div>
            <div class="text-slate-200 font-mono text-xs">
              {#if container.ports && container.ports.length > 0}
                {container.ports.map((p) => `${p.host}:${p.container}`).join('  ')}
              {:else}
                —
              {/if}
            </div>
          </div>
        </div>
        {#if container.notes}
          <div>
            <div class="text-xs text-slate-500 mb-1">Notes</div>
            <div class="text-slate-300 text-sm">{container.notes}</div>
          </div>
        {/if}
      </div>
    {:else if activeTab === 'stats'}
      {#if container.status === 'running'}
        <LiveStatsChart containerId={id} {apiBase} />
      {:else}
        <div class="panel p-8 text-center text-slate-500 text-sm">
          Container is not running — start it to see live stats.
        </div>
      {/if}
    {:else if activeTab === 'terminal'}
      {#if container.status === 'running'}
        <Terminal containerId={id} {apiBase} />
      {:else}
        <div class="panel p-8 text-center text-slate-500 text-sm">
          Container is not running — start it to open a terminal.
        </div>
      {/if}
    {:else if activeTab === 'screen'}
      {#if container.status === 'running'}
        <div class="panel p-4">
          <ScreenStream containerId={id} />
        </div>
      {:else}
        <div class="panel p-8 text-center text-slate-500 text-sm">
          Container is not running — start it to see the screen.
        </div>
      {/if}
    {/if}

    <!-- Always-visible logs panel -->
    {#if container.status === 'running' || activeTab === 'overview'}
      <LogViewer containerId={id} {apiBase} kind="runtime" />
    {/if}
  {/if}
</div>
