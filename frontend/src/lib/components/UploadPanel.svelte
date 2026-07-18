<script lang="ts">
  import { api } from '$lib/api/api';
  import { refreshContainers } from '$lib/stores/containers';

  let file: File | null = null;
  let name = '';
  let memory = 128;
  let cpu = 50;
  let notes = '';
  let portsText = ''; // free-form "host:container host:container ..."
  let uploadPct = 0;
  let uploading = false;
  let creating = false;
  let error: string | null = null;
  let successMsg: string | null = null;

  function onFileSelected(e: Event) {
    const input = e.target as HTMLInputElement;
    if (input.files && input.files[0]) {
      file = input.files[0];
      // Default the name to the APK filename (without extension).
      const stem = file.name.replace(/\.apk$/i, '');
      if (!name) name = stem.replace(/[^a-zA-Z0-9_-]/g, '-').toLowerCase();
    }
  }

  async function submit() {
    if (!file) {
      error = 'Please choose an APK file first.';
      return;
    }
    error = null;
    successMsg = null;

    // Step 1: upload
    uploading = true;
    let uploaded;
    try {
      uploaded = await api.uploadApk(file, (pct) => (uploadPct = pct));
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      uploading = false;
      return;
    }
    uploading = false;

    // Step 2: create + start
    creating = true;
    try {
      // Parse ports text into PortMapping[] — accepts forms like "8080:80 9000:9000".
      const ports = portsText
        .split(/\s+|,/)
        .map((s) => s.trim())
        .filter(Boolean)
        .map((s) => {
          const m = /^(\d+):(\d+)$/.exec(s);
          if (!m) throw new Error(`Bad port mapping: "${s}" (expected host:container, e.g. 8080:80)`);
          return { host: Number(m[1]), container: Number(m[2]) };
        });
      const created = await api.createContainer({
        name: name || undefined,
        apk: uploaded.filename,
        memory_mb: memory,
        cpu_percent: cpu,
        notes: notes || undefined,
        ports,
      });
      await api.startContainer(created.id);
      successMsg = `Container "${created.name}" created and started.`;
      await refreshContainers();
      // Reset form
      file = null;
      name = '';
      notes = '';
      portsText = '';
      uploadPct = 0;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      creating = false;
    }
  }
</script>

<div class="panel p-5">
  <h2 class="text-sm font-semibold text-slate-100 mb-4 flex items-center gap-2">
    <span class="w-2 h-2 rounded-full bg-droid-accent"></span>
    Launch New Container
  </h2>

  <form on:submit|preventDefault={submit} class="space-y-4">
    <!-- APK file picker -->
    <div>
      <label class="label" for="apk">APK File</label>
      <label
        class="block border-2 border-dashed border-droid-border rounded-lg p-4 text-center
               cursor-pointer hover:border-droid-accent/50 transition-colors"
      >
        <input
          id="apk"
          type="file"
          accept=".apk"
          class="hidden"
          on:change={onFileSelected}
        />
        {#if file}
          <div class="text-sm text-slate-200 font-mono">{file.name}</div>
          <div class="text-xs text-slate-500 mt-1">
            {(file.size / 1024 / 1024).toFixed(2)} MB
          </div>
        {:else}
          <div class="text-sm text-slate-400">Click to select an .apk file</div>
          <div class="text-xs text-slate-600 mt-1">or drag &amp; drop</div>
        {/if}
      </label>
      {#if uploading}
        <div class="mt-2">
          <div class="h-1.5 bg-droid-border rounded-full overflow-hidden">
            <div
              class="h-full bg-droid-accent transition-all"
              style="width: {uploadPct}%"
            ></div>
          </div>
          <div class="text-xs text-slate-500 mt-1">Uploading... {uploadPct}%</div>
        </div>
      {/if}
    </div>

    <!-- Name -->
    <div>
      <label class="label" for="name">Container Name (optional)</label>
      <input
        id="name"
        class="input"
        bind:value={name}
        placeholder="my-android-app"
      />
    </div>

    <!-- Resources -->
    <div class="grid grid-cols-2 gap-3">
      <div>
        <label class="label" for="mem">Memory (MB)</label>
        <input
          id="mem"
          type="number"
          min="32"
          max="1024"
          class="input"
          bind:value={memory}
        />
      </div>
      <div>
        <label class="label" for="cpu">CPU (% of one core)</label>
        <input
          id="cpu"
          type="number"
          min="1"
          max="100"
          class="input"
          bind:value={cpu}
        />
      </div>
    </div>

    <!-- Notes -->
    <div>
      <label class="label" for="notes">Notes (optional)</label>
      <textarea
        id="notes"
        class="input"
        rows="2"
        bind:value={notes}
        placeholder="What is this container for?"
      ></textarea>
    </div>

    <!-- Port publishing -->
    <div>
      <label class="label" for="ports">Publish Ports (optional)</label>
      <input
        id="ports"
        class="input font-mono"
        bind:value={portsText}
        placeholder="8080:80  9000:9000"
      />
      <div class="text-xs text-slate-500 mt-1">
        Space- or comma-separated <code>host:container</code> pairs.
        Each entry forwards host port → container port via iptables DNAT.
      </div>
    </div>

    {#if error}
      <div class="text-sm text-droid-err bg-droid-err/10 border border-droid-err/30 rounded-md p-2">
        {error}
      </div>
    {/if}
    {#if successMsg}
      <div class="text-sm text-droid-ok bg-droid-ok/10 border border-droid-ok/30 rounded-md p-2">
        {successMsg}
      </div>
    {/if}

    <button
      type="submit"
      class="btn-primary w-full justify-center"
      disabled={uploading || creating || !file}
    >
      {#if uploading}
        Uploading... {uploadPct}%
      {:else if creating}
        Starting container...
      {:else}
        Launch Container
      {/if}
    </button>
  </form>
</div>
