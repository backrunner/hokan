<script lang="ts">
  import { onDestroy } from 'svelte';

  const examples = [
    { label: 'Git', query: 'git st', prompt: '~/projects/hokan', rows: [
      { command: 'git status', detail: 'Show working tree status', source: 'spec' },
      { command: 'git stash', detail: 'Stash your local changes', source: 'spec' },
      { command: 'git status --short', detail: 'A familiar command', source: 'history' }
    ] },
    { label: 'Project', query: 'pnpm ', prompt: '~/projects/website', rows: [
      { command: 'pnpm dev', detail: 'Start the development server', source: 'project' },
      { command: 'pnpm build', detail: 'Build the production site', source: 'project' },
      { command: 'pnpm check', detail: 'Check the project', source: 'project' }
    ] },
    { label: 'History', query: 'cargo ', prompt: '~/projects/hokan', rows: [
      { command: 'cargo test --lib', detail: 'From your shell history', source: 'history' },
      { command: 'cargo check --all-targets', detail: 'From your shell history', source: 'history' },
      { command: 'cargo fmt --all -- --check', detail: 'From your shell history', source: 'history' }
    ] }
  ];
  let active = 0;
  let inserted = false;
  let selected = 0;
  let resetTimer: ReturnType<typeof setTimeout> | undefined;
  $: example = examples[active];

  function choose(index: number) {
    clearTimeout(resetTimer);
    active = index;
    selected = 0;
    inserted = false;
  }
  function insert(index: number) {
    clearTimeout(resetTimer);
    selected = index;
    inserted = true;
    resetTimer = setTimeout(() => { inserted = false; }, 4500);
  }
  onDestroy(() => clearTimeout(resetTimer));
</script>

<figure class="terminal-example">
<div class="demo">
  <div class="demo-controls">
    <span>Example</span>
    <div class="demo-tabs" role="group" aria-label="Completion examples">
      {#each examples as item, index}
        <button type="button" class:active={active === index} aria-pressed={active === index} onclick={() => choose(index)}>{item.label}</button>
      {/each}
    </div>
  </div>
  <div class="demo-body">
    <div class="prompt-context">{example.prompt}</div>
    <div class="terminal-command"><span class="prompt-chevron">❯</span><span>{inserted ? example.rows[selected].command : example.query}<span class="cursor" aria-hidden="true"></span></span></div>
    <div class="completion-box" class:dimmed={inserted}>
      {#each example.rows as row, index}
        <button type="button" class="completion-row" class:selected={index === selected} onclick={() => insert(index)} aria-label={`Insert ${row.command} in this demo`}>
          <span class="row-chevron" aria-hidden="true">{index === selected ? '›' : ' '}</span>
          <span class="row-main"><strong>{row.command}</strong><small>{row.detail}</small></span>
          <span class="source-label">{row.source}</span>
        </button>
      {/each}
      <div class="terminal-hints"><span><kbd>tab</kbd> insert</span><span><kbd>↑ ↓</kbd> select</span><span><kbd>esc</kbd> close</span></div>
    </div>
    <div class="demo-status" role="status">{inserted ? 'Inserted for review. Nothing executed.' : 'Click a suggestion to insert it here.'}</div>
  </div>
</div>
<figcaption class="demo-caption">Interactive illustration. Results depend on your shell and project.</figcaption>
</figure>
