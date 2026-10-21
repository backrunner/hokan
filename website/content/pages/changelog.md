---
title: Changelog
description: Release notes for Hokan 0.1.0, including input isolation, explicit path completion, terminal recovery, and stable updates.
---

## v0.1.0

The first stable release includes the CLI help improvements from beta.17 and
strengthens input ownership and filesystem completion.

- Only known local-path arguments trigger filesystem completion. Package names,
  repository and image names, expressions, and unknown command arguments no longer
  cause directory scans merely because they contain a slash. Known file slots
  and redirections retain completion; native shell Tab remains available when
  no Hokan menu is open.
- Completion tasks, AI results, pending Tab actions, and confirmations belong to
  one prompt. Late results cannot act on a later prompt, and foreground programs
  receive their own input.
- Track split and late terminal color replies across program exit, with a
  two-second grace period, while preserving replies for active programs.
- Offer explicit spelling and parent-path repairs. Accepting a repair only fills
  the command; it never creates directories or executes the correction.
- Probe command help with the interactive shell PATH and invalidate cached help
  when PATH or the executable changes.
- Handle duplicate shell notifications without disrupting history, and keep
  recoverable protocol diagnostics in opt-in logs.
- Refresh dependencies within the Rust 1.96 compatibility boundary.

Existing beta users can move to stable with:

```sh
hokan upgrade --channel stable --yes
```

Restart Hokan after updating. Future updates follow the installed binary's
stable channel. Configuration and history are retained. Certification remains
limited to the [tested environments](/docs/reference/compatibility).

The [GitHub release](https://github.com/backrunner/hokan/releases/tag/v0.1.0)
contains macOS and Linux archives for x86_64 and aarch64, SPDX SBOMs, SHA-256
checksums, and the pinned-key Ed25519 checksum signature.

## v0.1.0-beta.10

This beta fixes recommendation and terminal interaction races, and repairs
self-updates against the actual release archives.

- Keep dismissed suggestions closed until input changes or the list is reopened.
- Preserve meaningful quoted, escaped, multiline, and Unicode whitespace in
  history and completion filtering.
- Reuse substring-search state and avoid UTF-8 decoding for ASCII fuzzy
  queries when scanning large histories, while preserving Unicode matching.
- Require confirmation when a known high-risk operation appears alongside
  syntax whose risk cannot be determined.
- Reject stale cursor reports and restore terminal input modes correctly when
  a command is queued before the initial prompt.
- Finish outstanding cursor queries before restoring terminal modes on exit,
  with a bounded wait for terminals that do not respond.
- Make release archives readable by both existing updaters and the installer.
- Stage updates beside the executable, serialize concurrent installs, preserve
  the original backup, and verify the exact reported program version.
- Default new beta installations to the beta update channel, preserve explicit
  channel settings, respect automatic-update opt-outs, and throttle failed
  background retries as well as successful checks.
- Publish releases only after all archives, checksums, and SBOMs are uploaded.
- Fix flaky terminal fixtures and update the withdrawn chacha20 dependency.

Existing beta users whose configuration explicitly tracks stable can switch once:

```sh
hokan upgrade --channel beta --yes
```

Automatic updates run silently when a Hokan session starts, at most once per
configured interval (30 minutes by default). The next launch uses the updated
binary. Set `[update].enabled = false` or `HOKAN_NO_AUTO_UPDATE=1` to disable them.

If an older updater cannot replace an installation whose cache is on a different
disk, rerun the beta installer documented in the README to obtain this fix.
