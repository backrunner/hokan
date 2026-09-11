---
title: Shell integration
description: Configure zsh, bash, and fish integration while preserving themes and other shell tools.
order: 4
keywords:
  - zsh
  - bash
  - fish
  - shell integration
---

`hokan install` detects the current shell, writes a versioned managed block near the beginning of the correct rc file, and creates a backup before changing an existing file. Re-running it is idempotent.

| Shell | Default rc file |
| --- | --- |
| zsh | `${ZDOTDIR:-$HOME}/.zshrc` |
| bash on macOS | `~/.bash_profile` |
| bash on Linux | `~/.bashrc` |
| fish | `${XDG_CONFIG_HOME:-~/.config}/fish/config.fish` |

Useful variants:

```bash
hokan install --shell zsh
hokan install --shell bash --rc-file ~/.bashrc
hokan install --shell fish --on-demand
```

`hokan setup` remains a compatibility alias for `hokan install`.

## Themes and completion plugins

Hokan passes prompt output, themes, Nerd Font glyphs, colors, and control sequences through the PTY. Completion systems that draw their own suggestions or take over the same keys can conflict with Hokan. Keep them active in ordinary shells and guard them inside Hokan:

```zsh
[[ -z $HOKAN_ACTIVE ]] && source /path/to/zsh-autosuggestions.zsh
```

For zsh themes initialized only from `.zprofile`, set `core.login_shell = true` so the managed child shell loads the same login configuration. `hokan doctor` detects this case and known conflicting plugins.
