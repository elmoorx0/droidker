<script lang="ts">
  // ScreenStream.svelte — live Android screen stream + touch injection.
  //
  // Architecture
  // ------------
  //   1. The component opens a WebSocket to /api/v1/containers/{id}/screen/ws.
  //   2. Each WS binary message contains an 8-byte width/height header
  //      (little-endian u32) followed by a complete JPEG file.
  //   3. We decode the JPEG via `createImageBitmap(new Blob([data]))` and
  //      draw it onto a <canvas>.
  //   4. Pointer events on the canvas are translated to container-screen
  //      coordinates (accounting for object-fit scaling) and POSTed to
  //      /api/v1/containers/{id}/screen/touch.
  //   5. The Home/Back/Recent buttons inject key events.
  //
  // The canvas keeps the same aspect ratio as the source frame. We use
  // `object-fit: contain` semantics: the canvas's *internal* size is the
  // frame size (set via canvas.width/height), and its *displayed* size is
  // whatever the parent container allows. The browser handles scaling.
  //
  // Touch coordinate translation:
  //   The canvas's display rect (getBoundingClientRect) may not match its
  //   internal resolution. To get the in-frame pixel coordinate, we:
  //     1. Take the pointer's position relative to the canvas (clientX - rect.left).
  //     2. Scale by (canvas.width / rect.width) and (canvas.height / rect.height).
  //   This gives us the pixel position in the source frame, which we POST
  //   to the touch endpoint.

  import { onMount, onDestroy } from 'svelte';
  import { api } from '$lib/api/api';

  export let containerId: string;
  export let fps: number = 10;
  export let quality: number = 70;

  let canvas: HTMLCanvasElement;
  let ws: WebSocket | null = null;
  let connected = false;
  let error: string | null = null;
  let frameCount = 0;
  let lastFrameTime = performance.now();
  let measuredFps = 0;
  let captureSource = 'unknown';
  let frameWidth = 0;
  let frameHeight = 0;
  let touchSlotMap: Map<number, number> = new Map(); // pointerId -> slot
  let nextSlot = 0;

  // Reconnect backoff
  let reconnectDelay = 250;
  let reconnectTimer: number | null = null;
  let destroyed = false;

  async function connect() {
    if (destroyed) return;
    error = null;
    const url = api.screenWsUrl(containerId);
    try {
      ws = new WebSocket(url);
      ws.binaryType = 'arraybuffer';
    } catch (e) {
      error = `WebSocket failed: ${(e as Error).message}`;
      scheduleReconnect();
      return;
    }

    ws.onopen = () => {
      connected = true;
      reconnectDelay = 250;
      // Send initial config.
      ws?.send(JSON.stringify({ type: 'set_fps', fps }));
      ws?.send(JSON.stringify({ type: 'set_quality', quality }));
    };

    ws.onmessage = async (ev) => {
      if (typeof ev.data === 'string') {
        // Control message from server.
        try {
          const msg = JSON.parse(ev.data);
          if (msg.type === 'error') error = msg.msg;
          if (msg.type === 'pong') { /* heartbeat */ }
        } catch {
          /* ignore */
        }
        return;
      }
      // Binary frame: 8-byte header + JPEG.
      const buf = new Uint8Array(ev.data);
      if (buf.length < 8) return;
      const view = new DataView(ev.data);
      const w = view.getUint32(0, true);
      const h = view.getUint32(4, true);
      frameWidth = w;
      frameHeight = h;
      const jpeg = buf.slice(8);

      // Resize canvas to match the source frame once.
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }

      try {
        const blob = new Blob([jpeg], { type: 'image/jpeg' });
        const bmp = await createImageBitmap(blob);
        const ctx = canvas.getContext('2d');
        if (ctx) {
          ctx.drawImage(bmp, 0, 0, w, h);
        }
        bmp.close();
      } catch (e) {
        // Image decode failures happen occasionally on partial frames — skip.
      }

      // FPS measurement.
      const now = performance.now();
      const dt = now - lastFrameTime;
      lastFrameTime = now;
      if (dt > 0) {
        const inst = 1000 / dt;
        // Exponential moving average for a stable display.
        measuredFps = measuredFps === 0 ? inst : measuredFps * 0.9 + inst * 0.1;
      }
      frameCount += 1;
    };

    ws.onerror = () => {
      error = 'WebSocket error';
    };

    ws.onclose = () => {
      connected = false;
      if (!destroyed) scheduleReconnect();
    };
  }

  function scheduleReconnect() {
    if (reconnectTimer !== null) return;
    reconnectDelay = Math.min(reconnectDelay * 2, 5000);
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, reconnectDelay);
  }

  function disconnect() {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws) {
      ws.onclose = null;
      ws.close();
      ws = null;
    }
    connected = false;
  }

  // ----- Touch event translation --------------------------------------------

  function pointerToFrameCoords(e: PointerEvent): { x: number; y: number } {
    const rect = canvas.getBoundingClientRect();
    // We use object-fit: contain semantics. The canvas may be letterboxed
    // inside its display rect if the aspect ratio differs.
    const frameAspect = canvas.width / canvas.height;
    const rectAspect = rect.width / rect.height;
    let dispW: number, dispH: number, offsetX: number, offsetY: number;
    if (rectAspect > frameAspect) {
      // Letterboxed left/right.
      dispH = rect.height;
      dispW = dispH * frameAspect;
      offsetX = (rect.width - dispW) / 2;
      offsetY = 0;
    } else {
      dispW = rect.width;
      dispH = dispW / frameAspect;
      offsetX = 0;
      offsetY = (rect.height - dispH) / 2;
    }
    const relX = e.clientX - rect.left - offsetX;
    const relY = e.clientY - rect.top - offsetY;
    // Scale to frame pixel coordinates.
    const x = Math.round((relX / dispW) * canvas.width);
    const y = Math.round((relY / dispH) * canvas.height);
    return { x, y };
  }

  function onPointerDown(e: PointerEvent) {
    e.preventDefault();
    canvas.setPointerCapture(e.pointerId);
    const slot = nextSlot++;
    touchSlotMap.set(e.pointerId, slot);
    const { x, y } = pointerToFrameCoords(e);
    api.sendTouch(containerId, { x, y, phase: 'down', slot }).catch((err) => {
      error = `touch down failed: ${err.message}`;
    });
  }

  function onPointerMove(e: PointerEvent) {
    if (!touchSlotMap.has(e.pointerId)) return;
    e.preventDefault();
    const slot = touchSlotMap.get(e.pointerId)!;
    const { x, y } = pointerToFrameCoords(e);
    api.sendTouch(containerId, { x, y, phase: 'move', slot }).catch((err) => {
      error = `touch move failed: ${err.message}`;
    });
  }

  function onPointerUp(e: PointerEvent) {
    if (!touchSlotMap.has(e.pointerId)) return;
    e.preventDefault();
    const slot = touchSlotMap.get(e.pointerId)!;
    const { x, y } = pointerToFrameCoords(e);
    api.sendTouch(containerId, { x, y, phase: 'up', slot }).catch((err) => {
      error = `touch up failed: ${err.message}`;
    });
    touchSlotMap.delete(e.pointerId);
    try { canvas.releasePointerCapture(e.pointerId); } catch { /* ignore */ }
  }

  function onPointerCancel(e: PointerEvent) {
    if (!touchSlotMap.has(e.pointerId)) return;
    const slot = touchSlotMap.get(e.pointerId)!;
    api.sendTouch(containerId, { x: 0, y: 0, phase: 'up', slot }).catch(() => {});
    touchSlotMap.delete(e.pointerId);
  }

  // ----- Key buttons --------------------------------------------------------

  async function tapKey(code: 'home' | 'back' | 'recent') {
    try {
      await api.sendKey(containerId, { code, down: true });
      await new Promise((r) => setTimeout(r, 50));
      await api.sendKey(containerId, { code, down: false });
    } catch (e) {
      error = `key ${code} failed: ${(e as Error).message}`;
    }
  }

  // ----- FPS / quality controls --------------------------------------------

  function setFps(v: number) {
    fps = v;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'set_fps', fps: v }));
    }
  }
  function setQuality(v: number) {
    quality = v;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'set_quality', quality: v }));
    }
  }

  onMount(() => {
    connect();
  });

  onDestroy(() => {
    destroyed = true;
    disconnect();
  });
</script>

<div class="screen-stream">
  <div class="toolbar">
    <div class="left">
      <span class="status" class:connected class:disconnected={!connected}>
        {connected ? '● Live' : '○ Disconnected'}
      </span>
      <span class="metric">{frameWidth}×{frameHeight}</span>
      <span class="metric">{measuredFps.toFixed(1)} fps</span>
      <span class="metric source" title="Frame source">{captureSource}</span>
      <span class="metric">{frameCount} frames</span>
    </div>
    <div class="right">
      <label>
        FPS
        <input type="range" min="1" max="30" bind:value={fps} on:change={() => setFps(fps)} />
        <span class="num">{fps}</span>
      </label>
      <label>
        Q
        <input type="range" min="10" max="95" bind:value={quality} on:change={() => setQuality(quality)} />
        <span class="num">{quality}</span>
      </label>
    </div>
  </div>

  <div class="frame-wrap">
    <canvas
      bind:this={canvas}
      on:pointerdown={onPointerDown}
      on:pointermove={onPointerMove}
      on:pointerup={onPointerUp}
      on:pointercancel={onPointerCancel}
      class="frame"
      style:touch-action="none"
    ></canvas>
    {#if error}
      <div class="error-banner">{error}</div>
    {/if}
  </div>

  <div class="keys">
    <button on:click={() => tapKey('back')} title="Back">◀</button>
    <button on:click={() => tapKey('home')} title="Home">●</button>
    <button on:click={() => tapKey('recent')} title="Recents">▢</button>
  </div>
</div>

<style>
  .screen-stream {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    width: 100%;
  }
  .toolbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1rem;
    padding: 0.4rem 0.6rem;
    background: #111;
    border: 1px solid #222;
    border-radius: 4px;
    font-size: 0.78rem;
    color: #aaa;
    flex-wrap: wrap;
  }
  .toolbar .left,
  .toolbar .right {
    display: flex;
    gap: 0.9rem;
    align-items: center;
  }
  .toolbar label {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
  }
  .toolbar input[type='range'] {
    width: 80px;
  }
  .toolbar .num {
    color: #fff;
    font-variant-numeric: tabular-nums;
    min-width: 2em;
    text-align: right;
  }
  .status {
    font-weight: 600;
  }
  .status.connected {
    color: #4ade80;
  }
  .status.disconnected {
    color: #f87171;
  }
  .metric {
    font-variant-numeric: tabular-nums;
  }
  .source {
    color: #93c5fd;
  }
  .frame-wrap {
    position: relative;
    width: 100%;
    background: #000;
    border-radius: 4px;
    overflow: hidden;
    aspect-ratio: 9 / 16;
    max-height: 70vh;
    margin: 0 auto;
  }
  .frame {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
    image-rendering: pixelated;
    cursor: pointer;
  }
  .error-banner {
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    background: rgba(220, 38, 38, 0.85);
    color: #fff;
    padding: 0.4rem 0.6rem;
    font-size: 0.8rem;
    text-align: center;
  }
  .keys {
    display: flex;
    gap: 0.5rem;
    justify-content: center;
  }
  .keys button {
    background: #1f2937;
    border: 1px solid #374151;
    color: #d1d5db;
    width: 48px;
    height: 36px;
    border-radius: 4px;
    font-size: 1.1rem;
    cursor: pointer;
    transition: background 0.15s;
  }
  .keys button:hover {
    background: #374151;
  }
  .keys button:active {
    background: #4b5563;
  }
</style>
