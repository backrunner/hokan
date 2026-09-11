---
title: Compatibility matrix
description: Tested environments and certification gaps for Hokan on macOS, Linux, zsh, bash, fish, tmux, and SSH.
order: 3
---

Hokan targets UTF-8 POSIX terminals on macOS and Linux with common ANSI/VT control sequences. Windows console, PowerShell, `TERM=dumb`, and non-interactive input/output are outside the interactive session scope.

The following records come from the [repository compatibility matrix](https://github.com/backrunner/hokan/blob/main/docs/compatibility.md), updated August 15, 2026. **Implemented**, **automated test coverage**, and **tested on a real environment** are different levels of evidence. A configured CI job is not a completed certification.

## Operating systems and shells

| Environment | Recorded status | Evidence or limitation |
| --- | --- | --- |
| macOS 27.0 arm64 | Tested | Local debug/release runs and real PTY harness |
| macOS x86_64 | Awaiting CI / hardware | Release cross-build configured; no hardware record for that round |
| Ubuntu/Fedora x86_64 | Awaiting CI / hardware | Linux CI jobs configured; Linux was not run on the local machine |
| Linux aarch64 | Awaiting CI / hardware | Cross-build release job configured |
| zsh 5.9 | Tested | Exact ZLE buffer/cursor synchronization and native fill-back |
| bash 3.2.57 | Tested | Standard emacs keys and Readline fill-back |
| fish 3.6+ | Awaiting certification | Adapter and fixtures implemented; custom bindings and vi mode are not certified |

## Themes, multiplexers, and transport

| Environment | Recorded status | Evidence or limitation |
| --- | --- | --- |
| Powerline-style prompt sequences | Automated coverage | Synthetic zsh fixtures with real PTY tests |
| powerlevel10k / oh-my-zsh agnoster | Awaiting certification | Real installations need terminal certification |
| oh-my-posh via login-shell configuration | Tested | Real PTY session with prompt, command execution, and exit checks |
| tmux 3.6b | Tested | Runtime probe selects the cell-diff fallback |
| tmux 3.7+ | Awaiting certification | Capability is selected by runtime probe, never guessed from version |
| OpenSSH loopback | Automated coverage | Isolated sshd and SSH PTY; actual remote hosts still need certification |

Terminal.app, iTerm2, Ghostty, Kitty, WezTerm, and Alacritty are in the release certification matrix. That does **not** mean each combination is certified. The public beta is intended to expand real-world validation.

## Expected recovery behavior

The test suite covers Unicode/CJK/emoji input, bracketed paste, terminal query handling, synchronized output, and recovery from panic and signals. Hokan hides the overlay during foreground programs, alternate-screen use, unknown VT state, or uncertain buffer synchronization and preserves byte passthrough.

Run `hokan doctor` in the actual terminal you plan to use. See [troubleshooting](/docs/troubleshooting) for themes, plugins, tmux, and restoration steps.
