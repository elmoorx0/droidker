<script lang="ts">
  import { api } from '$lib/api/api';
  import { refreshContainers } from '$lib/stores/containers';
  import type { ContainerSummary } from '$lib/api/api';
  import StatusBadge from './StatusBadge.svelte';

  export let container: ContainerSummary;
  export let onOpen: (id: string) => void = () => {};

  let busy = false;
  let error: string | null = null;

  async function start() {
    busy = true;
    error = null;
    try {
      await api.startContainer(container.id);
      await refreshContainers();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function stop() {
    busy = true;
    error = null;
    try {
      await api.stopContainer(container.id);
      await refreshContainers();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!confirm(`Delete container "${container.name}"? This cannot be undone.`)) return;
    busy = true;
    error = null;
    try {
      await api.deleteContainer(container.id);
      await refreshContainers();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  $: shortId = container.id.slice(0, 8);
</script>

<div class="panel p-4 hover:border-droid-accent/40 transition-colors">
  <div class="flex items-start justify-between gap-3">
    <div class="min-w-0 flex-1">
      <div class="flex items-center gap-2">
        <span class="font-mono text-xs text-slate-500">{shortId}</span>
        <h3 class="text-sm font-semibold text-slate-100 truncate">{container.name}</h3>
        <StatusBadge status={container.status} />
      </div>
      <div class="mt-2 text-xs text-slate-400 space-y-0.5">
        <div>
          <span class="text-slate-500">pkg:</span>
          <span class="font-mono">{container.package}</span>
        </div>
        <div class="flex gap-4">
          <span>
            <span class="text-slate-500">pid:</span>
            <span class="font-mono">{container.pid || '—'}</span>
          </span>
          <span>
            <span class="text-slate-500">ip:</span>
            <span class="font-mono">{container.ip || '—'}</span>
          </span>
        </div>
      </div>
      {#if error}
        <div class="mt-2 text-xs text-droid-err">{error}</div>
      {/if}
    </div>

    <div class="flex flex-col gap-1.5 shrink-0">
      <button
        class="btn-secondary text-xs px-2 py-1"
        disabled={busy}
        on:click={() => onOpen(container.id)}
      >
        Open
      </button>
      {#if container.status === 'running'}
        <button
          class="btn-danger text-xs px-2 py-1"
          disabled={busy}
          on:click={stop}
        >
          {busy ? '...' : 'Stop'}
        </button>
      {:else if container.status === 'created' || container.status === 'stopped' || container.status === 'exited'}
        <button
          class="btn-primary text-xs px-2 py-1"
          disabled={busy}
          on:click={start}
        >
          {busy ? '...' : 'Start'}
        </button>
      {/if}
      <button
        class="btn-ghost text-xs px-2 py-1"
        disabled={busy || container.status === 'running'}
        on:click={remove}
        title={container.status === 'running' ? 'Stop the container first' : 'Delete'}
      >
        Remove
      </button>
    </div>
  </div>
</div>
