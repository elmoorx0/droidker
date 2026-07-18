<script lang="ts">
  // LogViewer.svelte — live tail of one container log file.
  //
  // Consumes `/api/v1/containers/{id}/logs/ws?kind=<kind>` and renders the
  // streamed bytes into a scrollable <pre>. The user can pick which log
  // to tail (init / runtime / logcat) and pause auto-scroll.

  import { onMount, onDestroy } from 'svelte';

  export let containerId: string;
  export let apiBase: string;
  export let kind: 'init' | 'runtime' | 'logcat' = 'runtime';

  let text = '';
  let autoScroll = true;
  let connected = false;
  let ws: WebSocket | null = null;
  let pre: HTMLPreElement;
  let destroyed = false;
  let activeKind = kind;

  $: if (activeKind !== kind) {
    kind = activeKind;
    reconnect();
  }

  function buildUrl(): string {
    return `${apiBase.replace(/^http/, 'ws')}/api/v1/containers/${containerId}/logs/ws?kind=${activeKind}`;
  }

  function connect() {
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
    text = '';
    try {
      ws = new WebSocket(buildUrl());
    } catch (e) {
      text = `[error] WebSocket failed: ${(e as Error).message}\n`;
      return;
    }
    ws.onopen = () => { connected = true; };
    ws.onclose = () => {
      connected = false;
      if (!destroyed) {
        setTimeout(() => { if (!destroyed) connect(); }, 2000);
      }
    };
    ws.onerror = () => { text += '[error] log WebSocket error\n'; };
    ws.onmessage = (ev) => {
      // The server sends raw text frames (not JSON) for logs.
      const chunk = typeof ev.data === 'string' ? ev.data : '';
      text += chunk;
      // Cap the buffer at 256 KB so we don't OOM the browser on long-running
      // logcat streams.
      if (text.length > 256 * 1024) {
        text = text.slice(-128 * 1024);
      }
      text = text; // trigger reactivity
      if (autoScroll) {
        // Defer the scroll to next tick so the DOM updates first.
        setTimeout(() => { if (pre) pre.scrollTop = pre.scrollHeight; }, 0);
      }
    };
  }

  function reconnect() {
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
    connect();
  }

  function clearLog() {
    text = '';
  }

  onMount(() => { connect(); });
  onDestroy(() => {
    destroyed = true;
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
  });
</script>

<div class="panel p-3">
  <div class="flex items-center justify-between mb-2">
    <div class="flex items-center gap-2">
      <h3 class="text-sm font-medium text-slate-200">Logs</h3>
      <select class="input text-xs w-32 py-1" bind:value={activeKind}>
        <option value="init">init</option>
        <option value="runtime">runtime</option>
        <option value="logcat">logcat</option>
      </select>
    </div>
    <div class="flex items-center gap-2 text-xs">
      <label class="flex items-center gap-1 text-slate-400 cursor-pointer">
        <input type="checkbox" bind:checked={autoScroll} class="mr-1" />
        auto-scroll
      </label>
      <button class="btn-secondary text-xs px-2 py-1" on:click={clearLog}>clear</button>
      <span
        class="inline-block w-2 h-2 rounded-full"
        class:bg-emerald-500={connected}
        class:bg-slate-600={!connected}
      ></span>
    </div>
  </div>

  <pre
    bind:this={pre}
    class="bg-slate-900 text-slate-200 text-xs p-2 rounded h-80 overflow-auto font-mono whitespace-pre-wrap break-all"
  >{text}</pre>
</div>
