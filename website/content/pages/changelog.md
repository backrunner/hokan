---
title: Changelog
description: Release notes for Hokan 0.1.0-beta.10, including completion, terminal recovery, and self-update fixes.
---

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
