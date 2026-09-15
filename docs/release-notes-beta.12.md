# v0.1.0-beta.12

This beta hardens paste handling and shutdown reliability, expands provider
coverage, and moves the default state directory to a private location.

- Multi-megabyte pastes — large text blocks and binary/image-like payloads —
  now stream safely to both the shell prompt and foreground TUIs such as
  Codex, Kimi, and Grok-style CLIs. The PTY writer applies backpressure on
  `EAGAIN` instead of tearing down the session, and fails after ten seconds
  of zero progress so a stopped foreground child can no longer freeze input.
- zsh, bash, and fish integrations cap `BUFFER`/`START` control payloads
  below the protocol frame limit, emitting `BUFFERX`/`STARTX` markers for
  oversized state; a multi-megabyte command line can no longer stall the
  shell on the control FIFO. Buffer mirroring resumes exact sync on the next
  in-budget frame, and oversized commands fall back to the shell's own
  history records.
- Session teardown is bounded end to end: the read pump is cancelled before
  joining, a watchdog closes the output mailbox if the drain stalls, the
  output actor falls back to nonblocking stdout and restores raw mode
  directly, and a child wedged in the kernel's exit path no longer keeps
  hokan — or its shell — alive forever.
- Provider diagnostics are severity-levelled: informational notes such as
  budget cutoffs stay out of the status line while real provider failures
  still surface, and slow subprocess-backed providers (processes, network
  interfaces, toolchains) are cached across keystroke bursts.
- The git provider now offers commits from every branch, refs, and tags for
  revision slots, ranks commits before refs, and distinguishes delete
  targets from new names; in-place executable upgrades invalidate cached
  command help.
- The default state directory moves to private `~/.hokan` with atomic
  migration from `~/.local/state/hokan` (and the pre-rename `hokann` path).
  `HOKAN_STATE_DIR` and `XDG_STATE_HOME` overrides still take precedence,
  and state-dir setup failures now name the offending path.

Existing beta users whose configuration explicitly tracks stable can switch once:

```sh
hokan upgrade --channel beta --yes
```

Automatic updates run silently when a Hokan session starts, at most once per
configured interval (30 minutes by default). The next launch uses the updated
binary. Set `[update].enabled = false` or `HOKAN_NO_AUTO_UPDATE=1` to disable them.
