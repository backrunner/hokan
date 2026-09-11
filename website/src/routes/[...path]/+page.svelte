<script lang="ts">
  import Site from '$lib/Site.svelte';
  import type { PageData } from './$types';
  export let data: PageData;
</script>

<Site {data} />
