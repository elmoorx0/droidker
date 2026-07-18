<script lang="ts">
  const endpoints = [
    { method: 'GET',    path: '/api/v1/health',                desc: 'Liveness probe.' },
    { method: 'GET',    path: '/api/v1/ready',                 desc: 'Readiness probe + count of loaded containers.' },
    { method: 'GET',    path: '/api/v1/containers',            desc: 'List all containers (summaries).' },
    { method: 'POST',   path: '/api/v1/containers',            desc: 'Create a new container.' },
    { method: 'GET',    path: '/api/v1/containers/{id}',       desc: 'Inspect a container.' },
    { method: 'POST',   path: '/api/v1/containers/{id}/start', desc: 'Start a stopped container.' },
    { method: 'POST',   path: '/api/v1/containers/{id}/stop',  desc: 'Stop a running container.' },
    { method: 'DELETE', path: '/api/v1/containers/{id}',       desc: 'Delete a stopped container.' },
    { method: 'POST',   path: '/api/v1/upload/apk',            desc: 'Upload an APK (multipart field "file").' },
  ];

  const methodColor: Record<string, string> = {
    GET:    'text-droid-accent',
    POST:   'text-droid-ok',
    DELETE: 'text-droid-err',
    PUT:    'text-droid-warn',
  };
</script>

<div class="space-y-6">
  <h1 class="text-xl font-semibold text-slate-100">API Reference</h1>

  <div class="panel p-5">
    <h2 class="text-sm font-semibold text-slate-100 mb-3">REST endpoints</h2>
    <div class="space-y-2">
      {#each endpoints as e}
        <div class="flex items-center gap-3 font-mono text-xs">
          <span class="font-bold {methodColor[e.method]} w-16">{e.method}</span>
          <span class="text-slate-300 flex-1">{e.path}</span>
          <span class="text-slate-500 text-right">{e.desc}</span>
        </div>
      {/each}
    </div>
  </div>

  <div class="panel p-5">
    <h2 class="text-sm font-semibold text-slate-100 mb-3">CLI quickstart</h2>
    <pre class="text-xs font-mono text-slate-300 bg-droid-bg p-3 rounded-md overflow-x-auto">{`# Upload + run an APK in one step
droidker run ~/Downloads/app.apk --name my-app --memory 128 --cpu 50

# List containers
droidker ps

# Stop / start / remove
droidker stop my-app
droidker start my-app
droidker rm my-app

# Inspect a container
droidker inspect my-app`}</pre>
  </div>

  <div class="panel p-5">
    <h2 class="text-sm font-semibold text-slate-100 mb-3">Roadmap</h2>
    <ul class="text-sm text-slate-400 space-y-1 list-disc list-inside">
      <li><span class="text-droid-ok">Milestone 1</span> — Project scaffold, API, CLI, dashboard, setup.sh <span class="text-slate-600">(this release)</span></li>
      <li><span class="text-droid-warn">Milestone 2</span> — Real Linux namespace + cgroup sandbox, Android rootfs build</li>
      <li><span class="text-droid-warn">Milestone 3</span> — Per-container detail page, log streaming, exec into sandbox</li>
      <li><span class="text-droid-warn">Milestone 4</span> — WebRTC screen streaming + touch injection</li>
      <li><span class="text-droid-warn">Milestone 5</span> — Humanizer engine (Bezier swipes, Gaussian delays)</li>
      <li><span class="text-droid-warn">Milestone 6</span> — ARM→x86_64 translation layer (libhoudini / libndk)</li>
    </ul>
  </div>
</div>
