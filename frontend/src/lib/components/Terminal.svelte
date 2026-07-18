<script lang="ts">
  // Terminal.svelte — interactive terminal widget for one container.
  //
  // Wires xterm.js to `/api/v1/containers/{id}/exec/ws` with PTY mode.
  // The user types into xterm; we forward keystrokes as `{type:"stdin",data:<base64>}`
  // frames. The server sends back `{type:"stdout",data:<base64>}` frames that
  // we decode and write into xterm.
  //
  // Wire protocol mirrors the daemon's ws::exec::ClientMsg/ServerMsg JSON
  // schema (see backend/src/exec/session.rs + ws/exec.rs).

  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import '@xterm/xterm/css/xterm.css';

  export let containerId: string;
  export let apiBase: string;
  export let cmd: string[] = ['/system/bin/sh'];
  export let cwd: string | null = null;

  let termContainer: HTMLDivElement;
  let term: Terminal | null = null;
  let fit: FitAddon | null = null;
  let ws: WebSocket | null = null;
  let connected = false;
  let error: string | null = null;
  let destroyed = false;

  function buildUrl(): string {
    const params = new URLSearchParams();
    for (const c of cmd) params.append('cmd', c);
    if (cwd) params.set('cwd', cwd);
    params.set('tty', 'true');
    params.set('rows', String(term?.rows ?? 24));
    params.set('cols', String(term?.cols ?? 80));
    return `${apiBase.replace(/^http/, 'ws')}/api/v1/containers/${containerId}/exec/ws?${params.toString()}`;
  }

  function b64encode(s: string): string {
    const bytes = new TextEncoder().encode(s);
    let bin = '';
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin);
  }

  function b64decode(s: string): Uint8Array {
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function connect() {
    if (!term) return;
    if (ws) { try { ws.close(); } catch { /* ignore */ } }

    const url = buildUrl();
    try {
      ws = new WebSocket(url);
    } catch (e) {
      error = `WebSocket failed: ${(e as Error).message}`;
      return;
    }

    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      connected = true;
      error = null;
      term?.writeln('\r\n\x1b[32m[droidker] connected\x1b[0m\r\n');
      // Send an initial resize so the server's PTY matches the client
      // geometry from the very first byte.
      sendResize();
    };

    ws.onclose = () => {
      connected = false;
      if (!destroyed) {
        term?.writeln('\r\n\x1b[33m[droidker] disconnected — will retry in 2s\x1b[0m');
        setTimeout(() => { if (!destroyed) connect(); }, 2000);
      }
    };

    ws.onerror = () => { error = 'exec WebSocket error'; };

    ws.onmessage = (ev) => {
      try {
        const msg = JSON.parse(typeof ev.data === 'string' ? ev.data : '');
        if (msg.type === 'stdout' || msg.type === 'stderr') {
          const bytes = b64decode(msg.data as string);
          const text = new TextDecoder().decode(bytes);
          term?.write(text);
        } else if (msg.type === 'exit') {
          term?.writeln(`\r\n\x1b[33m[droidker] process exited with code ${msg.code}\x1b[0m`);
          connected = false;
          try { ws?.close(); } catch { /* ignore */ }
        } else if (msg.type === 'error') {
          term?.writeln(`\r\n\x1b[31m[droidker] error: ${msg.message}\x1b[0m`);
        }
      } catch {
        // Non-JSON frame — write the raw text so debugging is easy.
        if (typeof ev.data === 'string') term?.write(ev.data);
      }
    };
  }

  function sendStdin(text: string) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify({ type: 'stdin', data: b64encode(text) }));
  }

  function sendResize() {
    if (!ws || ws.readyState !== WebSocket.OPEN || !term) return;
    ws.send(JSON.stringify({ type: 'resize', rows: term.rows, cols: term.cols }));
  }

  function handleResize() {
    if (!fit || !term) return;
    try {
      fit.fit();
      sendResize();
    } catch { /* ignore */ }
  }

  onMount(() => {
    term = new Terminal({
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      fontSize: 13,
      cursorBlink: true,
      scrollback: 5000,
      theme: {
        background: '#0f172a',
        foreground: '#e2e8f0',
        cursor: '#e2e8f0',
        selectionBackground: '#1e3a5f',
        black: '#0f172a',
        red: '#ef4444',
        green: '#10b981',
        yellow: '#eab308',
        blue: '#3b82f6',
        magenta: '#a855f7',
        cyan: '#06b6d4',
        white: '#e2e8f0',
        brightBlack: '#475569',
        brightRed: '#f87171',
        brightGreen: '#34d399',
        brightYellow: '#facc15',
        brightBlue: '#60a5fa',
        brightMagenta: '#c084fc',
        brightCyan: '#22d3ee',
        brightWhite: '#f1f5f9'
      }
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(termContainer);
    try { fit.fit(); } catch { /* ignore */ }
    term.writeln('\x1b[32m[droidker] starting shell...\x1b[0m');
    term.onData((data) => sendStdin(data));
    // Connect once xterm is ready.
    connect();
    // Track viewport resizes.
    window.addEventListener('resize', handleResize);
  });

  onDestroy(() => {
    destroyed = true;
    window.removeEventListener('resize', handleResize);
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
    term?.dispose();
  });
</script>

<div class="panel p-3">
  <div class="flex items-center justify-between mb-2">
    <h3 class="text-sm font-medium text-slate-200">Terminal</h3>
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

  <div bind:this={termContainer} class="bg-slate-900 rounded p-2 min-h-[400px]"></div>
</div>
