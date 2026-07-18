<script lang="ts">
  import { containers, refreshContainers } from '$lib/stores/containers';
  import ContainerCard from '$lib/components/ContainerCard.svelte';
  import { goto } from '$app/navigation';

  let filter = '';
  let statusFilter = 'all';

  $: filtered = $containers.items.filter((c) => {
    if (statusFilter !== 'all' && c.status !== statusFilter) return false;
    if (filter) {
      const q = filter.toLowerCase();
      return (
        c.name.toLowerCase().includes(q) ||
        c.package.toLowerCase().includes(q) ||
        c.id.includes(q)
      );
    }
    return true;
  });

  function open(id: string) {
    goto(`/containers/${id}`);
  }
</script>

<div class="space-y-4">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold text-slate-100">Containers</h1>
    <button class="btn-secondary text-xs" on:click={() => refreshContainers()}>Refresh</button>
  </div>

  <div class="panel p-3 flex gap-3 items-center">
    <input class="input" placeholder="Filter by name, package, or ID..." bind:value={filter} />
    <select class="input w-40" bind:value={statusFilter}>
      <option value="all">All states</option>
      <option value="running">Running</option>
      <option value="created">Created</option>
      <option value="stopped">Stopped</option>
      <option value="exited">Exited</option>
    </select>
  </div>

  {#if $containers.error}
    <div class="text-sm text-droid-err bg-droid-err/10 border border-droid-err/30 rounded-md p-3">
      {$containers.error}
    </div>
  {:else if filtered.length === 0}
    <div class="panel p-8 text-center text-slate-500">
      No containers match the filter.
    </div>
  {:else}
    <div class="space-y-2">
      {#each filtered as c (c.id)}
        <ContainerCard container={c} onOpen={open} />
      {/each}
    </div>
  {/if}
</div>
