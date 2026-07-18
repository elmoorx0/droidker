// lib/api.ts
//
// Typed wrapper around the DroidKer backend REST API.
// Every fetch goes through `/api/v1/*` so the Vite dev proxy (or nginx in
// production) can forward the request to the Rust daemon.

export type ContainerStatus =
  | 'created'
  | 'running'
  | 'paused'
  | 'stopped'
  | 'exited'
  | 'creating';

export interface ContainerSummary {
  id: string;
  name: string;
  package: string;
  status: ContainerStatus;
  pid: number;
  ip: string | null;
  created_at: string;
}

export interface PortMapping {
  host: number;
  container: number;
}

export interface Container extends ContainerSummary {
  apk_sha256: string;
  memory_mb: number;
  cpu_percent: number;
  rootfs: string;
  veth_host: string | null;
  ports: PortMapping[];
  updated_at: string;
  notes: string | null;
}

export interface CreateContainerRequest {
  name?: string;
  apk: string;
  memory_mb?: number;
  cpu_percent?: number;
  notes?: string;
  ports?: PortMapping[];
}

export interface UploadResult {
  filename: string;
  sha256: string;
  size: number;
  original_name: string;
}

const BASE = '/api/v1';

async function jsonFetch<T>(url: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(url, init);
  if (!resp.ok) {
    let msg = `HTTP ${resp.status}`;
    try {
      const body = await resp.json();
      msg = body.error || msg;
    } catch {
      /* ignore */
    }
    throw new Error(msg);
  }
  if (resp.status === 204) return undefined as unknown as T;
  return resp.json() as Promise<T>;
}

export const api = {
  async health(): Promise<{ status: string }> {
    return jsonFetch(`${BASE}/health`);
  },

  async ready(): Promise<{
    ready: boolean;
    data_dir: string;
    containers_loaded: number;
  }> {
    return jsonFetch(`${BASE}/ready`);
  },

  async listContainers(): Promise<ContainerSummary[]> {
    return jsonFetch(`${BASE}/containers`);
  },

  async getContainer(id: string): Promise<Container> {
    return jsonFetch(`${BASE}/containers/${id}`);
  },

  async createContainer(body: CreateContainerRequest): Promise<Container> {
    return jsonFetch(`${BASE}/containers`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  },

  async startContainer(id: string): Promise<Container> {
    return jsonFetch(`${BASE}/containers/${id}/start`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: 'null',
    });
  },

  async stopContainer(id: string): Promise<Container> {
    return jsonFetch(`${BASE}/containers/${id}/stop`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: 'null',
    });
  },

  async deleteContainer(id: string): Promise<void> {
    return jsonFetch(`${BASE}/containers/${id}`, { method: 'DELETE' });
  },

  // ----- Screen streaming + input injection -------------------------------

  /**
   * Build the WebSocket URL for the screen stream. Caller is responsible
   * for opening the WebSocket and parsing frames (see ScreenStream.svelte).
   */
  screenWsUrl(id: string): string {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    return `${proto}://${location.host}${BASE}/containers/${id}/screen/ws`;
  },

  /**
   * Inject a touch event into the container's virtual touchscreen.
   * Coordinates are in container-screen pixels (the same coordinate space
   * as the streamed frames).
   */
  async sendTouch(
    id: string,
    ev: { x: number; y: number; phase: 'down' | 'move' | 'up'; pressure?: number; slot?: number },
  ): Promise<void> {
    return jsonFetch(`${BASE}/containers/${id}/screen/touch`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(ev),
    });
  },

  /**
   * Inject a key event (home/back/recent).
   */
  async sendKey(
    id: string,
    ev: { code: 'home' | 'back' | 'recent'; down: boolean },
  ): Promise<void> {
    return jsonFetch(`${BASE}/containers/${id}/screen/key`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(ev),
    });
  },

  async getScreenInfo(
    id: string,
  ): Promise<{
    container_id: string;
    streaming: boolean;
    input_injector_active: boolean;
    event_path: string | null;
    default_fps: number;
    default_quality: number;
    default_max_width: number;
  }> {
    return jsonFetch(`${BASE}/containers/${id}/screen/info`);
  },

  async uploadApk(file: File, onProgress?: (pct: number) => void): Promise<UploadResult> {
    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open('POST', `${BASE}/upload/apk`);

      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && onProgress) {
          onProgress(Math.round((e.loaded / e.total) * 100));
        }
      };

      xhr.onload = () => {
        if (xhr.status >= 200 && xhr.status < 300) {
          try {
            resolve(JSON.parse(xhr.responseText));
          } catch (e) {
            reject(e);
          }
        } else {
          reject(new Error(`Upload failed: HTTP ${xhr.status}`));
        }
      };

      xhr.onerror = () => reject(new Error('Network error during upload'));

      const form = new FormData();
      form.append('file', file);
      xhr.send(form);
    });
  },
};
