<script lang="ts">
  import { resolveLocalizedHref } from 'svedocs/theme/headless';
  import type { SvedocsBrandProps } from 'svedocs/theme/types';

  let { context }: SvedocsBrandProps = $props();
</script>

<a class="sd-brand hk-brand" href={resolveLocalizedHref(context.config.theme.brand.href, context)}>
  <img class="sd-brand-logo" src={context.config.theme.brand.logo} alt="" width="30" height="30" />
  <span>{context.config.theme.brand.label}</span>
</a>
