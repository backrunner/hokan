<script lang="ts">
  import type { SvedocsTreeItem } from 'svedocs/core';
  import type { SvedocsSidebarProps } from 'svedocs/theme/types';

  let { items = [], currentPath = '' }: SvedocsSidebarProps = $props();
  const normalize = (path = '') => path.replace(/\/+$/, '') || '/';
  const active = (item: SvedocsTreeItem) => Boolean(item.path) && normalize(item.path) === normalize(currentPath);
</script>

{#snippet entries(nodes: SvedocsTreeItem[], nested = false)}
  <ul class="hk-nav-list" class:hk-nav-nested={nested}>
    {#each nodes as item (item.id)}
      <li class:hk-nav-group={Boolean(item.children?.length)}>
        {#if item.children?.length}
          {#if item.path}
            <a class="hk-nav-heading" class:hk-active={active(item)} href={item.path} aria-current={active(item) ? 'page' : undefined}>{item.title}</a>
          {:else}
            <span class="hk-nav-heading">{item.title}</span>
          {/if}
          {@render entries(item.children, true)}
        {:else if item.path}
          <a class="hk-nav-link" class:hk-active={active(item)} href={item.path} aria-current={active(item) ? 'page' : undefined}>{item.title}</a>
        {:else}
          <span class="hk-nav-heading">{item.title}</span>
        {/if}
      </li>
    {/each}
  </ul>
{/snippet}

<div class="hk-doc-nav">{@render entries(items)}</div>
