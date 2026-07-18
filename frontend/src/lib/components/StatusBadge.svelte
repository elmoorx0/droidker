<script lang="ts">
  import type { ContainerStatus } from '$lib/api/api';

  export let status: ContainerStatus;

  const classMap: Record<ContainerStatus, string> = {
    running: 'badge-running',
    stopped: 'badge-stopped',
    exited: 'badge-stopped',
    created: 'badge-created',
    paused: 'badge-paused',
    creating: 'badge-other',
  };
</script>

<span class={classMap[status] || 'badge-other'}>
  {status}
</span>
