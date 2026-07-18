// lib/stores/containers.ts
//
// Reactive container list with auto-refresh. Used by every dashboard view.

import { writable } from 'svelte/store';
import { api, type ContainerSummary } from '$lib/api/api';

interface ContainerStore {
  items: ContainerSummary[];
  loading: boolean;
  error: string | null;
  lastUpdated: number | null;
}

const initial: ContainerStore = {
  items: [],
  loading: false,
  error: null,
  lastUpdated: null,
};

export const containers = writable<ContainerStore>(initial);

let pollHandle: ReturnType<typeof setInterval> | null = null;

export async function refreshContainers(): Promise<void> {
  containers.update((s) => ({ ...s, loading: true, error: null }));
  try {
    const items = await api.listContainers();
    containers.set({
      items,
      loading: false,
      error: null,
      lastUpdated: Date.now(),
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    containers.update((s) => ({ ...s, loading: false, error: msg }));
  }
}

export function startPolling(intervalMs = 3000): void {
  if (pollHandle) return;
  refreshContainers();
  pollHandle = setInterval(refreshContainers, intervalMs);
}

export function stopPolling(): void {
  if (pollHandle) {
    clearInterval(pollHandle);
    pollHandle = null;
  }
}
