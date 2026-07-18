<script lang="ts">
  // LiveStatsChart.svelte — live memory + CPU chart for one container.
  //
  // Consumes `/api/v1/containers/{id}/stats/ws` and renders two SVG sparklines
  // (memory + CPU usage). We keep the last 60 samples (≈60s at 1Hz polling)
  // and redraw on each tick.
  //
  // No charting library — just inline SVG so the bundle stays tiny. The
  // 1-GB VPS target makes every KB matter.

  export let containerId: string;
  export let apiBase: string;

  type StatsPayload = {
    memory?: { current: number; max: number; peak: number; oom: number };
    cpu?: { usage_usec: number; uusage_usec: number; quota: number; period: number };
    pids?: { current: number; max: number };
  };

  let samples: { mem: number; cpuPct: number; t: number }[] = [];
  let lastCpu: { usec: number; t: number } | null = null;
  let connected = false;
  let error: string | null = null;
  let ws: WebSocket | null = null;

  const MAX_SAMPLES = 60;
  const W = 480;
  const H = 80;
  const PAD = 4;

  $: memPath = buildPath(samples.map((s) => s.mem));
  $: cpuPath = buildPath(samples.map((s) => s.cpuPct));

  function buildPath(values: number[]): string {
    if (values.length < 2) return '';
    const max = Math.max(...values, 1);
    const n = values.length;
    return values
      .map((v, i) => {
        const x = PAD + (i / (n - 1)) * (W - 2 * PAD);
        const y = H - PAD - (v / max) * (H - 2 * PAD);
        return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`;
      })
      .join(' ');
  }

  function connect() {
    if (ws) {
      try { ws.close(); } catch { /* ignore */ }
    }
    const url = `${apiBase.replace(/^http/, 'ws')}/api/v1/containers/${containerId}/stats/ws`;
    try {
      ws = new WebSocket(url);
    } catch (e) {
      error = `WebSocket failed: ${(e as Error).message}`;
      return;
    }
    ws.onopen = () => { connected = true; error = null; };
    ws.onclose = () => {
      connected = false;
      // Auto-reconnect after 2s unless we've been destroyed.
      if (!destroyed) {
        reconnectTimer = setTimeout(connect, 2000);
      }
    };
    ws.onerror = () => { error = 'stats WebSocket error'; };
    ws.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data as string);
        handleStats(msg);
      } catch { /* ignore malformed frames */ }
    };
  }

  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let destroyed = false;

  function handleStats(payload: StatsPayload) {
    const now = Date.now();
    let memBytes = 0;
    let cpuPct = 0;

    if (payload.memory) {
      memBytes = payload.memory.current;
    }
    if (payload.cpu) {
      if (lastCpu) {
        const dUsec = payload.cpu.usage_usec - lastCpu.usec;
        const dtMs = now - lastCpu.t;
        // CPU usage % = (delta_usec / 1000) / dtMs * 100
        // (delta_usec is in microseconds; dtMs is in milliseconds; both
        // cancel out so the formula is just delta_usec / (dtMs * 1000) * 100).
        if (dtMs > 0) {
          cpuPct = Math.min(100, (dUsec / 1000 / dtMs) * 100);
        }
      }
      lastCpu = { usec: payload.cpu.usage_usec, t: now };
    }

    samples.push({ mem: memBytes, cpuPct, t: now });
    if (samples.length > MAX_SAMPLES) samples.shift();
    samples = samples; // trigger reactivity
  }

  function formatBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  onMount(() => {
    connect();
  });

  onDestroy(() => {
    destroyed = true;
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
  });

  // Import lifecycle hooks lazily so the component stays self-contained.
  import { onMount, onDestroy } from 'svelte';
</script>

<div class="panel p-3">
  <div class="flex items-center justify-between mb-2">
    <h3 class="text-sm font-medium text-slate-200">Live Stats</h3>
    <div class="flex items-center gap-2 text-xs">
      <span
        class="inline-block w-2 h-2 rounded-full"
        class:bg-emerald-500={connected}
        class:bg-slate-600={!connected}
      ></span>
      <span class="text-slate-400">{connected ? 'live' : 'disconnected'}</span>
    </div>
  </div>

  {#if error}
    <div class="text-xs text-droid-err mb-2">{error}</div>
  {/if}

  <div class="grid grid-cols-2 gap-3">
    <div>
      <div class="text-xs text-slate-400 mb-1">Memory</div>
      <svg width={W} height={H} class="w-full h-auto bg-slate-900/50 rounded">
        <path d={memPath} stroke="#10b981" stroke-width="1.5" fill="none" />
        {#if samples.length > 0}
          <text x={PAD + 2} y={12} class="text-[10px]" fill="#94a3b8">
            {formatBytes(samples[samples.length - 1].mem)}
          </text>
        {/if}
      </svg>
    </div>
    <div>
      <div class="text-xs text-slate-400 mb-1">CPU %</div>
      <svg width={W} height={H} class="w-full h-auto bg-slate-900/50 rounded">
        <path d={cpuPath} stroke="#3b82f6" stroke-width="1.5" fill="none" />
        {#if samples.length > 0}
          <text x={PAD + 2} y={12} class="text-[10px]" fill="#94a3b8">
            {samples[samples.length - 1].cpuPct.toFixed(1)}%
          </text>
        {/if}
      </svg>
    </div>
  </div>
</div>
