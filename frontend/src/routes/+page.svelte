<script lang="ts">
  import { containers, refreshContainers } from '$lib/stores/containers';
  import { onMount } from 'svelte';
  import { api } from '$lib/api/api';
  import ContainerCard from '$lib/components/ContainerCard.svelte';
  import UploadPanel from '$lib/components/UploadPanel.svelte';

  let daemonReady = false;
  let daemonError: string | null = null;
  let containersLoaded = 0;

  onMount(async () => {
    try {
      const r = await api.ready();
      daemonReady = r.ready;
      containersLoaded = r.containers_loaded;
    } catch (e) {
      daemonError = e instanceof Error ? e.message : String(e);
    }
    refreshContainers();
  });

  $: running = $containers.items.filter((c) => c.status === 'running').length;
  $: stopped = $containers.items.filter(
    (c) => c.status === 'stopped' || c.status === 'exited'
  ).length;

  function openContainer(id: string) {
    // For Milestone 1 we just route to a per-container page (created in M3).
    alert(`Container detail view lands in Milestone 3. ID: ${id}`);
  }
</script>

<div class="space-y-6">
  <!-- Status banner -->
  <div class="panel p-4 flex items-center justify-between">
    <div class="flex items-center gap-3">
      <div
        class="w-2.5 h-2.5 rounded-full {daemonReady
          ? 'bg-droid-ok animate-pulse-slow'
          : 'bg-droid-err'}"
      ></div>
      <div>
        <div class="text-sm font-medium text-slate-100">
          {daemonReady ? 'Daemon connected' : 'Daemon offline'}
        </div>
        {#if daemonError}
          <div class="text-xs text-droid-err">{daemonError}</div>
        {:else}
          <div class="text-xs text-slate-500">
            {containersLoaded} container(s) loaded on disk · polling every 3s
          </div>
        {/if}
      </div>
    </div>
    <button class="btn-secondary text-xs" on:click={() => refreshContainers()}>
      Refresh
    </button>
  </div>

  <!-- KPI row -->
  <div class="grid grid-cols-3 gap-4">
    <div class="panel p-4">
      <div class="text-xs text-slate-500 uppercase tracking-wider">Total</div>
      <div class="text-3xl font-bold text-slate-100 mt-1">{$containers.items.length}</div>
    </div>
    <div class="panel p-4">
      <div class="text-xs text-slate-500 uppercase tracking-wider">Running</div>
      <div class="text-3xl font-bold text-droid-ok mt-1">{running}</div>
    </div>
    <div class="panel p-4">
      <div class="text-xs text-slate-500 uppercase tracking-wider">Stopped</div>
      <div class="text-3xl font-bold text-droid-err mt-1">{stopped}</div>
    </div>
  </div>

  <!-- Two-column layout: upload + container list -->
  <div class="grid grid-cols-1 lg:grid-cols-3 gap-6">
    <div class="lg:col-span-1">
      <UploadPanel />
    </div>

    <div class="lg:col-span-2">
      <div class="panel p-5">
        <div class="flex items-center justify-between mb-4">
          <h2 class="text-sm font-semibold text-slate-100 flex items-center gap-2">
            <span class="w-2 h-2 rounded-full bg-droid-accent-2"></span>
            Containers
          </h2>
          {#if $containers.loading}
            <span class="text-xs text-slate-500">refreshing...</span>
          {/if}
        </div>

        {#if $containers.error}
          <div class="text-sm text-droid-err bg-droid-err/10 border border-droid-err/30 rounded-md p-3">
            {$containers.error}
          </div>
        {:else if $containers.items.length === 0}
          <div class="text-center py-10 text-slate-500">
            <div class="text-4xl mb-2">∅</div>
            <div class="text-sm">No containers yet. Upload an APK to get started.</div>
          </div>
        {:else}
          <div class="space-y-2">
            {#each $containers.items as c (c.id)}
              <ContainerCard container={c} onOpen={openContainer} />
            {/each}
          </div>
        {/if}
      </div>
    </div>
  </div>
</div>
