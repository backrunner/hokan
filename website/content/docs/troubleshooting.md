---
title: Troubleshooting
description: Diagnose shell startup, terminal rendering, completion conflicts, and recovery issues.
order: 9
keywords:
  - Hokan troubleshooting
  - terminal recovery
  - hokan doctor
---

Start with:

```bash
hokan doctor
```

The doctor checks shell selection, managed startup blocks, login-shell configuration, known completion conflicts, terminal capabilities, and provider setup.

## Prompt or theme looks different

Hokan launches the real shell in a PTY and passes prompt bytes through. If a theme loads only from `.zprofile`, enable `core.login_shell = true` and restart the shell.

## Another completion plugin takes over keys

Guard plugins that draw their own suggestions or bind the same keys:

```zsh
[[ -z $HOKAN_ACTIVE ]] && source /path/to/plugin.zsh
```

## Terminal state is not restored

Hokan treats recovery as a first-class path. Leave the session with `hokan-leave`, restart the shell, and run `hokan doctor` to capture the environment. Report a reproducible case with terminal name, shell, tmux or SSH usage, and the last command entered.

## Reset shell integration

Re-run the installer or:

```bash
hokan uninstall --integration-only
hokan install
```

## Recover terminal echo or paste behavior

After `SIGKILL` or a host terminal failure, the normal cleanup path may not run. From a shell, restore the terminal state:

```bash
stty sane
printf '\033[?2004l'
printf '\033[0m\033[?25h'
reset
```

`stty sane` restores TTY attributes. The escape sequences disable bracketed paste and restore the text style and visible cursor. `reset` resets more emulator state if the earlier steps are insufficient.

## Icons appear as squares

Set `ui.nerd_fonts = false` for text labels, or select a Nerd Font in your terminal. If borders also render incorrectly, verify the UTF-8 locale (`LANG` / `LC_CTYPE`).

## The completion list disappears

The overlay hides during foreground applications, alternate-screen use, unknown terminal state, or uncertain cursor synchronization. `Ctrl-L`, resizing, and returning from `vim` or `less` trigger a new cursor anchor. For bash/fish, try default key bindings before testing custom bindings or vi mode.

## tmux does not use synchronized output

Hokan probes terminal capabilities at runtime. tmux 3.6b uses a cell-diff fallback; this is expected. Record the exact tmux, outer terminal, and shell versions when reporting detach/attach or pane resize issues.

## Report a reproducible issue

Include the Hokan version, operating system, architecture, shell and terminal versions, whether SSH or tmux is involved, and a minimal reproduction. Remove private paths, commands, or credentials before sharing diagnostics. The [repository troubleshooting guide](https://github.com/backrunner/hokan/blob/main/docs/troubleshooting.md) has more detailed recovery notes.
