---
title: Hokan — shell-aware terminal completion
description: Hokan adds inline completion to zsh, bash, and fish using shell history, command options, files, and project scripts. Installation, guides, and reference.
layout: home
keywords:
  - terminal completion
  - shell
  - zsh
  - bash
  - fish
  - Rust CLI
---

# Completion for zsh, bash & fish

Hokan adds an inline completion menu to your existing terminal. Suggestions come from your shell history, command options, files, and project scripts. Hokan is written in Rust and available for macOS and Linux as a public beta.

## Install

The shell installer verifies the release checksum and sets up your shell without `sudo`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.10/hokan-installer.sh \
  | HOKAN_VERSION=0.1.0-beta.10 sh
```

Open a new terminal, then run `hokan doctor` to check the integration.

[Installation guide](/docs/getting-started/install) · [On-demand mode](/docs/getting-started/install#on-demand-mode) · [Compatibility](/docs/reference/compatibility)

## In the terminal

Your real shell runs underneath Hokan in a PTY. Your prompt and shell configuration still apply. `Tab` inserts a suggestion for review. `Enter` runs the command. Local completion works offline. [AI suggestions](/docs/guides/ai) are optional and requested explicitly.

[Completion sources](/docs/concepts/completion-sources)

## Documentation

- [First shell](/docs/getting-started/first-shell): try completions and learn the keyboard controls.
- [Shell integration](/docs/guides/shell-integration): startup, prompts, and the PTY wrapper.
- [Configuration](/docs/reference/configuration): set completion sources, key bindings, and AI providers.
- [CLI reference](/docs/reference/cli): commands, flags, and diagnostics.
- [Troubleshooting](/docs/troubleshooting): fix startup, display, and completion issues.

Hokan is in public beta. Check the [tested environments](/docs/reference/compatibility) for your shell and terminal. Bug reports are welcome on [GitHub](https://github.com/backrunner/hokan/issues).
