<script lang="ts">
  import type { ComponentProps } from 'svelte';
  import { DocsApp } from 'svedocs/theme';
  import themeComponents from 'virtual:svedocs/theme-components';
  import loadSearch from 'virtual:svedocs/search-loader';
  import Landing from './components/Landing.svelte';

  export let data: ComponentProps<typeof DocsApp>;
</script>

<DocsApp {...data} {themeComponents} {loadSearch}>
  <svelte:fragment slot="landing" let:context>
    <Landing {context} />
  </svelte:fragment>
</DocsApp>
