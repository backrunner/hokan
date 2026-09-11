<script lang="ts">
  import { onDestroy } from 'svelte';
  import { resolveLocalizedHref } from 'svedocs/theme/headless';
  import type { SvedocsThemeContext } from 'svedocs/theme/types';
  import TerminalDemo from './TerminalDemo.svelte';

  export let context: SvedocsThemeContext;
  const install = 'curl --proto \'=https\' --tlsv1.2 -LsSf https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.10/hokan-installer.sh | HOKAN_VERSION=0.1.0-beta.10 sh';
  const displayInstall = install.replace(' https:', ' \\\n  https:').replace(' | ', ' \\\n  | ');
  let copyStatus = 'Copy install command';
  let timer: ReturnType<typeof setTimeout> | undefined;
  $: href = (path: string) => resolveLocalizedHref(path, context);
  async function copy() {
    clearTimeout(timer);
    try {
      await navigator.clipboard.writeText(install);
      copyStatus = 'Copied';
    } catch {
      copyStatus = 'Select the command to copy';
    }
    timer = setTimeout(() => { copyStatus = 'Copy install command'; }, 4000);
  }
  onDestroy(() => clearTimeout(timer));
</script>

<div class="hokan-landing">
  <header class="home-intro">
    <div class="intro-meta">
      <span>A command-line tool, written in Rust</span>
      <a href={href('/changelog')}>v0.1.0-beta.10 <span class="release-label">Release notes</span></a>
    </div>
    <div class="intro-body">
      <h1>Completion for<br />zsh, bash &amp; fish.</h1>
      <div class="intro-description">
        <p>Hokan adds an inline completion menu to your existing terminal. Suggestions come from your shell history, command options, files, and project scripts.</p>
        <a class="home-link" href={href('/docs')}>{context.t('home.primaryAction')} <span aria-hidden="true">→</span></a>
      </div>
    </div>
  </header>

  <section class="home-section" aria-labelledby="install-title">
    <div class="section-label">
      <h2 id="install-title">Install</h2>
      <p>macOS &amp; Linux · Public beta</p>
    </div>
    <div class="section-content">
      <div class="install-panel">
        <div class="install-heading"><span>Shell installer</span><button type="button" onclick={copy} aria-label={copyStatus}>{copyStatus === 'Copied' ? 'Copied' : 'Copy'}</button></div>
        <pre aria-label="Hokan installation command"><code>{displayInstall}</code></pre>
        <span class="sr-only" role="status">{copyStatus === 'Copy install command' ? '' : copyStatus}</span>
      </div>
      <p class="install-note">The installer sets up your shell without <code>sudo</code>. Open a new terminal, then run <code>hokan doctor</code> to check the integration.</p>
      <div class="install-links">
        <a href={href('/docs/getting-started/install')}>Installation guide</a>
        <a href={href('/docs/getting-started/install#on-demand-mode')}>On-demand mode</a>
        <a href={href('/docs/reference/compatibility')}>Compatibility</a>
      </div>
    </div>
  </section>

  <section class="home-section" aria-labelledby="terminal-title">
    <div class="section-label">
      <h2 id="terminal-title">In the terminal</h2>
      <p>Interactive example</p>
    </div>
    <div class="section-content example-layout">
      <TerminalDemo />
      <div class="example-notes">
        <p>Your real shell runs underneath Hokan. Your prompt and shell configuration still apply.</p>
        <p><kbd>Tab</kbd> inserts a suggestion for review. <kbd>Enter</kbd> runs the command.</p>
        <p>Local completion works offline. <a href={href('/docs/guides/ai')}>AI suggestions</a> are optional and requested explicitly.</p>
        <a class="home-link" href={href('/docs/concepts/completion-sources')}>Completion sources <span aria-hidden="true">→</span></a>
      </div>
    </div>
  </section>

  <section class="home-section manual-section" aria-labelledby="manual-title">
    <div class="section-label">
      <h2 id="manual-title">Documentation</h2>
      <p>Setup and reference</p>
    </div>
    <div class="section-content">
      <nav class="manual-index" aria-label="Documentation index">
        <a href={href('/docs/getting-started/first-shell')}><span>First shell</span><span>Try completions and learn the keyboard controls.</span><span aria-hidden="true">→</span></a>
        <a href={href('/docs/guides/shell-integration')}><span>Shell integration</span><span>Startup, prompts, and the PTY wrapper.</span><span aria-hidden="true">→</span></a>
        <a href={href('/docs/reference/configuration')}><span>Configuration</span><span>Set completion sources, key bindings, and AI providers.</span><span aria-hidden="true">→</span></a>
        <a href={href('/docs/reference/cli')}><span>CLI reference</span><span>Commands, flags, and diagnostics.</span><span aria-hidden="true">→</span></a>
        <a href={href('/docs/troubleshooting')}><span>Troubleshooting</span><span>Fix startup, display, and completion issues.</span><span aria-hidden="true">→</span></a>
      </nav>
      <p class="beta-note">Hokan is in public beta. Check the <a href={href('/docs/reference/compatibility')}>tested environments</a> for your shell and terminal. Bug reports are welcome on <a href="https://github.com/backrunner/hokan/issues" target="_blank" rel="noreferrer">GitHub</a>.</p>
    </div>
  </section>
</div>
